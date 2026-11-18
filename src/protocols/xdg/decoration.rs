use super::toplevel::Toplevel;
use crate::{
	client::Client,
	error::{WaylandError, WaylandResult},
};
use std::sync::{Arc, Weak};
use waynest::ObjectId;
use waynest_protocols::server::unstable::xdg_decoration_unstable_v1::{
	zxdg_decoration_manager_v1::*, zxdg_toplevel_decoration_v1::*,
};
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct XdgDecorationManager {
	pub _version: u32,
	pub id: ObjectId,
}
impl ZxdgDecorationManagerV1 for XdgDecorationManager {
	type Connection = Client;

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}

	async fn get_toplevel_decoration(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
		toplevel: ObjectId,
	) -> WaylandResult<()> {
		let toplevel = client.try_get::<Toplevel>(toplevel)?;
		let existing = toplevel.decoration.lock().upgrade();
		if existing.is_some_and(|d| client.holds(d.id, &*d)) {
			return Err(WaylandError::Fatal {
				object_id: self.id,
				code: Error::AlreadyConstructed as u32,
				message: "xdg_toplevel already has a decoration",
			});
		}
		let decoration = client.insert(
			id,
			XdgDecoration {
				id,
				toplevel: Arc::downgrade(&toplevel),
			},
		)?;
		*toplevel.decoration.lock() = Arc::downgrade(&decoration);
		Ok(())
	}
}

/// the panel shell always draws the frame, so whatever the client asks for it gets server side
#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct XdgDecoration {
	pub id: ObjectId,
	toplevel: Weak<Toplevel>,
}
impl XdgDecoration {
	pub async fn send_mode(&self, client: &mut Client) -> WaylandResult<()> {
		self.configure(client, self.id, Mode::ServerSide).await
	}

	/// the mode only counts once it comes with an xdg_surface.configure, which waits for the initial commit
	async fn reconfigure(&self, client: &mut Client) -> WaylandResult<()> {
		match self.toplevel.upgrade() {
			Some(t) => t.reconfigure(client).await,
			None => Ok(()),
		}
	}
}
impl ZxdgToplevelDecorationV1 for XdgDecoration {
	type Connection = Client;

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(sender_id);
		Ok(())
	}

	async fn set_mode(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		_mode: Mode,
	) -> WaylandResult<()> {
		self.reconfigure(client).await
	}

	async fn unset_mode(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		self.reconfigure(client).await
	}
}
