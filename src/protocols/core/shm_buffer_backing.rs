use crate::{
	CLIENT,
	vulkan_ctx::{VK, VkContext},
};

use super::shm_pool::ShmPool;
use mint::Vector2;
use stardust_xr_cme::dmatex::Dmatex;
use stardust_xr_cme::format::DmatexFormat;
use stardust_xr_fusion::dmatex::{AlphaMode, DmatexRef, DmatexSize};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use thiserror::Error;
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::sync::mpsc;
use vulkano::buffer::{AllocateBufferError, Buffer, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{
	AutoCommandBufferBuilder, CommandBufferSubmitInfo, CommandBufferUsage, CopyBufferToImageInfo,
	SemaphoreSubmitInfo, SubmitInfo,
};
use vulkano::format::Format as VkFormat;
use vulkano::image::ImageUsage;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::sync::semaphore::{
	ExternalSemaphoreHandleType, ExternalSemaphoreHandleTypes, Semaphore, SemaphoreCreateInfo,
};
use vulkano::{DeviceSize, Validated};
use waynest_protocols::server::core::wayland::wl_shm::Format;

#[derive(Debug)]
/// Parameters for a shared memory buffer
pub struct ShmBufferBacking {
	pool: Arc<ShmPool>,
	offset: usize,
	stride: usize,
	size: Vector2<u64>,
	wl_format: Format,
	dmatex: Arc<Dmatex>,
	/// `None` when the dmatex is host-linear and written directly by the CPU
	staging_buffer: Option<Subbuffer<[u8]>>,
	next_acquire_point: AtomicU64,
	timeline_copy: Arc<TimelineSyncObj>,
}

impl ShmBufferBacking {
	pub async fn new(
		pool: Arc<ShmPool>,
		offset: usize,
		stride: usize,
		size: Vector2<u64>,
		wl_format: Format,
	) -> Result<Self, ShmBackingCreationError> {
		let client = CLIENT.wait();
		let vk = VK.wait();
		let texture_format = match wl_format {
			Format::Argb8888 | Format::Xrgb8888 => VkFormat::B8G8R8A8_SRGB,
			_ => return Err(ShmBackingCreationError::UnsupportedFormat),
		};
		// let formats =
		let format = DmatexFormat::enumerate(client, &vk.render_dev)
			.await
			.map_err(ShmBackingCreationError::DmatexFormatEnumerationFailed)?
			.get(&texture_format)
			.cloned()
			.ok_or(ShmBackingCreationError::FormatNotSupportedByDmatex)?;
		// Copying shm into a host-visible LINEAR image on the CPU needs no queue submits.
		// NVIDIA 615.78 loses the device (Xid 32) on our transfer submits once a dmatex
		// exists, so this is the default; SDXR_SHM_GPU_UPLOAD=1 forces the old path.
		let host_linear = if *SHM_GPU_UPLOAD {
			None
		} else {
			match Dmatex::new_host_linear(
				client,
				&vk.dev,
				&vk.render_dev,
				[size.x as u32, size.y as u32],
				&format,
				AlphaMode::PremultipliedElectrical,
			)
			.await
			{
				Ok(dmatex) => Some(dmatex),
				Err(e) => {
					tracing::warn!("host-linear shm dmatex unavailable, using GPU upload: {e}");
					None
				}
			}
		};
		let (dmatex, staging_buffer) = match host_linear {
			Some(dmatex) => (Arc::new(dmatex), None),
			None => {
				let dmatex = Arc::new(
					Dmatex::new(
						client,
						&vk.dev,
						&vk.render_dev,
						DmatexSize::Size2D {
							size: [size.x as u32, size.y as u32].into(),
						},
						&format,
						None,
						AlphaMode::PremultipliedElectrical,
						ImageUsage::TRANSFER_DST,
					)
					.await,
				);

				let staging_buffer = Buffer::new_slice::<u8>(
					vk.mem_alloc.clone(),
					BufferCreateInfo {
						usage: BufferUsage::TRANSFER_SRC,
						..Default::default()
					},
					AllocationCreateInfo {
						memory_type_filter: MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
						..Default::default()
					},
					// when supporting more formats we need to change this 4
					size.x * size.y * 4 as DeviceSize,
				)
				.map_err(ShmBackingCreationError::StagingBufferAllocationFailed)?;
				(dmatex, Some(staging_buffer))
			}
		};
		let timeline_copy = Arc::new(
			TimelineSyncObj::import(
				vk.render_dev.drm_node(),
				dmatex
					.timeline
					.export()
					.map_err(ShmBackingCreationError::TimelineCopyFailed)?
					.as_fd(),
			)
			.map_err(ShmBackingCreationError::TimelineCopyFailed)?,
		);
		Ok(Self {
			pool,
			offset,
			stride,
			size,
			wl_format,
			dmatex,
			staging_buffer,
			timeline_copy,
			next_acquire_point: AtomicU64::new(0),
		})
	}

	pub fn update(&self) -> (DmatexRef, u64, u64) {
		let acquire = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
		let release = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
		let Some(staging_buffer) = &self.staging_buffer else {
			self.update_host_linear(acquire);
			return (self.dmatex.dmatex.clone(), acquire, release);
		};
		// TODO: move this to a blocking thread
		match staging_buffer.write() {
			Ok(mut writer) => {
				let shm_data = self.pool.data_lock();
				for y in 0..self.size.y {
					let shm_offset = self.offset + (y as usize * self.stride);
					let gpu_offset = (y * self.size.x * 4) as usize;
					let line_len = (self.size.x * 4) as usize;

					let (Some(dst), Some(src)) = (
						writer.get_mut(gpu_offset..(gpu_offset + line_len)),
						shm_data.get(shm_offset..(shm_offset + line_len)),
					) else {
						tracing::error!("shm buffer reaches outside its pool");
						break;
					};
					dst.copy_from_slice(src);
				}
			}
			Err(e) => tracing::error!("failed to write shm staging buffer: {e}"),
		}
		// still sent on a failed copy so the acquire point gets signaled
		if UPLOAD_QUEUE
			.send(DmatexUpload {
				tex: self.dmatex.clone(),
				staging: staging_buffer.clone(),
				acquire,
			})
			.is_err()
		{
			tracing::error!("dmatex upload thread is gone");
		}
		// self.staging_buffer.
		(self.dmatex.dmatex.clone(), acquire, release)
	}

	fn update_host_linear(&self, acquire: u64) {
		let Some(map) = self.dmatex.host_map else {
			unreachable!("shm backings without a staging buffer are host-linear");
		};
		{
			let shm_data = self.pool.data_lock();
			let line_len = (self.size.x * 4) as usize;
			for y in 0..self.size.y as usize {
				let shm_offset = self.offset + y * self.stride;
				let Some(src) = shm_data.get(shm_offset..shm_offset + line_len) else {
					tracing::error!("shm buffer reaches outside its pool");
					break;
				};
				// SAFETY: self.dmatex owns the mapping and only this backing writes it.
				// The server may sample mid-copy, which can tear but is otherwise fine.
				if !unsafe { map.write_row(y, src) } {
					tracing::error!("shm row {y} doesn't fit the host-linear dmatex");
					break;
				}
			}
		}
		// SAFETY: the CPU copy above is the only work this acquire point waits on
		if let Err(e) = unsafe { self.dmatex.timeline.signal(acquire) } {
			tracing::error!("failed to signal host-linear acquire point: {e}");
		}
	}

	/// Host-linear updates copy synchronously, so the wl_buffer can be released right away
	pub fn is_host_linear(&self) -> bool {
		self.staging_buffer.is_none()
	}

	pub fn timeline(&self) -> Arc<TimelineSyncObj> {
		self.timeline_copy.clone()
	}

	pub fn is_transparent(&self) -> bool {
		match self.wl_format {
			Format::Xrgb8888 => false,
			Format::Argb8888 => true,
			_ => true,
		}
	}

	pub fn size(&self) -> Vector2<usize> {
		[self.size.x as usize, self.size.y as usize].into()
	}

	pub fn new_timeline_point(&self) -> u64 {
		self.next_acquire_point.fetch_add(1, Ordering::Relaxed)
	}
}

#[derive(Debug, Error)]
pub enum ShmBackingCreationError {
	#[error("Format not supported")]
	UnsupportedFormat,
	#[error("Dmatex format enumeration failed: {0}")]
	DmatexFormatEnumerationFailed(stardust_xr_fusion::Error),
	#[error("Format not supported by Dmatex")]
	FormatNotSupportedByDmatex,
	#[error("Staging buffer allocation failed: {0}")]
	StagingBufferAllocationFailed(Validated<AllocateBufferError>),
	#[error("Timeline copy failed: {0}")]
	TimelineCopyFailed(rustix::io::Errno),
}

struct DmatexUpload {
	tex: Arc<Dmatex>,
	staging: Subbuffer<[u8]>,
	acquire: u64,
}
static UPLOAD_QUEUE: LazyLock<mpsc::UnboundedSender<DmatexUpload>> = LazyLock::new(|| {
	let (tx, rx) = mpsc::unbounded_channel();
	let runtime = tokio::runtime::Handle::current();
	std::thread::spawn(move || {
		let _guard = runtime.enter();
		dmatex_upload_task(rx)
	});
	tx
});
fn dmatex_upload_task(mut receiver: mpsc::UnboundedReceiver<DmatexUpload>) {
	let mut uploads = Vec::new();
	let vk = VK.wait();
	loop {
		let n = receiver.blocking_recv_many(&mut uploads, 16);
		if n == 0 {
			tracing::error!("dmatex upload channel somehow closed");
			break;
		}
		let semaphores = match submit_uploads(vk, &uploads) {
			Ok(semaphores) => semaphores,
			Err(e) => {
				tracing::error!("failed to submit dmatex uploads: {e:#}");
				for upload in uploads.drain(..) {
					signal_acquire(&upload);
				}
				continue;
			}
		};
		for (upload, semaphore) in uploads.drain(..).zip(semaphores.into_iter()) {
			tokio::spawn(async move {
				let fd = match unsafe { semaphore.export_fd(ExternalSemaphoreHandleType::SyncFd) } {
					Ok(fd) => fd,
					Err(e) => {
						tracing::error!("failed to export upload semaphore: {e}");
						signal_acquire(&upload);
						return;
					}
				};
				if let Err(e) = upload
					.tex
					.timeline
					.import_sync_file_point(fd.as_fd(), upload.acquire)
				{
					tracing::error!("failed to import upload sync file: {e}");
					signal_acquire(&upload);
					return;
				}
				match upload.tex.timeline.wait_async(upload.acquire) {
					Ok(wait) => wait.await,
					Err(e) => tracing::error!("failed to wait for upload: {e}"),
				}
			});
		}
	}
}

static SHM_GPU_UPLOAD: LazyLock<bool> =
	LazyLock::new(|| std::env::var_os("SDXR_SHM_GPU_UPLOAD").is_some_and(|v| v == "1"));

fn submit_uploads(vk: &VkContext, uploads: &[DmatexUpload]) -> anyhow::Result<Vec<Arc<Semaphore>>> {
	let mut cmd_buf = AutoCommandBufferBuilder::primary(
		vk.cballoc.clone(),
		vk.queue.queue_family_index(),
		CommandBufferUsage::OneTimeSubmit,
	)?;
	let mut semaphores = Vec::with_capacity(uploads.len());
	for upload in uploads.iter() {
		cmd_buf.copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(
			upload.staging.clone(),
			upload.tex.image.clone(),
		))?;
		semaphores.push(Arc::new(Semaphore::new(
			vk.dev.clone(),
			SemaphoreCreateInfo {
				export_handle_types: ExternalSemaphoreHandleTypes::SYNC_FD,
				..Default::default()
			},
		)?));
	}
	let buf = cmd_buf.build()?;
	vk.queue.with(|mut queue| unsafe {
		queue.submit(
			&[SubmitInfo {
				command_buffers: vec![CommandBufferSubmitInfo::new(buf)],
				signal_semaphores: semaphores
					.iter()
					.cloned()
					.map(SemaphoreSubmitInfo::new)
					.collect(),
				..Default::default()
			}],
			None,
		)
	})?;
	Ok(semaphores)
}

/// shows a stale frame instead of leaving the panel waiting on this point forever
fn signal_acquire(upload: &DmatexUpload) {
	if let Err(e) = unsafe { upload.tex.timeline.signal(upload.acquire) } {
		tracing::error!("failed to signal upload acquire point: {e}");
	}
}
