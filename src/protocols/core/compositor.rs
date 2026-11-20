use super::{region::Region, surface::Surface};
use crate::error::{WaylandError, WaylandResult};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_compositor::*;
use waynest_protocols::server::core::wayland::wl_surface::WlSurface;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher, Default)]
#[waynest(error = WaylandError, connection = crate::client::Client)]
pub struct Compositor;
impl WlCompositor for Compositor {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_compositor:request:create_surface
	async fn create_surface(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		let surface = Surface::new(client, id)?;
		client.insert_raw(id, surface.clone())?;
		for output in client.display()?.outputs(client) {
			surface.enter(client, id, output.id).await?;
		}

		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_compositor:request:create_region
	async fn create_region(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		client.insert(id, Region::new(id))?;
		Ok(())
	}
}
