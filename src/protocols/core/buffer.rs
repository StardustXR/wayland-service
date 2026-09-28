use crate::client::{Client, MessageSink};
use crate::error::WaylandResult;
use crate::protocols::core::shm_buffer_backing::{Rect, ShmBufferBacking, ShmTexture, ShmTextures};
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
				message_sink: client.message_sink()?,
			},
		)?)
	}

	pub fn update(self: &Arc<Self>, shm: Option<&ShmTextures>, damage: &[Rect]) -> Option<BufferSubmit> {
		match &self.backing {
			BufferBacking::Dmabuf(backing) => {
				let (dmatex, acquire, release) = backing.update();
				let timeline = backing.timeline();
				let release_task = self.release_task(timeline.clone(), release);
				Some(BufferSubmit {
					dmatex,
					acquire,
					release: SignalOnDrop::new(timeline, release)?,
					opaque: !backing.is_transparent(),
					source: SubmitSource::Dmabuf {
						buffer: self.clone(),
						release_task,
					},
				})
			}
			BufferBacking::Shm(backing) => {
				let update = shm.and_then(|textures| textures.update(backing, damage));
				let _ = self
					.message_sink
					.send(crate::client::Message::ReleaseBuffer(self.clone()));
				let (tex, acquire, release) = update?;
				Some(BufferSubmit {
					dmatex: tex.dmatex.dmatex.clone(),
					acquire,
					release: SignalOnDrop::new(tex.timeline.clone(), release)?,
					opaque: !backing.is_transparent(),
					source: SubmitSource::Shm(tex),
				})
			}
		}
	}

	pub fn shm(&self) -> Option<&ShmBufferBacking> {
		match &self.backing {
			BufferBacking::Shm(backing) => Some(backing),
			BufferBacking::Dmabuf(_) => None,
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
			BufferBacking::Shm(_) => unreachable!("shm submits get their points from ShmTextures"),
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
				match timeline.wait_async(release) {
					Ok(wait) => wait.await,
					Err(e) => {
						tracing::error!("failed to wait for buffer release, releasing now: {e}")
					}
				}
				tracing::trace!("sending buffer release");
				message_sink.send(crate::client::Message::ReleaseBuffer(buffer))
			}
		})
		.abort_handle()
	}
}
enum SubmitSource {
	Dmabuf {
		buffer: Arc<Buffer>,
		// this is explicitly not an AbortOnDrop, we only want to abort it in some rare cases
		release_task: AbortHandle,
	},
	Shm(Arc<ShmTexture>),
}
pub struct BufferSubmit {
	dmatex: DmatexRef,
	acquire: u64,
	release: DmatexSubmitReleaseLocal<SignalOnDrop>,
	opaque: bool,
	source: SubmitSource,
}
impl BufferSubmit {
	pub fn reapply(self) -> Option<BufferSubmit> {
		let timeline = self.release.timeline().clone();
		let (new_release, source) = match self.source {
			SubmitSource::Dmabuf {
				buffer,
				release_task,
			} => {
				let new_release = buffer.new_timeline_point();
				release_task.abort();
				let release_task = buffer.release_task(timeline.clone(), new_release);
				(
					new_release,
					SubmitSource::Dmabuf {
						buffer,
						release_task,
					},
				)
			}
			SubmitSource::Shm(tex) => (tex.new_timeline_point(), SubmitSource::Shm(tex)),
		};
		Some(BufferSubmit {
			dmatex: self.dmatex,
			acquire: self.acquire,
			release: SignalOnDrop::new(timeline, new_release)?,
			opaque: self.opaque,
			source,
		})
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
	pub fn consumed(&self) -> bool {
		self.release.consumed()
	}
	pub fn opaque(&self) -> bool {
		self.opaque
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
