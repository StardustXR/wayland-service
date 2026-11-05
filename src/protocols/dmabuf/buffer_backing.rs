use crate::{CLIENT, vulkan_ctx::VK};

use super::buffer_params::BufferParams;
use drm_fourcc::DrmFourcc;
use mint::Vector2;
use stardust_xr_fusion::dmatex::{
	AlphaMode, DisjointDmatexPlane, DmatexExt, DmatexFormat, DmatexPlanes, DmatexRef, DmatexSize,
};
use std::{
	os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::io::unix::AsyncFd;
use waynest_protocols::server::stable::linux_dmabuf_v1::zwp_linux_buffer_params_v1::Flags;

/// Parameters for a shared memory buffer
#[derive(Debug)]
pub struct DmabufBacking {
	size: Vector2<u32>,
	format: DrmFourcc,
	timeline: Arc<TimelineSyncObj>,
	/// raw plane fds; NOT registered with the reactor (a dmabuf polls ready forever,
	/// which made tokio spin ~54k epoll_wait/s). Per frame we export a one-shot sync_file.
	fds: Arc<Vec<OwnedFd>>,
	dmatex: DmatexRef,
	next_acquire_point: AtomicU64,
}

impl DmabufBacking {
	pub async fn new(
		planes: Vec<DisjointDmatexPlane>,
		modifier: u64,
		size: Vector2<u32>,
		format: DrmFourcc,
	) -> Result<Self, DmatexImportError> {
		tracing::info!("Creating new DmabufBacking");
		let client = CLIENT.wait();
		let vk = VK.wait();
		let timeline = Arc::new(
			TimelineSyncObj::new(vk.render_dev.drm_node())
				.map_err(DmatexImportError::TimelineCreationError)?,
		);
		let mut last_dev_ino = None;
		let mut disjoint = false;
		for dev_ino in planes
			.iter()
			.filter_map(|p| rustix::fs::fstat(&p.dmabuf_fd).ok())
			.map(|stat| (stat.st_dev, stat.st_ino))
		{
			if let Some(last_dev_ino) = last_dev_ino {
				if last_dev_ino != dev_ino {
					disjoint = true;
					break;
				}
			}
			last_dev_ino = Some(dev_ino);
		}
		let planes = if disjoint {
			DmatexPlanes::Disjoint { planes }
		} else {
			DmatexPlanes::Simple {
				planes: planes.iter().map(|v| v.plane).collect(),
				dmabuf_fd: planes
					.into_iter()
					.next()
					.ok_or(DmatexImportError::NoPlanes)?
					.dmabuf_fd,
			}
		};
		let fds = match &planes {
			DmatexPlanes::Simple {
				dmabuf_fd,
				planes: _,
			} => vec![
				dmabuf_fd
					.try_clone()
					.map_err(DmatexImportError::DmabufFdCloneError)?,
			]
			.into(),
			DmatexPlanes::Disjoint { planes } => planes
				.iter()
				.map(|v| {
					v.dmabuf_fd
						.try_clone()
						.map_err(DmatexImportError::DmabufFdCloneError)
				})
				.collect::<Result<Vec<_>, _>>()?
				.into(),
		};
		let dmatex = DmatexRef::import(
			client,
			DmatexSize::Size2D { size },
			DmatexFormat {
				drm_fourcc: format as u32,
				drm_modifier: modifier,
				is_srgb: true,
				alpha_mode: AlphaMode::PremultipliedElectrical,
				ycbcr_info: None,
			},
			1,
			planes,
			timeline
				.export()
				.map_err(DmatexImportError::TimelineExportError)?,
		)
		.await
		.map_err(DmatexImportError::DmatexImportError)?;

		Ok(DmabufBacking {
			size,
			format,
			timeline,
			fds,
			dmatex,
			next_acquire_point: AtomicU64::new(0),
		})
	}
	#[tracing::instrument(level = "debug", skip_all)]
	pub async fn from_params(
		params: Arc<BufferParams>,
		size: Vector2<u32>,
		format: DrmFourcc,
		_flags: Flags,
	) -> Result<Self, DmatexImportError> {
		let mut planes = Vec::from_iter(std::mem::take(&mut *params.planes.lock()));
		planes.sort_by_key(|(index, _)| *index);
		let planes = planes.into_iter().map(|(_, tex)| tex).collect::<Vec<_>>();
		let modifier = *params.modifier.get().ok_or(DmatexImportError::NoModifier)?;
		Self::new(planes, modifier, size, format).await
	}

	pub fn update(&self) -> (DmatexRef, u64, u64) {
		let acquire = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
		let release = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
		let fds = self.fds.clone();
		let timeline = self.timeline.clone();
		// Opt-in fast path (WL_DMABUF_IMPORT_FENCE=1): import the sync_file into the
		// acquire timeline point with no CPU wait. Default off: on NVIDIA the exported
		// sync_file is often already signaled (weak/empty implicit fences), so import
		// makes acquire available immediately and the consumer can sample a half-written
		// host-mirror frame (periodic blink). Old semantics were: spawn, await readable,
		// then timeline.signal(acquire); the one-shot path below keeps that via the one-shot path below.
		if import_fence_enabled() && fds.len() == 1 {
			match export_sync_file(fds[0].as_fd()) {
				Ok(sync) => match timeline.import_sync_file_point(sync.as_fd(), acquire) {
					Ok(()) => return (self.dmatex.clone(), acquire, release),
					Err(e) => tracing::warn!("import dmabuf sync_file failed, cpu wait: {e}"),
				},
				Err(e) => tracing::warn!("DMA_BUF_IOCTL_EXPORT_SYNC_FILE failed, cpu wait: {e}"),
			}
		}
		// Default: one-shot sync_file per plane, awaited then dropped (deregistered).
		// Same CPU-side wait+signal as before, without a lifelong AsyncFd registration.
		tokio::spawn(async move {
			for fd in fds.iter() {
				match export_sync_file(fd.as_fd()).and_then(AsyncFd::new) {
					Ok(sync) => _ = sync.readable().await,
					Err(e) => tracing::warn!("dmabuf fence export failed, not waiting: {e}"),
				}
			}
			unsafe {
				_ = timeline.signal(acquire);
			}
		});
		(self.dmatex.clone(), acquire, release)
	}

	pub fn timeline(&self) -> Arc<TimelineSyncObj> {
		self.timeline.clone()
	}

	pub fn is_transparent(&self) -> bool {
		matches!(
			self.format,
			DrmFourcc::Abgr1555
				| DrmFourcc::Abgr16161616f
				| DrmFourcc::Abgr2101010
				| DrmFourcc::Abgr4444
				| DrmFourcc::Abgr8888
				| DrmFourcc::Argb1555
				| DrmFourcc::Argb16161616f
				| DrmFourcc::Argb2101010
				| DrmFourcc::Argb4444
				| DrmFourcc::Argb8888
				| DrmFourcc::Axbxgxrx106106106106
				| DrmFourcc::Ayuv
				| DrmFourcc::Rgba1010102
				| DrmFourcc::Rgba4444
				| DrmFourcc::Rgba5551
				| DrmFourcc::Rgba8888
		)
	}

	pub fn size(&self) -> Vector2<usize> {
		[self.size.x as usize, self.size.y as usize].into()
	}

	pub fn new_timeline_point(&self) -> u64 {
		self.next_acquire_point.fetch_add(1, Ordering::Relaxed)
	}
}
#[derive(Debug, thiserror::Error)]
pub enum DmatexImportError {
	#[error("Format modifier combination not found")]
	InvalidFormat,
	#[error("No modifier (no planes)")]
	NoModifier,
	#[error("No planes (params already used)")]
	NoPlanes,
	#[error("Failed to enumerate Server Dmatex formats: {0}")]
	FailedToEnumerateServerFormats(stardust_xr_fusion::Error),
	#[error("Failed to import Dmatex into server: {0}")]
	DmatexImportError(stardust_xr_fusion::Error),
	#[error("Failed to create TimelineSyncObj: {0}")]
	TimelineCreationError(rustix::io::Errno),
	#[error("Failed to export TimelineSyncObj: {0}")]
	TimelineExportError(rustix::io::Errno),
	#[error("Failed clone Dmabuf fd: {0}")]
	DmabufFdCloneError(std::io::Error),
	#[error("Failed to register Dmabuf fd with tokio: {0}")]
	DmabufFdRegisterError(std::io::Error),
}

/// WL_DMABUF_IMPORT_FENCE=1 enables import_sync_file_point fast path (default off).
fn import_fence_enabled() -> bool {
	static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*ENABLED.get_or_init(|| matches!(std::env::var_os("WL_DMABUF_IMPORT_FENCE").as_deref(), Some(v) if v == "1"))
}

/// DMA_BUF_IOCTL_EXPORT_SYNC_FILE with DMA_BUF_SYNC_READ: returns a sync_file that
/// signals once all pending writes (implicit fences) on the dmabuf are done.
pub(crate) fn export_sync_file(fd: BorrowedFd<'_>) -> std::io::Result<OwnedFd> {
	#[repr(C)]
	struct DmaBufExportSyncFile {
		flags: u32,
		fd: i32,
	}
	unsafe impl rustix::ioctl::Ioctl for &mut DmaBufExportSyncFile {
		type Output = ();
		const IS_MUTATING: bool = true;
		fn opcode(&self) -> rustix::ioctl::Opcode {
			rustix::ioctl::opcode::read_write::<DmaBufExportSyncFile>(b'b', 2)
		}
		fn as_ptr(&mut self) -> *mut core::ffi::c_void {
			(*self as *mut DmaBufExportSyncFile).cast()
		}
		unsafe fn output_from_ptr(
			_: rustix::ioctl::IoctlOutput,
			_: *mut core::ffi::c_void,
		) -> rustix::io::Result<()> {
			Ok(())
		}
	}
	const DMA_BUF_SYNC_READ: u32 = 1;
	let mut arg = DmaBufExportSyncFile { flags: DMA_BUF_SYNC_READ, fd: -1 };
	unsafe { rustix::ioctl::ioctl(fd, &mut arg) }.map_err(std::io::Error::from)?;
	Ok(unsafe { OwnedFd::from_raw_fd(arg.fd) })
}

#[cfg(test)]
mod tests {
	#[test]
	fn export_sync_file_opcode_matches_kernel() {
		// include/uapi/linux/dma-buf.h: _IOWR('b', 2, struct dma_buf_export_sync_file) = 0xc0086202
		#[repr(C)]
		struct S { _f: u32, _d: i32 }
		assert_eq!(rustix::ioctl::opcode::read_write::<S>(b'b', 2) as u64, 0xc008_6202);
	}
}
