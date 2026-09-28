use crate::{
	CLIENT,
	vulkan_ctx::{VK, VkContext},
};

use super::shm_pool::ShmPool;
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_cme::dmatex::Dmatex;
use stardust_xr_cme::format::DmatexFormat;
use stardust_xr_fusion::dmatex::{AlphaMode, DmatexSize};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use thiserror::Error;
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::sync::mpsc;
use vulkano::buffer::{AllocateBufferError, Buffer, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{
	AutoCommandBufferBuilder, BufferImageCopy, CommandBufferSubmitInfo, CommandBufferUsage,
	CopyBufferToImageInfo, SemaphoreSubmitInfo, SubmitInfo,
};
use vulkano::format::Format as VkFormat;
use vulkano::image::ImageUsage;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::sync::semaphore::{
	ExternalSemaphoreHandleType, ExternalSemaphoreHandleTypes, Semaphore, SemaphoreCreateInfo,
};
use vulkano::{DeviceSize, Validated};
use waynest_protocols::server::core::wayland::wl_shm::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
	pub x: u32,
	pub y: u32,
	pub w: u32,
	pub h: u32,
}
impl Rect {
	pub fn clamped(x: i32, y: i32, w: i32, h: i32, size: Vector2<u32>) -> Option<Self> {
		let x0 = x.clamp(0, size.x as i32) as u32;
		let y0 = y.clamp(0, size.y as i32) as u32;
		let x1 = x.saturating_add(w).clamp(0, size.x as i32) as u32;
		let y1 = y.saturating_add(h).clamp(0, size.y as i32) as u32;
		(x1 > x0 && y1 > y0).then_some(Self {
			x: x0,
			y: y0,
			w: x1 - x0,
			h: y1 - y0,
		})
	}
	fn full(size: Vector2<u32>) -> Self {
		Self {
			x: 0,
			y: 0,
			w: size.x,
			h: size.y,
		}
	}
	fn union(self, o: Self) -> Self {
		let x = self.x.min(o.x);
		let y = self.y.min(o.y);
		Self {
			x,
			y,
			w: (self.x + self.w).max(o.x + o.w) - x,
			h: (self.y + self.h).max(o.y + o.h) - y,
		}
	}
}

/// only a view into the pool, the pixels get copied out on commit so the buffer can be
/// released right away
#[derive(Debug)]
pub struct ShmBufferBacking {
	pool: Arc<ShmPool>,
	offset: usize,
	stride: usize,
	size: Vector2<u32>,
	wl_format: Format,
}
impl ShmBufferBacking {
	pub fn new(
		pool: Arc<ShmPool>,
		offset: usize,
		stride: usize,
		size: Vector2<u32>,
		wl_format: Format,
	) -> Result<Self, ShmBackingCreationError> {
		if !matches!(wl_format, Format::Argb8888 | Format::Xrgb8888) {
			return Err(ShmBackingCreationError::UnsupportedFormat);
		}
		Ok(Self {
			pool,
			offset,
			stride,
			size,
			wl_format,
		})
	}

	/// dst is tightly packed at our size
	fn copy_rects(&self, dst: &mut [u8], rects: &[Rect]) {
		let shm = self.pool.data_lock();
		for r in rects {
			for y in r.y..r.y + r.h {
				let src = self.offset + y as usize * self.stride + r.x as usize * 4;
				let d = (y * self.size.x + r.x) as usize * 4;
				let len = r.w as usize * 4;
				let (Some(dst), Some(src)) = (dst.get_mut(d..d + len), shm.get(src..src + len))
				else {
					tracing::error!("shm buffer reaches outside its pool");
					return;
				};
				dst.copy_from_slice(src);
			}
		}
	}

	pub fn is_transparent(&self) -> bool {
		self.wl_format == Format::Argb8888
	}

	pub fn size(&self) -> Vector2<usize> {
		[self.size.x as usize, self.size.y as usize].into()
	}
}

/// the gpu side of a surface's shm buffers, recreated on resize
///
/// two textures so we never write the one the server is still showing, the server only lets
/// go of a texture once a newer one is ready, so a single texture would wait on itself
#[derive(Debug)]
pub struct ShmTextures {
	size: Vector2<u32>,
	/// the latest frame, the textures catch up to it from here
	shadow: Arc<Mutex<Vec<u8>>>,
	textures: [Arc<ShmTexture>; 2],
	next: AtomicUsize,
	fresh: AtomicBool,
}
impl ShmTextures {
	pub async fn new(size: Vector2<u32>) -> Result<Self, ShmBackingCreationError> {
		let a = ShmTexture::new(size).await?;
		let b = ShmTexture::new(size).await?;
		Ok(Self {
			size,
			shadow: Arc::new(Mutex::new(vec![0; size.x as usize * size.y as usize * 4])),
			textures: [Arc::new(a), Arc::new(b)],
			next: AtomicUsize::new(0),
			fresh: AtomicBool::new(true),
		})
	}

	pub fn size(&self) -> Vector2<u32> {
		self.size
	}

	/// returns (texture, server_acquire_point, server_release_point)
	pub fn update(
		&self,
		buffer: &ShmBufferBacking,
		damage: &[Rect],
	) -> Option<(Arc<ShmTexture>, u64, u64)> {
		if buffer.size != self.size {
			tracing::warn!("shm buffer doesn't match the surface textures, skipping");
			return None;
		}
		let full = [Rect::full(self.size)];
		let damage = if self.fresh.swap(false, Ordering::Relaxed) {
			&full[..]
		} else {
			damage
		};
		{
			let mut shadow = self.shadow.lock();
			buffer.copy_rects(&mut shadow, damage);
			for tex in &self.textures {
				tex.add_missing(damage);
			}
		}
		let tex = self.textures[self.next.fetch_add(1, Ordering::Relaxed) % 2].clone();
		let acquire = tex.new_timeline_point();
		let release = tex.new_timeline_point();
		tokio::spawn({
			let tex = tex.clone();
			let shadow = self.shadow.clone();
			async move {
				// the point before our acquire is always this texture's last release
				if let Some(prev_release) = acquire.checked_sub(1) {
					match tex.dmatex.timeline.wait_async(prev_release) {
						Ok(wait) => wait.await,
						Err(e) => tracing::error!("failed to wait for shm texture release: {e}"),
					}
				}
				let regions = {
					let shadow = shadow.lock();
					let regions = std::mem::take(&mut *tex.missing.lock());
					match tex.staging.write() {
						Ok(mut staging) => {
							let w = tex.width as usize;
							for r in &regions {
								for y in r.y as usize..(r.y + r.h) as usize {
									let o = (y * w + r.x as usize) * 4;
									let len = r.w as usize * 4;
									staging[o..o + len].copy_from_slice(&shadow[o..o + len]);
								}
							}
							regions
						}
						Err(e) => {
							tracing::error!("failed to write shm staging buffer: {e}");
							Vec::new()
						}
					}
				};
				DmatexUpload {
					tex: tex.clone(),
					regions,
					acquire,
				}
				.queue();
			}
		});
		Some((tex, acquire, release))
	}
}

#[derive(Debug)]
pub struct ShmTexture {
	pub dmatex: Arc<Dmatex>,
	/// render node import of the dmatex timeline, for release signaling
	pub timeline: Arc<TimelineSyncObj>,
	staging: Subbuffer<[u8]>,
	width: u32,
	missing: Mutex<Vec<Rect>>,
	next_point: AtomicU64,
}
impl ShmTexture {
	async fn new(size: Vector2<u32>) -> Result<Self, ShmBackingCreationError> {
		let client = CLIENT.wait();
		let vk = VK.wait();
		let format = DmatexFormat::enumerate(client, &vk.render_dev)
			.await
			.map_err(ShmBackingCreationError::DmatexFormatEnumerationFailed)?
			.get(&VkFormat::B8G8R8A8_SRGB)
			.cloned()
			.ok_or(ShmBackingCreationError::FormatNotSupportedByDmatex)?;
		let dmatex = Arc::new(
			Dmatex::new(
				client,
				&vk.dev,
				&vk.render_dev,
				DmatexSize::Size2D { size },
				&format,
				None,
				AlphaMode::PremultipliedElectrical,
				ImageUsage::TRANSFER_DST,
			)
			.await,
		);
		let staging = Buffer::new_slice::<u8>(
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
			size.x as DeviceSize * size.y as DeviceSize * 4,
		)
		.map_err(ShmBackingCreationError::StagingBufferAllocationFailed)?;
		let timeline = Arc::new(
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
			dmatex,
			timeline,
			staging,
			width: size.x,
			missing: Mutex::new(vec![Rect::full(size)]),
			next_point: AtomicU64::new(0),
		})
	}

	fn add_missing(&self, damage: &[Rect]) {
		let mut missing = self.missing.lock();
		missing.extend_from_slice(damage);
		if missing.len() > 16 {
			let bbox = missing.iter().copied().reduce(Rect::union);
			missing.clear();
			missing.extend(bbox);
		}
	}

	pub fn new_timeline_point(&self) -> u64 {
		self.next_point.fetch_add(1, Ordering::Relaxed)
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
	tex: Arc<ShmTexture>,
	regions: Vec<Rect>,
	acquire: u64,
}
impl DmatexUpload {
	fn queue(self) {
		if UPLOAD_QUEUE.send(self).is_err() {
			tracing::error!("dmatex upload thread is gone");
		}
	}
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
		// nothing to copy, the server still needs the acquire point though
		uploads.retain(|upload| {
			if upload.regions.is_empty() {
				signal_acquire(upload);
			}
			!upload.regions.is_empty()
		});
		if uploads.is_empty() {
			continue;
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
				let timeline = &upload.tex.dmatex.timeline;
				if let Err(e) = timeline.import_sync_file_point(fd.as_fd(), upload.acquire) {
					tracing::error!("failed to import upload sync file: {e}");
					signal_acquire(&upload);
					return;
				}
				match timeline.wait_async(upload.acquire) {
					Ok(wait) => wait.await,
					Err(e) => tracing::error!("failed to wait for upload: {e}"),
				}
			});
		}
	}
}

fn submit_uploads(vk: &VkContext, uploads: &[DmatexUpload]) -> anyhow::Result<Vec<Arc<Semaphore>>> {
	let mut cmd_buf = AutoCommandBufferBuilder::primary(
		vk.cballoc.clone(),
		vk.queue.queue_family_index(),
		CommandBufferUsage::OneTimeSubmit,
	)?;
	let mut semaphores = Vec::with_capacity(uploads.len());
	for upload in uploads.iter() {
		let image = upload.tex.dmatex.image.clone();
		let w = upload.tex.width;
		cmd_buf.copy_buffer_to_image(CopyBufferToImageInfo {
			regions: upload
				.regions
				.iter()
				.map(|r| BufferImageCopy {
					buffer_offset: (r.y as DeviceSize * w as DeviceSize + r.x as DeviceSize) * 4,
					buffer_row_length: w,
					image_subresource: image.subresource_layers(),
					image_offset: [r.x, r.y, 0],
					image_extent: [r.w, r.h, 1],
					..Default::default()
				})
				.collect(),
			..CopyBufferToImageInfo::buffer_image(upload.tex.staging.clone(), image.clone())
		})?;
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
	if let Err(e) = unsafe { upload.tex.dmatex.timeline.signal(upload.acquire) } {
		tracing::error!("failed to signal upload acquire point: {e}");
	}
}
