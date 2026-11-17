use crate::{
	CLIENT,
	client::{Client, Message},
	default_panel_shell::spawn_default_panel_shell,
	error::{WaylandError, WaylandResult},
	panel_item_ui::PanelItemUi,
	protocols::{
		core::surface::{RoleError, SurfaceRole},
		xdg::toplevel::Toplevel,
	},
	util::get_env,
};

use super::{
	backend::XdgBackend,
	popup::{self, Popup},
	positioner::Positioner,
	toplevel::Commit,
};
use crate::protocols::core::seat::Seat;
use gluon_ipc::Node;
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_fusion::spatial::{Spatial, SpatialExt as _, Transform};
use stardust_xr_panels::panel_item::{SurfaceId, SurfaceUpdateTarget};
use std::sync::{Arc, Weak};
use waynest::ObjectId;
pub use waynest_protocols::server::stable::xdg_shell::xdg_surface::*;
use waynest_protocols::server::stable::xdg_shell::xdg_wm_base::Error as WmBaseError;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Surface {
	id: ObjectId,
	pub(super) version: u32,
	pub wm_base: ObjectId,
	pub wl_surface: Arc<crate::protocols::core::surface::Surface>,
	role_object: Mutex<RoleObject>,
}

/// whichever role object currently plays this surface's role, so acks reach it
#[derive(Debug)]
enum RoleObject {
	None,
	Toplevel(Weak<Toplevel>),
	Popup(Weak<Popup>),
}
impl Surface {
	pub fn new(
		id: ObjectId,
		version: u32,
		wm_base: ObjectId,
		wl_surface: Arc<crate::protocols::core::surface::Surface>,
	) -> Self {
		Self {
			id,
			version,
			wm_base,
			wl_surface,
			role_object: Mutex::new(RoleObject::None),
		}
	}

	fn has_role_object(&self) -> bool {
		match &*self.role_object.lock() {
			RoleObject::Toplevel(t) => t.strong_count() > 0,
			RoleObject::Popup(p) => p.strong_count() > 0,
			RoleObject::None => false,
		}
	}

	fn role_error(&self, e: RoleError) -> WaylandError {
		match e {
			RoleError::Occupied => WaylandError::Fatal {
				object_id: self.id,
				code: Error::AlreadyConstructed as u32,
				message: "xdg_surface already has a role object",
			},
			RoleError::Mismatch => WaylandError::Fatal {
				object_id: self.wm_base,
				code: WmBaseError::Role as u32,
				message: "wl_surface already has a different role",
			},
		}
	}

	pub async fn reconfigure(&self, client: &mut Client) -> WaylandResult<()> {
		let serial = client.next_event_serial();
		self.configure(client, self.id, serial).await
	}
}

async fn map_toplevel(
	toplevel: Arc<Toplevel>,
	generation: u64,
	seat: Weak<Seat>,
	pid: Option<i32>,
) {
	let panel_item = create_panel_item(&toplevel, seat, pid).await;
	if toplevel.finish_mapping(generation, panel_item).is_ok() {
		toplevel
			.wl_surface()
			.apply_buffered_surface_update_recursive();
	}
}

async fn create_panel_item(
	toplevel: &Arc<Toplevel>,
	seat: Weak<Seat>,
	pid: Option<i32>,
) -> Option<Arc<Node<XdgBackend>>> {
	let Some(seat) = seat.upgrade() else {
		tracing::warn!("no seat available, cannot map toplevel");
		return None;
	};
	let client = CLIENT.wait();
	let spatial_token = pid
		.and_then(|pid| get_env(pid).ok())
		.and_then(|mut v| v.remove("STARDUST_STARTUP_TOKEN"));
	if let Some(panel_item) = spawn_default_panel_shell(&spatial_token, toplevel, &seat).await {
		return Some(panel_item);
	}
	let spatial_ref = if let Some(token) = spatial_token
		&& let Some(spatial_ref) = client.startup_token_spatial(token).await
	{
		spatial_ref
	} else {
		match Spatial::new(client, client.root(), Transform::IDENTITY).await {
			Ok((_, spatial_ref)) => spatial_ref,
			Err(e) => {
				tracing::error!("failed to create spatial for toplevel: {e}");
				return None;
			}
		}
	};
	PanelItemUi::create(spatial_ref, &seat, toplevel)
		.await
		.inspect_err(|e| tracing::error!("failed to create panel item ui for toplevel: {e:#}"))
		.ok()
}

impl XdgSurface for Surface {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/xdg-shell#xdg_surface:request:destroy
	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		self.wl_surface.end_xdg().map_err(|_| WaylandError::Fatal {
			object_id: self.id,
			code: Error::DefunctRoleObject as u32,
			message: "xdg_surface destroyed before its role object",
		})?;
		client.remove(self.id);
		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_surface:request:get_toplevel
	async fn get_toplevel(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
		toplevel_id: ObjectId,
	) -> WaylandResult<()> {
		self.wl_surface
			.assign_role(SurfaceRole::XdgToplevel)
			.map_err(|e| self.role_error(e))?;

		let toplevel = client.insert(
			toplevel_id,
			Toplevel::new(
				toplevel_id,
				self.wl_surface.clone(),
				client.try_get::<Self>(sender_id)?,
			),
		)?;

		let toplevel_weak = Arc::downgrade(&toplevel);
		let display = client.display()?;
		let seat = display.seat.get().map(Arc::downgrade).unwrap_or_default();
		let pid = display.pid;
		let message_tx = client.message_sink()?.clone();
		*self.wl_surface.toplevel.write() = toplevel_weak.clone();
		*self.role_object.lock() = RoleObject::Toplevel(toplevel_weak.clone());
		self.wl_surface.add_commit_handler(move |surface| {
			let Some(toplevel) = toplevel_weak.upgrade() else {
				return false;
			};
			match toplevel.commit(surface.currently_has_valid_buffer(), surface.current_size()) {
				Commit::Nothing => {}
				Commit::Configure => {
					let _ = message_tx.send(Message::ReconfigureToplevel(toplevel));
				}
				Commit::Map { generation } => {
					tokio::spawn(map_toplevel(toplevel, generation, seat.clone(), pid));
				}
				Commit::Resized(size) => {
					if let Some(panel_item) = toplevel.panel_item()
						&& let Some(shell) = panel_item.panel_shell()
					{
						_ = shell.toplevel_resized(Vector2 {
							x: size.x as u32,
							y: size.y as u32,
						});
					}
				}
			}
			true
		});

		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_surface:request:get_popup
	async fn get_popup(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		popup_id: ObjectId,
		parent: Option<ObjectId>,
		positioner: ObjectId,
	) -> WaylandResult<()> {
		self.wl_surface
			.assign_role(SurfaceRole::XdgPopup)
			.map_err(|e| self.role_error(e))?;

		let invalid_parent = |message| WaylandError::Fatal {
			object_id: self.wm_base,
			code: WmBaseError::InvalidPopupParent as u32,
			message,
		};
		// a null parent is for protocols like layer shell to fill in, and none are offered here
		let parent = parent.ok_or(invalid_parent("popup parent can't be null here"))?;
		let parent = client
			.get::<Surface>(parent)
			.ok_or(invalid_parent("popup parent isn't a live xdg_surface"))?;
		if !parent.has_role_object() {
			return Err(invalid_parent("popup parent has no toplevel or popup"));
		}
		let toplevel = parent.wl_surface.toplevel.read().clone();
		*self.wl_surface.toplevel.write() = toplevel;

		let positioner = client
			.try_get::<Positioner>(positioner)?
			.complete(self.wm_base)?;

		let parent_id = parent.wl_surface.surface_id.get().map(|id| match *id {
			SurfaceUpdateTarget::Toplevel => SurfaceId::Toplevel,
			SurfaceUpdateTarget::Child { id } => SurfaceId::Child { id },
			SurfaceUpdateTarget::Cursor => {
				tracing::error!("creating popup for cursor, defaulting to toplevel instead");
				SurfaceId::Toplevel
			}
		});
		let surface = client.try_get::<Surface>(self.id)?;
		let popup = client.insert(
			popup_id,
			Popup::new(self.version, surface, parent_id, positioner, popup_id),
		)?;

		let popup_weak = Arc::downgrade(&popup);
		*self.role_object.lock() = RoleObject::Popup(popup_weak.clone());
		let message_tx = client.message_sink()?;
		self.wl_surface.add_commit_handler(move |surface| {
			let Some(popup) = popup_weak.upgrade() else {
				return false;
			};
			if let popup::Commit::Configure = popup.commit(surface.currently_has_valid_buffer()) {
				let _ = message_tx.send(Message::ConfigurePopup(popup));
			}
			true
		});

		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_surface:request:set_window_geometry
	async fn set_window_geometry(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_x: i32,
		_y: i32,
		_width: i32,
		_height: i32,
	) -> WaylandResult<()> {
		// we're gonna delegate literally all the window management
		// to 3D stuff sooo we don't care, maximized is the floating state
		Ok(())
	}

	/// https://wayland.app/protocols/xdg-shell#xdg_surface:request:ack_configure
	async fn ack_configure(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_serial: u32,
	) -> WaylandResult<()> {
		match &*self.role_object.lock() {
			RoleObject::Toplevel(t) => t.upgrade().map(|t| t.acked()),
			RoleObject::Popup(p) => p.upgrade().map(|p| p.acked()),
			RoleObject::None => None,
		};
		Ok(())
	}
}
