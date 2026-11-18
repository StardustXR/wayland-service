use super::{
	positioner::{Positioner, PositionerData},
	surface::Surface,
};
use crate::{client::Client, error::WaylandResult};
use mint::Vector2;
use parking_lot::Mutex;
use rand::random;
use stardust_xr_panels::panel_item::{ChildState, Rect, SurfaceId, SurfaceUpdateTarget};
use std::sync::Arc;
use waynest::ObjectId;
use waynest_protocols::server::stable::xdg_shell::xdg_popup::XdgPopup;
use waynest_server::Client as _;

/// same dance as a toplevel's, minus the wait for a panel item since a popup just
/// becomes a child of its parent's
#[derive(Debug)]
enum Lifecycle {
	/// no configure may be sent until the client's first commit
	AwaitingInitialCommit,
	AwaitingAck,
	Configured,
	Mapped,
}

/// whether the commit handler has to send the initial configure
pub enum Commit {
	Nothing,
	Configure,
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Popup {
	version: u32,
	pub surface: Arc<Surface>,
	parent: Option<SurfaceId>,
	positioner_data: Mutex<PositionerData>,
	lifecycle: Mutex<Lifecycle>,
	id: ObjectId,
}
impl Popup {
	pub fn new(
		version: u32,
		surface: Arc<Surface>,
		parent: Option<SurfaceId>,
		positioner_data: PositionerData,
		id: ObjectId,
	) -> Self {
		let _ = surface
			.wl_surface
			.surface_id
			.set(SurfaceUpdateTarget::Child { id: random() });

		Self {
			version,
			surface,
			parent,
			positioner_data: Mutex::new(positioner_data),
			lifecycle: Mutex::new(Lifecycle::AwaitingInitialCommit),
			id,
		}
	}

	pub fn commit(&self, has_buffer: bool) -> Commit {
		let mut lifecycle = self.lifecycle.lock();
		match *lifecycle {
			Lifecycle::AwaitingInitialCommit => {
				*lifecycle = Lifecycle::AwaitingAck;
				Commit::Configure
			}
			// without a panel item yet this just stays configured and tries again next commit
			Lifecycle::Configured if has_buffer && self.add_child() => {
				*lifecycle = Lifecycle::Mapped;
				Commit::Nothing
			}
			Lifecycle::Mapped if !has_buffer => {
				*lifecycle = Lifecycle::AwaitingInitialCommit;
				self.remove_child();
				Commit::Nothing
			}
			_ => Commit::Nothing,
		}
	}
	pub fn acked(&self) {
		let mut lifecycle = self.lifecycle.lock();
		if matches!(*lifecycle, Lifecycle::AwaitingAck) {
			*lifecycle = Lifecycle::Configured;
		}
	}
	fn unmap(&self) {
		let mut lifecycle = self.lifecycle.lock();
		if matches!(*lifecycle, Lifecycle::Mapped) {
			self.remove_child();
		}
		*lifecycle = Lifecycle::AwaitingInitialCommit;
	}

	fn add_child(&self) -> bool {
		let wl_surface = &self.surface.wl_surface;
		let (Some(panel_item), Some(parent), Some(SurfaceUpdateTarget::Child { id })) = (
			wl_surface.panel_item(),
			self.parent,
			wl_surface.surface_id.get().copied(),
		) else {
			return false;
		};
		panel_item.add_child(
			wl_surface,
			ChildState {
				id,
				parent,
				geometry: self.positioner_data.lock().infinite_geometry(),
				z_order: 1,
				input_regions: vec![Rect {
					origin: Vector2::from([0.0; 2]),
					size: Vector2::from([1.0; 2]),
				}],
			},
		);
		true
	}
	fn remove_child(&self) {
		if let Some(panel_item) = self.surface.wl_surface.panel_item() {
			panel_item.remove_child(&self.surface.wl_surface);
		}
	}

	pub async fn send_configure(&self, client: &mut Client) -> WaylandResult<()> {
		if !client.holds(self.id, self)
			|| matches!(*self.lifecycle.lock(), Lifecycle::AwaitingInitialCommit)
		{
			return Ok(());
		}
		let geometry = self.positioner_data.lock().infinite_geometry();
		self.configure(
			client,
			self.id,
			geometry.origin.x,
			geometry.origin.y,
			geometry.size.x as i32,
			geometry.size.y as i32,
		)
		.await?;
		self.surface.reconfigure(client).await
	}
}
impl XdgPopup for Popup {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/xdg-shell#xdg_popup:request:grab
	async fn grab(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_seat: ObjectId,
		_serial: u32,
	) -> WaylandResult<()> {
		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_popup:request:reposition
	async fn reposition(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
		positioner: ObjectId,
		token: u32,
	) -> WaylandResult<()> {
		let positioner_data = client
			.try_get::<Positioner>(positioner)?
			.complete(self.surface.wm_base)?;
		*self.positioner_data.lock() = positioner_data;

		// before the initial commit the first configure will carry the new position anyway
		if matches!(*self.lifecycle.lock(), Lifecycle::AwaitingInitialCommit) {
			return Ok(());
		}
		if self.version >= 3 {
			self.repositioned(client, sender_id, token).await?;
		}
		self.send_configure(client).await?;

		if matches!(*self.lifecycle.lock(), Lifecycle::Mapped)
			&& let Some(panel_item) = self.surface.wl_surface.panel_item()
		{
			panel_item.reposition_child(
				&self.surface.wl_surface,
				positioner_data.infinite_geometry(),
			);
		}
		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_popup:request:destroy
	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		self.unmap();
		self.surface.wl_surface.retire_role();
		Ok(())
	}
}
impl Drop for Popup {
	fn drop(&mut self) {
		self.unmap();
	}
}
