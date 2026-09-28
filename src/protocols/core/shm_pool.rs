// use super::shm_buffer_backing::ShmBufferBacking;
use crate::{
	client::Client,
	error::{WaylandError, WaylandResult},
	protocols::core::{
		buffer::{Buffer, BufferBacking},
		shm_buffer_backing::ShmBufferBacking,
	},
};
use memmap2::{MmapOptions, RemapOptions};
use parking_lot::{Mutex, MutexGuard, RawMutex, lock_api::MappedMutexGuard};
use std::os::fd::{AsRawFd, OwnedFd};
use waynest::ObjectId;
use waynest_protocols::server::core::wayland::wl_shm::{Error as ShmError, Format};
pub use waynest_protocols::server::core::wayland::wl_shm_pool::*;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct ShmPool {
	inner: Mutex<memmap2::MmapMut>,
	id: ObjectId,
}

impl ShmPool {
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn new(fd: OwnedFd, size: i32, id: ObjectId) -> WaylandResult<Self> {
		let map = unsafe {
			MmapOptions::new()
				.len(size as usize)
				.map_mut(fd.as_raw_fd())?
		};

		Ok(Self {
			inner: Mutex::new(map),
			id,
		})
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn data_lock(&self) -> MappedMutexGuard<'_, RawMutex, [u8]> {
		MutexGuard::map(self.inner.lock(), |i| i.as_mut())
	}
}

impl WlShmPool for ShmPool {
	type Connection = Client;

	/// https://wayland.app/protocols/wayland#wl_shm_pool:request:create_buffer
	#[tracing::instrument(level = "debug", skip_all)]
	async fn create_buffer(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
		id: ObjectId,
		offset: i32,
		width: i32,
		height: i32,
		stride: i32,
		format: Format,
	) -> WaylandResult<()> {
		let pool = client.try_get::<ShmPool>(sender_id)?;
		let (w, h, o, st) = (width as i64, height as i64, offset as i64, stride as i64);
		if w <= 0 || h <= 0 || o < 0 || st < w * 4 || o + st * h > pool.data_lock().len() as i64 {
			return Err(WaylandError::Fatal {
				object_id: sender_id,
				code: ShmError::InvalidStride.into(),
				message: "buffer doesn't fit in the shm pool",
			});
		}
		let params = ShmBufferBacking::new(
			pool,
			offset as usize,
			stride as usize,
			[width as u32, height as u32].into(),
			format,
		)
		.map_err(|e| {
			tracing::error!("failed to create shm buffer: {e}");
			WaylandError::Fatal {
				object_id: sender_id,
				code: ShmError::InvalidFormat.into(),
				message: "failed to create shm buffer",
			}
		})?;

		Buffer::new(client, id, BufferBacking::Shm(params))?;
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_shm_pool:request:resize
	#[tracing::instrument(level = "debug", skip_all)]
	async fn resize(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		size: i32,
	) -> WaylandResult<()> {
		let mut inner = self.inner.lock();
		if size < 0 || (size as usize) < inner.len() {
			tracing::error!("client tried to shrink shm pool, ignoring");
			return Ok(());
		}
		unsafe { inner.remap(size as usize, RemapOptions::new().may_move(true))? };
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_shm_pool:request:destroy
	#[tracing::instrument(level = "debug", skip_all)]
	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}
