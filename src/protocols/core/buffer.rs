use crate::client::{Client, Message, MessageSink};
use crate::error::WaylandResult;
use crate::protocols::dmabuf::buffer_backing::DmabufBacking;

use mint::Vector2;
use std::sync::Arc;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_buffer::*;
use waynest_server::{Client as _, RequestDispatcher};

#[derive(Debug)]
pub struct BufferUsage {
    pub buffer: Arc<Buffer>,
    message_sink: MessageSink,
}
impl BufferUsage {
    pub fn new(client: &Client, buffer: &Arc<Buffer>) -> Arc<Self> {
        Arc::new(Self {
            buffer: buffer.clone(),
            message_sink: client.message_sink(),
        })
    }
}
impl Drop for BufferUsage {
    fn drop(&mut self) {
        let _ = self
            .message_sink
            .send(Message::ReleaseBuffer(self.buffer.clone()));
    }
}

#[derive(Debug)]
pub enum BufferBacking {
    // Shm(ShmBufferBacking),
    Dmabuf(DmabufBacking),
}

#[derive(Debug, RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Buffer {
    pub id: ObjectId,
    backing: BufferBacking,
}

impl Buffer {
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(
        client: &mut Client,
        id: ObjectId,
        backing: BufferBacking,
    ) -> WaylandResult<Arc<Self>> {
        Ok(client.insert(id, Self { id, backing })?)
    }

    /// returns (dmatex_uid, server_acquire_point, server_release_point)
    pub fn update(&self) -> (u64, u64, u64) {
        match &self.backing {
            BufferBacking::Dmabuf(backing) => todo!(),
        }
    }

    pub fn is_transparent(&self) -> bool {
        match &self.backing {
            // BufferBacking::Shm(backing) => backing.is_transparent(),
            BufferBacking::Dmabuf(backing) => backing.is_transparent(),
        }
    }

    pub fn size(&self) -> Vector2<usize> {
        match &self.backing {
            // BufferBacking::Shm(backing) => backing.size(),
            BufferBacking::Dmabuf(backing) => backing.size(),
        }
    }
    pub fn uses_buffer_usage(&self) -> bool {
        matches!(
            self.backing,
            BufferBacking::Dmabuf(_) /* | BufferBacking::Shm(_) */
        )
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
