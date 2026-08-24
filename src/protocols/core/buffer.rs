use crate::client::{Client, MessageSink};
use crate::error::WaylandResult;
use crate::protocols::core::shm_buffer_backing::ShmBufferBacking;
use crate::protocols::dmabuf::buffer_backing::DmabufBacking;
use crate::signal_on_drop::SignalOnDrop;
use crate::util::AbortOnDrop;

use mint::Vector2;
use stardust_xr_fusion::dmatex::{DmatexRef, DmatexSubmitRelease, DmatexSubmitReleaseLocal};
use std::sync::Arc;
use std::time::Duration;
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::task::AbortHandle;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_buffer::*;
use waynest_server::{Client as _, RequestDispatcher};

#[derive(Debug)]
pub enum BufferBacking {
	Shm(ShmBufferBacking),
	Dmabuf(DmabufBacking),
}

#[derive(Debug, RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Buffer {
	pub id: ObjectId,
	backing: BufferBacking,
	message_sink: MessageSink,
}

impl Buffer {
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn new(
		client: &mut Client,
		id: ObjectId,
		backing: BufferBacking,
	) -> WaylandResult<Arc<Self>> {
		Ok(client.insert(
			id,
			Self {
				id,
				backing,
				message_sink: client.message_sink(),
			},
		)?)
	}

	/// returns (dmatex_uid, server_acquire_point, server_release_point)
	pub fn update(self: &Arc<Self>) -> BufferSubmit {
		let (dmatex, acquire, release) = match &self.backing {
			BufferBacking::Dmabuf(backing) => backing.update(),
			BufferBacking::Shm(backing) => backing.update(),
		};
		let timeline = match &self.backing {
			BufferBacking::Dmabuf(backing) => backing.timeline(),
			BufferBacking::Shm(backing) => backing.timeline(),
		};
		let release_task = self.release_task(timeline.clone(), release);
		BufferSubmit {
			dmatex,
			acquire,
			release: SignalOnDrop::new(timeline, release),
			release_task,
			buffer: self.clone(),
		}
	}

	pub fn is_transparent(&self) -> bool {
		match &self.backing {
			BufferBacking::Shm(backing) => backing.is_transparent(),
			BufferBacking::Dmabuf(backing) => backing.is_transparent(),
		}
	}

	pub fn size(&self) -> Vector2<usize> {
		match &self.backing {
			BufferBacking::Shm(backing) => backing.size(),
			BufferBacking::Dmabuf(backing) => backing.size(),
		}
	}
	fn new_timeline_point(&self) -> u64 {
		match &self.backing {
			BufferBacking::Shm(backing) => backing.new_timeline_point(),
			BufferBacking::Dmabuf(backing) => backing.new_timeline_point(),
		}
	}
	fn release_task(self: &Arc<Self>, timeline: Arc<TimelineSyncObj>, release: u64) -> AbortHandle {
		tokio::spawn({
			let message_sink = self.message_sink.clone();
			let buffer = self.clone();
			async move {
				let _task: AbortOnDrop = tokio::spawn(async {
					tokio::time::sleep(Duration::from_millis(500)).await;
					tracing::warn!("buffer not released for 500ms");
				})
				.into();
				timeline.wait_async(release).unwrap().await;
				tracing::trace!("sending buffer release");
				message_sink.send(crate::client::Message::ReleaseBuffer(buffer))
			}
		})
		.abort_handle()
	}
}
pub struct BufferSubmit {
	dmatex: DmatexRef,
	acquire: u64,
	release: DmatexSubmitReleaseLocal<SignalOnDrop>,
	// this is explicitly not an AbortOnDrop, we only want to abort it in some rare cases
	release_task: AbortHandle,
	buffer: Arc<Buffer>,
}
impl BufferSubmit {
	pub fn reapply(self) -> BufferSubmit {
		let new_release = self.buffer.new_timeline_point();
		self.release_task.abort();
		let release_task = self
			.buffer
			.release_task(self.release.timeline().clone(), new_release);
		let release = SignalOnDrop::new(self.release.timeline().clone(), new_release);
		BufferSubmit {
			dmatex: self.dmatex,
			acquire: self.acquire,
			release,
			release_task,
			buffer: self.buffer,
		}
	}
	pub fn dmatex(&self) -> DmatexRef {
		self.dmatex.clone()
	}
	pub fn acquire(&self) -> u64 {
		self.acquire
	}
	pub fn release(&self) -> DmatexSubmitRelease {
		self.release.proxy().clone()
	}
	pub fn buffer(&self) -> &Arc<Buffer> {
		&self.buffer
	}
}

impl WlBuffer for Buffer {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_buffer:request:destroy
	async fn destroy(&self, client: &mut Client, _sender_id: ObjectId) -> WaylandResult<()> {
		client.remove(self.id);
		tracing::info!("Destroying buffer {:?}", self.id);
		Ok(())
	}
}
