use crate::{
    CLIENT, PROJECT_DIRS,
    client::{Client, Message},
    display::Display,
    error::{WaylandError, WaylandResult},
    protocols::{core::surface::SurfaceRole, xdg::toplevel::Toplevel},
    util::get_env,
};

use super::{popup::Popup, positioner::Positioner, toplevel::MappedInner};
use mint::Vector2;
use stardust_xr_fusion::spatial::{Spatial, SpatialExt as _, Transform};
use stardust_xr_panel_item::panel_item::{ChildState, Rect, SurfaceId, SurfaceUpdateTarget};
use std::sync::Arc;
use waynest::ObjectId;
use waynest_protocols::server::stable::xdg_shell::xdg_popup::XdgPopup;
pub use waynest_protocols::server::stable::xdg_shell::xdg_surface::*;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Surface {
    id: ObjectId,
    version: u32,
    pub wl_surface: Arc<crate::protocols::core::surface::Surface>,
    configured: Arc<std::sync::atomic::AtomicBool>,
}
impl Surface {
    pub fn new(
        id: ObjectId,
        version: u32,
        wl_surface: Arc<crate::protocols::core::surface::Surface>,
    ) -> Self {
        Self {
            id,
            version,
            wl_surface,
            configured: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub async fn reconfigure(&self, client: &mut Client) -> WaylandResult<()> {
        let serial = client.next_event_serial();
        self.configure(client, self.id, serial).await
    }
}

impl XdgSurface for Surface {
    type Connection = crate::client::Client;

    /// https://wayland.app/protocols/xdg-shell#xdg_surface:request:destroy
    async fn destroy(
        &self,
        client: &mut Self::Connection,
        _sender_id: ObjectId,
    ) -> WaylandResult<()> {
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
        let toplevel = client.insert(
            toplevel_id,
            Toplevel::new(
                toplevel_id,
                self.wl_surface.clone(),
                client.get::<Self>(sender_id).unwrap(),
            ),
        )?;

        self.wl_surface
            .try_set_role(SurfaceRole::XdgToplevel, Error::AlreadyConstructed)
            .await?;

        let toplevel_weak = Arc::downgrade(&toplevel);
        let display = client.get::<Display>(ObjectId::DISPLAY).unwrap();
        let seat = Arc::downgrade(display.seat.get().unwrap());
        let pid = dbg!(display.pid);
        let configured = self.configured.clone();
        let mut first_commit = true;
        let message_tx = client.message_sink().clone();
        *self.wl_surface.toplevel.write() = toplevel_weak.clone();
        self.wl_surface.add_commit_handler(move |surface| {
            let Some(toplevel) = toplevel_weak.upgrade() else {
                return true;
            };

            if first_commit {
                let _ = message_tx.send(Message::ReconfigureToplevel(toplevel.clone()));
                first_commit = false;
            }

            let mapped_lock = toplevel.mapped.lock();
            if mapped_lock.is_none()
                && configured.load(std::sync::atomic::Ordering::Relaxed)
                && surface.currently_has_valid_buffer()
            {
                drop(mapped_lock);
                let client = CLIENT.wait();
                let seat = seat.clone();
                let toplevel = toplevel.clone();
                let spatial_token = pid
                    .and_then(|pid| get_env(pid).ok())
                    .and_then(|mut v| v.remove("STARDUST_STARTUP_TOKEN"));
                tokio::spawn(async move {
                    if let path = PROJECT_DIRS.config_dir().join("default_panel_shell")
                        && path.exists()
                        && path.to_str().is_some()
                    {
                        let mut vars = Vec::with_capacity(4);
                        vars.push(("SDXR_WL_DEFAULT_PANEL_SHELL".into(), "1".into()));
                        if let Some(token) = spatial_token.as_ref() {
                            vars.push(("STARDUST_STARTUP_TOKEN".into(), token.clone()));
                        }
                        if let Some(app_id) = toplevel.app_id() {
                            vars.push(("SDXR_WL_APP_ID".into(), app_id));
                        }
                        if let Some(title) = toplevel.title() {
                            vars.push(("SDXR_WL_TITLE".into(), title));
                        }
                        protostar_launcher::launch(path.to_str().unwrap().into(), vars).await;
                    }
                    let spatial_ref = if let Some(token) = spatial_token
                        && let Some(spatial_ref) = client.startup_token_spatial(dbg!(token)).await
                    {
                        spatial_ref
                    } else {
                        let (_, spatial_ref) =
                            Spatial::new(client, client.root(), Transform::IDENTITY)
                                .await
                                .unwrap();
                        spatial_ref
                    };
                    let mapped_inner =
                        MappedInner::create(&seat.upgrade().unwrap(), &toplevel, spatial_ref).await;
                    let mut mapped_lock = toplevel.mapped.lock();
                    // *surface.panel_item.lock() = Arc::downgrade(&mapped_inner.panel_item);
                    mapped_lock.replace(mapped_inner);
                });
                return false;
            }
            drop(mapped_lock);
            if let Some(panel_item) = toplevel.panel_item() {
                let size_lock = toplevel.last_committed_res.lock();
                if let Some(size) = surface.current_buffer_size()
                    && size_lock.is_none_or(|v| v != size)
                {
                    _ = panel_item.panel_shell().toplevel_resized(Vector2 {
                        x: size.x as u32,
                        y: size.y as u32,
                    });
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
            .try_set_role(SurfaceRole::XdgPopup, Error::AlreadyConstructed)
            .await?;

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

        let positioner = client.get::<Positioner>(positioner).unwrap();

        let surface = client.get::<Surface>(self.id).unwrap();

        let popup = client.insert(
            popup_id,
            Popup::new(self.version, surface, &positioner, popup_id),
        )?;

        let positioner_geometry = positioner.data().infinite_geometry();

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
            parent: parent_id.clone(),
            geometry: positioner.data().infinite_geometry(),
            z_order: 1,
            input_regions: vec![Rect {
                origin: Vector2::from([0.0; 2]).into(),
                size: Vector2::from([1.0; 2]).into(),
            }],
        };

        let popup_weak = Arc::downgrade(&popup);
        let configured = self.configured.clone();
        self.wl_surface.add_commit_handler(move |surface| {
            let Some(popup) = popup_weak.upgrade() else {
                return true;
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
        Ok(())
    }
}
