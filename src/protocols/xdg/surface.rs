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

use super::{backend::XdgBackend, popup::Popup, positioner::Positioner, toplevel::Commit};
use crate::protocols::core::seat::Seat;
use gluon_ipc::Node;
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_fusion::spatial::{Spatial, SpatialExt as _, Transform};
use stardust_xr_panels::panel_item::{ChildState, Rect, SurfaceId, SurfaceUpdateTarget};
use std::sync::{Arc, Weak};
use waynest::ObjectId;
pub use waynest_protocols::server::stable::xdg_shell::xdg_surface::*;
use waynest_protocols::server::stable::xdg_shell::{
	xdg_popup::XdgPopup, xdg_wm_base::Error as WmBaseError,
};
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Surface {
	id: ObjectId,
	version: u32,
	pub wm_base: ObjectId,
	pub wl_surface: Arc<crate::protocols::core::surface::Surface>,
	configured: Arc<std::sync::atomic::AtomicBool>,
	toplevel: Mutex<Weak<Toplevel>>,
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
			configured: Arc::new(std::sync::atomic::AtomicBool::new(false)),
			toplevel: Mutex::new(Weak::new()),
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
		*self.toplevel.lock() = toplevel_weak.clone();
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
		sender_id: ObjectId,
		popup_id: ObjectId,
		parent: Option<ObjectId>,
		positioner: ObjectId,
	) -> WaylandResult<()> {
		self.wl_surface
			.assign_role(SurfaceRole::XdgPopup)
			.map_err(|e| self.role_error(e))?;

		let Some(parent) = parent else {
			return Err(WaylandError::Fatal {
				object_id: popup_id,
				code: 3,
				message: "Parent surface does not have an XDG role",
			});
		};
		let Some(parent) = client.get::<Surface>(parent) else {
			return Err(WaylandError::Fatal {
				object_id: popup_id,
				code: 3,
				message: "Parent surface does not exist",
			});
		};
		let toplevel = parent.wl_surface.toplevel.read().clone();
		*self.wl_surface.toplevel.write() = toplevel;

		let positioner = client
			.try_get::<Positioner>(positioner)?
			.complete(self.wm_base)?;

		let surface = client.try_get::<Surface>(self.id)?;

		let popup = client.insert(
			popup_id,
			Popup::new(self.version, surface, positioner, popup_id),
		)?;

		let positioner_geometry = positioner.infinite_geometry();

		popup
			.configure(
				client,
				popup_id,
				positioner_geometry.origin.x,
				positioner_geometry.origin.y,
				positioner_geometry.size.x as i32,
				positioner_geometry.size.y as i32,
			)
			.await?;
		let serial = client.next_event_serial();
		self.configure(client, sender_id, serial).await?;

		let Some(SurfaceUpdateTarget::Child { id }) = self.wl_surface.surface_id.get() else {
			return Ok(());
		};
		let Some(parent_id) = parent.wl_surface.surface_id.get() else {
			return Ok(());
		};
		let parent_id = match *parent_id {
			SurfaceUpdateTarget::Toplevel => SurfaceId::Toplevel,
			SurfaceUpdateTarget::Child { id } => SurfaceId::Child { id },
			SurfaceUpdateTarget::Cursor => {
				tracing::error!("creating popup for cursor, defaulting to toplevel instead");
				SurfaceId::Toplevel
			}
		};

		let child_info = ChildState {
			id: *id,
			parent: parent_id,
			geometry: positioner_geometry,
			z_order: 1,
			input_regions: vec![Rect {
				origin: Vector2::from([0.0; 2]),
				size: Vector2::from([1.0; 2]),
			}],
		};

		let popup_weak = Arc::downgrade(&popup);
		let configured = self.configured.clone();
		self.wl_surface.add_commit_handler(move |surface| {
			let Some(popup) = popup_weak.upgrade() else {
				return false;
			};
			let Some(panel_item) = surface.panel_item() else {
				return true;
			};

			if configured.load(std::sync::atomic::Ordering::SeqCst)
				&& surface.currently_has_valid_buffer()
			{
				panel_item.add_child(&popup.surface.wl_surface, child_info.clone());
				return false;
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
		self.configured
			.store(true, std::sync::atomic::Ordering::SeqCst);
		if let Some(toplevel) = self.toplevel.lock().upgrade() {
			toplevel.acked();
		}
		Ok(())
	}
}
