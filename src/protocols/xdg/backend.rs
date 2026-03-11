use super::toplevel::Toplevel;
use crate::{
    BINDER_DEV, CLIENT,
    client::Message,
    protocols::core::{
        keyboard::KEYMAPS,
        seat::{Seat, SeatMessage},
        surface::Surface,
    },
};
use binderbinder::{TransactionHandler, binder_object::BinderObject};
use dashmap::DashMap;
use gluon_wire::{GluonDataReader, drop_tracking::DropNotifier};
use slotmap::Key;
use stardust_xr_fusion::spatial::SpatialRef;
use stardust_xr_panel_item::protocol::{
    ChildState, Geometry, KeymapId, PanelItem, PanelItemAcceptor, PanelItemHandler, PanelShell,
    ScrollSource, SurfaceId,
};
use std::sync::Weak;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;
use tracing;

#[derive(Debug)]
pub struct XdgBackend {
    _seat: Weak<Seat>,
    toplevel: Weak<Toplevel>,
    panel_shell: OnceLock<PanelShell>,
    output_spatial: OnceLock<SpatialRef>,
    pub children: DashMap<u64, (Weak<Surface>, ChildState)>,
    drop_notifs: RwLock<Vec<DropNotifier>>,
}

impl XdgBackend {
    pub fn new(
        seat: &Arc<Seat>,
        toplevel: &Arc<Toplevel>,
        panel_shell: PanelShell,
        output_spatial_ref: SpatialRef,
    ) -> Self {
        let backend = Self {
            _seat: Arc::downgrade(seat),
            toplevel: Arc::downgrade(toplevel),
            children: DashMap::new(),
            panel_shell: OnceLock::from(panel_shell),
            output_spatial: OnceLock::from(output_spatial_ref),
            drop_notifs: RwLock::default(),
        };
        backend.reset_input();
        backend
    }
    pub async fn connect(
        item_acceptor: PanelItemAcceptor,
        seat: &Arc<Seat>,
        toplevel: &Arc<Toplevel>,
    ) -> Arc<BinderObject<XdgBackend>> {
        let dev = BINDER_DEV.wait();
        let item_backend = XdgBackend {
            _seat: Arc::downgrade(seat),
            toplevel: Arc::downgrade(toplevel),
            children: DashMap::new(),
            panel_shell: OnceLock::new(),
            output_spatial: OnceLock::new(),
            drop_notifs: RwLock::default(),
        };
        let obj = dev.register_object(item_backend);
        let (shell, spatial_ref_id) = item_acceptor.accept(PanelItem::from_handler(&obj)).await;
        let spatial_ref = SpatialRef::import(CLIENT.wait(), spatial_ref_id.id)
            .await
            .unwrap();
        obj.panel_shell.set(shell).unwrap();
        obj.output_spatial.set(spatial_ref).unwrap();
        obj.reset_input();
        obj
    }

    // Since XdgBackend is created and owned by Mapped which is owned by Toplevel,
    // we can safely assume the Toplevel reference will always be valid
    fn toplevel(&self) -> Arc<Toplevel> {
        self.toplevel
            .upgrade()
            .expect("Toplevel should always be valid while XdgBackend exists")
    }

    pub fn panel_shell(&self) -> &PanelShell {
        self.panel_shell.get().unwrap()
    }

    fn surface_from_id(&self, id: &SurfaceId) -> Option<Arc<Surface>> {
        match id {
            SurfaceId::Toplevel => Some(self.toplevel().wl_surface().clone()),
            SurfaceId::Child { id } => self.children.get(id).as_deref().and_then(|c| c.0.upgrade()),
        }
    }

    pub fn add_child(&self, surface: &Arc<Surface>, info: ChildState) {
        let Some(SurfaceId::Child { id }) = surface.surface_id.get().cloned() else {
            return;
        };
        if info.id != id {
            tracing::warn!("id mismatch between child state and surf");
        }
        self.children
            .insert(id, (Arc::downgrade(surface), info.clone()));

        self.panel_shell().create_child(info.clone());
    }

    pub fn reposition_child(&self, surface: &Arc<Surface>, geometry: Geometry) {
        let Some(SurfaceId::Child { id }) = surface.surface_id.get() else {
            return;
        };

        if let Some(mut child) = self.children.get_mut(id) {
            child.1.geometry = geometry.clone();
        }
        self.panel_shell().move_child(*id, geometry);
    }

    pub fn update_child_z_order(&self, surface: &Arc<Surface>, z_order: i32) {
        let Some(SurfaceId::Child { id }) = surface.surface_id.get() else {
            return;
        };

        if let Some(mut child) = self.children.get_mut(id) {
            child.1.z_order = z_order;
            let info = child.1.clone();
            drop(child);
            // TODO: this seems very wrong, idk if we ever communicate the z order here
            self.panel_shell().move_child(*id, info.geometry);
        }
    }

    pub fn remove_child(&self, surface: &Surface) {
        let Some(SurfaceId::Child { id }) = surface.surface_id.get() else {
            return;
        };
        self.children.remove(id);

        self.panel_shell().destroy_child(*id);
    }
}
impl PanelItemHandler for XdgBackend {
    async fn register_xkb_keymap(&self, xkb_keymap: String) -> KeymapId {
        let slot =
            if let Some((key, _)) = KEYMAPS.read().await.iter().find(|(_, v)| *v == &xkb_keymap) {
                key
            } else {
                KEYMAPS.write().await.insert(xkb_keymap)
            };

        KeymapId {
            id: slot.data().as_ffi(),
        }
    }

    fn absolute_pointer_motion(
        &self,
        surface: SurfaceId,
        position: stardust_xr_panel_item::protocol::Vec2,
    ) {
        let Some(surface) = self.surface_from_id(&surface) else {
            return;
        };
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::AbsolutePointerMotion {
                surface,
                position: position.into(),
            }));
    }

    fn relative_pointer_motion(
        &self,
        _surface: SurfaceId,
        delta: stardust_xr_panel_item::protocol::Vec2,
    ) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::RelativePointerMotion {
                delta: delta.into(),
            }));
    }

    fn pointer_button(&self, surface: SurfaceId, button: u32, pressed: bool) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerButton {
                    surface,
                    button,
                    pressed,
                }));
        }
    }

    fn pointer_scroll_discrete(
        &self,
        surface: SurfaceId,
        delta: stardust_xr_panel_item::protocol::Vec2,
        source: ScrollSource,
    ) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollDiscrete {
                    surface,
                    delta: delta.into(),
                    source,
                }));
        }
    }

    fn pointer_scroll_pixels(
        &self,
        surface: SurfaceId,
        delta: stardust_xr_panel_item::protocol::Vec2,
        source: ScrollSource,
    ) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollDiscrete {
                    surface,
                    delta: delta.into(),
                    source,
                }));
        }
    }

    fn pointer_scroll_stop(&self, surface: SurfaceId) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollStop { surface }));
        }
    }

    fn key(&self, surface: SurfaceId, keymap: KeymapId, key: u32, pressed: bool) {
        tracing::debug!(
            "Backend: Keyboard key {} {}",
            key,
            if pressed { "pressed" } else { "released" }
        );
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::KeyboardKey {
                    surface,
                    keymap_id: keymap.id,
                    key,
                    pressed,
                }));
        }
    }

    fn touch_down(
        &self,
        surface: SurfaceId,
        id: u32,
        position: stardust_xr_panel_item::protocol::Vec2,
    ) {
        tracing::debug!(
            "Backend: Touch down {} at ({}, {})",
            id,
            position.x,
            position.y
        );
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::TouchDown {
                    surface,
                    id,
                    position: position.into(),
                }));
        }
    }

    fn touch_move(
        &self,
        _surface: SurfaceId,
        id: u32,
        position: stardust_xr_panel_item::protocol::Vec2,
    ) {
        tracing::debug!(
            "Backend: Touch move {} to ({}, {})",
            id,
            position.x,
            position.y
        );
        let toplevel = self.toplevel();
        let _ = toplevel
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::TouchMove {
                id,
                position: position.into(),
            }));
    }

    fn touch_up(
        &self,
        _surface: SurfaceId,
        id: u32,
        _position: stardust_xr_panel_item::protocol::Vec2,
    ) {
        tracing::debug!("Backend: Touch up {}", id);
        let toplevel = self.toplevel();
        let _ = toplevel
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::TouchUp { id }));
    }

    fn close_toplevel(&self) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::CloseToplevel(self.toplevel().clone()));
    }

    fn resize_toplevel_to_app_request(&self) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::ResizeToplevel {
                toplevel: self.toplevel().clone(),
                size: None,
            });
    }

    fn request_toplevel_resize(&self, new_size: stardust_xr_panel_item::protocol::UVec2) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::ResizeToplevel {
                toplevel: self.toplevel().clone(),
                size: Some(new_size.into()),
            });
    }

    fn toplevel_focused(&self, focused: bool) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::SetToplevelVisualActive {
                toplevel: self.toplevel().clone(),
                active: focused,
            });
    }

    async fn drop_notification_requested(&self, notifier: DropNotifier) {
        self.drop_notifs.write().await.push(notifier);
    }
}
impl XdgBackend {
    // fn start_data(&self) -> Result<PanelItemInitData> {
    //     let top_level = self.toplevel();
    //     let surface = top_level.wl_surface();
    //     let state_lock = surface.state_lock();
    //     let surface_state = state_lock.current();
    //
    //     let size = surface_state
    //         .buffer
    //         .as_ref()
    //         .map(|b| [b.buffer.size().x as u32, b.buffer.size().y as u32].into())
    //         .unwrap_or([0; 2].into());
    //     let toplevel = ToplevelInfo {
    //         parent: self.toplevel().parent(),
    //         title: self.toplevel().title(),
    //         app_id: self.toplevel().app_id(),
    //         size,
    //         min_size: surface_state
    //             .min_size
    //             .map(|v| [v.x as f32, v.y as f32].into()),
    //         max_size: surface_state
    //             .max_size
    //             .map(|v| [v.x as f32, v.y as f32].into()),
    //         logical_rectangle: surface_state.geometry.unwrap_or(Geometry {
    //             origin: [0; 2].into(),
    //             size,
    //         }),
    //     };
    //
    //     Ok(ToplevelState {
    //         cursor: None,
    //         toplevel,
    //         children: vec![],
    //         pointer_grab: None,
    //         keyboard_grab: None,
    //     })
    // }

    fn reset_input(&self) {
        tracing::debug!("Backend: Reset input");
        let toplevel = self.toplevel();
        let _ = toplevel
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::Reset));
    }
}
impl TransactionHandler for XdgBackend {
    async fn handle(
        &self,
        transaction: binderbinder::device::Transaction,
    ) -> binderbinder::payload::PayloadBuilder<'_> {
        let mut data = GluonDataReader::from_payload(transaction.payload);
        self.dispatch_two_way(transaction.code, &mut data)
            .await
            .to_payload()
    }

    async fn handle_one_way(&self, transaction: binderbinder::device::Transaction) {
        let mut data = GluonDataReader::from_payload(transaction.payload);
        self.dispatch_one_way(transaction.code, &mut data).await
    }
}
