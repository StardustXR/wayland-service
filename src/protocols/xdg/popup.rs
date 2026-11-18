use super::{
	positioner::{Positioner, PositionerData},
	surface::Surface,
};
use crate::{
	client::{Client, Message},
	error::WaylandResult,
};
use mint::Vector2;
use parking_lot::Mutex;
use rand::random;
use stardust_xr_panels::panel_item::{ChildState, Rect, SurfaceId, SurfaceUpdateTarget};
use std::sync::{
	Arc, Weak,
	atomic::{AtomicBool, Ordering},
};
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
	/// popup_done was sent, the client is expected to destroy it and it never maps again
	Dismissed,
}

/// every live popup oldest first, walked backwards so nested popups get dismissed topmost first
static POPUPS: Mutex<Vec<Weak<Popup>>> = Mutex::new(Vec::new());

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
	grabbed: AtomicBool,
	parent_popup: Weak<Popup>,
	id: ObjectId,
}
impl Popup {
	pub fn new(
		version: u32,
		surface: Arc<Surface>,
		parent: Option<SurfaceId>,
		parent_popup: Weak<Popup>,
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
			grabbed: AtomicBool::new(false),
			parent_popup,
			id,
		}
	}

	pub fn register(popup: &Arc<Popup>) {
		POPUPS.lock().push(Arc::downgrade(popup));
	}
	fn dismiss_where(f: impl Fn(&Popup) -> bool) {
		let popups = {
			let mut popups = POPUPS.lock();
			popups.retain(|p| p.strong_count() > 0);
			popups
				.iter()
				.rev()
				.filter_map(Weak::upgrade)
				.collect::<Vec<_>>()
		};
		for popup in popups.into_iter().filter(|p| f(p)) {
			let sink = &popup.surface.wl_surface.message_sink;
			let _ = sink.send(Message::DismissPopup(popup.clone()));
		}
	}
	/// presses on the grabbing client's own surfaces are its to handle, anywhere else ends the grab
	pub fn dismiss_grabs_outside(surface: &crate::protocols::core::surface::Surface) {
		Self::dismiss_where(|p| {
			p.grabbed.load(Ordering::Relaxed)
				&& !p
					.surface
					.wl_surface
					.message_sink
					.same_channel(&surface.message_sink)
		});
	}
	pub fn dismiss_popups_of(toplevel_surface: &crate::protocols::core::surface::Surface) {
		Self::dismiss_where(|p| p.surface.wl_surface.same_toplevel(toplevel_surface));
	}
	pub fn topmost_grab(
		surface: &crate::protocols::core::surface::Surface,
	) -> Option<Arc<crate::protocols::core::surface::Surface>> {
		POPUPS
			.lock()
			.iter()
			.rev()
			.filter_map(Weak::upgrade)
			.find(|p| {
				p.grabbed.load(Ordering::Relaxed)
					&& !p.is_dismissed()
					&& p.surface.wl_surface.same_toplevel(surface)
			})
			.map(|p| p.surface.wl_surface.clone())
	}
	fn is_dismissed(&self) -> bool {
		matches!(*self.lifecycle.lock(), Lifecycle::Dismissed)
	}
	pub async fn dismiss(&self, client: &mut Client) -> WaylandResult<()> {
		{
			let mut lifecycle = self.lifecycle.lock();
			match *lifecycle {
				Lifecycle::Dismissed => return Ok(()),
				Lifecycle::Mapped => self.remove_child(),
				_ => {}
			}
			*lifecycle = Lifecycle::Dismissed;
		}
		if client.holds(self.id, self) {
			self.popup_done(client, self.id).await?;
		}
		Ok(())
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
			|| matches!(
				*self.lifecycle.lock(),
				Lifecycle::AwaitingInitialCommit | Lifecycle::Dismissed
			) {
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
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		_seat: ObjectId,
		_serial: u32,
	) -> WaylandResult<()> {
		self.grabbed.store(true, Ordering::Relaxed);
		if self
			.parent_popup
			.upgrade()
			.is_some_and(|p| p.is_dismissed())
		{
			return self.dismiss(client).await;
		}
		let display = client.display()?;
		display
			.devices
			.refocus_within(client, self.surface.wl_surface.clone())
			.await
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
		if matches!(
			*self.lifecycle.lock(),
			Lifecycle::AwaitingInitialCommit | Lifecycle::Dismissed
		) {
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
		let grabbed = self.grabbed.swap(false, Ordering::Relaxed);
		self.unmap();
		let wl_surface = &self.surface.wl_surface;
		wl_surface.retire_role();
		if !grabbed {
			return Ok(());
		}
		// the grab goes back to the parent popup if it had one, otherwise the toplevel
		let Some(target) = Self::topmost_grab(wl_surface)
			.or_else(|| Some(wl_surface.toplevel.read().upgrade()?.wl_surface().clone()))
		else {
			return Ok(());
		};
		let display = client.display()?;
		display.devices.refocus_within(client, target).await
	}
}
impl Drop for Popup {
	fn drop(&mut self) {
		self.unmap();
	}
}
