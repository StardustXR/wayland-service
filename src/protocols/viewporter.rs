use crate::{error::WaylandResult, protocols::core::surface::Surface};
use stardust_xr_panels::panel_item::Rect;
use std::sync::{Arc, Weak};
use waynest::{Fixed, ObjectId};
use waynest_protocols::server::stable::viewporter::wp_viewport::*;
use waynest_protocols::server::stable::viewporter::wp_viewporter::*;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Viewporter {
	id: ObjectId,
}

impl Viewporter {
	pub fn new(id: ObjectId) -> Self {
		Self { id }
	}
}

impl WpViewporter for Viewporter {
	type Connection = crate::client::Client;

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}

	async fn get_viewport(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
		surface_id: ObjectId,
	) -> WaylandResult<()> {
		let surface = client
			.get::<Surface>(surface_id)
			.map(|s| Arc::downgrade(&s))
			.unwrap_or_default();
		let viewport = Viewport::new(id, surface);
		client.insert(id, viewport)?;
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Viewport {
	id: ObjectId,
	surface: Weak<Surface>,
}
impl Viewport {
	pub fn new(id: ObjectId, surface: Weak<Surface>) -> Self {
		Self { id, surface }
	}
}

impl WpViewport for Viewport {
	type Connection = crate::client::Client;

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		if let Some(surface) = self.surface.upgrade() {
			let mut state = surface.state_lock();
			state.pending.viewport_src = None;
			state.pending.viewport_dst = None;
		}
		client.remove(self.id);
		Ok(())
	}

	async fn set_source(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		x: Fixed,
		y: Fixed,
		width: Fixed,
		height: Fixed,
	) -> WaylandResult<()> {
		let Some(surface) = self.surface.upgrade() else {
			return Ok(());
		};
		let [x, y, w, h] = [x, y, width, height].map(|v| f64::from(v) as f32);
		// all -1 unsets it
		surface.state_lock().pending.viewport_src = (w >= 0.0).then(|| Rect {
			origin: [x, y].into(),
			size: [w, h].into(),
		});
		Ok(())
	}

	async fn set_destination(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		let Some(surface) = self.surface.upgrade() else {
			return Ok(());
		};
		surface.state_lock().pending.viewport_dst =
			(width > 0 && height > 0).then(|| [width as u32, height as u32].into());
		Ok(())
	}
}
