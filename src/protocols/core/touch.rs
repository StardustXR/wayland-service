use mint::Vector2;
use parking_lot::Mutex;
use std::{
	collections::HashMap,
	sync::{Arc, Weak},
};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_touch::*;
use waynest_server::Client as _;

use crate::{client::Client, error::WaylandResult, protocols::core::surface::Surface};

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Touch {
	pub id: ObjectId,
	/// the points the client saw go down, and on which surface
	points: Mutex<HashMap<u32, Weak<Surface>>>,
}
impl Touch {
	pub fn new(id: ObjectId) -> Self {
		Self {
			id,
			points: Mutex::new(HashMap::new()),
		}
	}

	pub async fn handle_touch_down(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		id: u32,
		position: Vector2<f32>,
	) -> WaylandResult<()> {
		if !client.holds(surface.id, &*surface) || self.points.lock().contains_key(&id) {
			return Ok(());
		}
		self.points.lock().insert(id, Arc::downgrade(&surface));
		let serial = client.next_event_serial();
		self.down(
			client,
			self.id,
			serial,
			0,
			surface.id,
			id as i32,
			(position.x as f64).into(),
			(position.y as f64).into(),
		)
		.await?;
		self.frame(client, self.id).await
	}

	pub async fn handle_touch_move(
		&self,
		client: &mut Client,
		id: u32,
		position: Vector2<f32>,
	) -> WaylandResult<()> {
		if !self.points.lock().contains_key(&id) {
			return Ok(());
		}
		self.motion(
			client,
			self.id,
			0,
			id as i32,
			(position.x as f64).into(),
			(position.y as f64).into(),
		)
		.await?;
		self.frame(client, self.id).await
	}

	pub async fn handle_touch_up(&self, client: &mut Client, id: u32) -> WaylandResult<()> {
		if self.points.lock().remove(&id).is_none() {
			return Ok(());
		}
		let serial = client.next_event_serial();
		self.up(client, self.id, serial, 0, id as i32).await?;
		self.frame(client, self.id).await
	}

	/// cancel ends every point the client has, not just the ones in this toplevel
	pub async fn reset(
		&self,
		client: &mut Client,
		toplevel_surface: &Surface,
	) -> WaylandResult<()> {
		let cancel = {
			let mut points = self.points.lock();
			let inside = points.values().any(|s| {
				s.upgrade()
					.is_some_and(|s| s.same_toplevel(toplevel_surface))
			});
			if inside {
				points.clear();
			}
			inside
		};
		if cancel {
			self.cancel(client, self.id).await?;
		}
		Ok(())
	}
}

impl WlTouch for Touch {
	type Connection = Client;

	/// https://wayland.app/protocols/wayland#wl_touch:request:release
	async fn release(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(sender_id);
		Ok(())
	}
}
