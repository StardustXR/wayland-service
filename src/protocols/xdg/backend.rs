use super::toplevel::Toplevel;
use crate::{
    BINDER_DEV,
    client::Message,
    panel_item_ui::PanelItemUi,
    protocols::core::{
        seat::{Seat, SeatMessage},
        surface::Surface,
    },
};
use binderbinder::binder_object::BinderObject;
use dashmap::DashMap;
use gluon::Handler;
use stardust_xr_fusion::{
    keymap::Keymap,
    spatial::SpatialRef,
    types::{Size2, Timestamp, Vec2F},
};
use stardust_xr_panel_item::{
    panel_item::{
        ChildState, Geometry, ModifierState, PanelItem, PanelItemHandler, PanelShell, ScrollSource,
        SurfaceId, SurfaceUpdateTarget,
    },
    panel_item_acceptor::PanelItemAcceptor,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Weak;
use std::sync::{Arc, OnceLock};
use tokio::task::AbortHandle;
use tracing;

#[derive(Handler)]
pub struct XdgBackend {
    seat: Weak<Seat>,
    toplevel: Weak<Toplevel>,
    panel_shell: OnceLock<PanelShell>,
    output_spatial: OnceLock<SpatialRef>,
    pub children: DashMap<u64, (Weak<Surface>, ChildState)>,
    task: OnceLock<AbortHandle>,
}
impl Drop for XdgBackend {
    fn drop(&mut self) {
        if let Some(task) = self.task.get() {
            task.abort();
        }
    }
}

impl std::fmt::Debug for XdgBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XdgBackend")
            .field("_seat", &self.seat)
            .field("toplevel", &self.toplevel)
            .field("panel_shell", &self.panel_shell)
            .field("output_spatial", &self.output_spatial)
            .field("children", &self.children)
            .finish()
    }
}

impl XdgBackend {
    pub fn new(
        seat: &Arc<Seat>,
        toplevel: &Arc<Toplevel>,
        panel_shell: PanelShell,
        output_spatial_ref: SpatialRef,
    ) -> Self {
        let backend = Self {
            seat: Arc::downgrade(seat),
            toplevel: Arc::downgrade(toplevel),
            children: DashMap::new(),
            panel_shell: OnceLock::from(panel_shell),
            output_spatial: OnceLock::from(output_spatial_ref),
            task: OnceLock::new(),
        };
        backend.reset_input();
        backend
    }
    /// Boxed so callers (like `PanelItemUi`) don't have to know this function's
    /// concrete future type: it recursively spawns a task that calls back into
    /// `PanelItemUi::create`, and any caller that already sits in `create`'s own
    /// call graph would otherwise create a cycle when rustc tries to resolve the
    /// opaque `impl Future` types of both functions against each other.
    pub fn connect(
        item_acceptor: PanelItemAcceptor,
        seat: &Arc<Seat>,
        toplevel: &Arc<Toplevel>,
    ) -> Pin<Box<dyn Future<Output = Arc<BinderObject<XdgBackend>>> + Send + Sync>> {
        let seat = seat.clone();
        let toplevel = toplevel.clone();
        Box::pin(async move {
            let dev = BINDER_DEV.wait();
            let item_backend = XdgBackend {
                seat: Arc::downgrade(&seat),
                toplevel: Arc::downgrade(&toplevel),
                children: DashMap::new(),
                panel_shell: OnceLock::new(),
                output_spatial: OnceLock::new(),
                task: OnceLock::new(),
            };
            let obj = Arc::new(dev.register_object(item_backend));
            let (shell, spatial_ref) = item_acceptor
                .accept(PanelItem::from_handler(&*obj))
                .await
                .unwrap();
            let drop_future = obj.strong_refs_hit_zero();
            obj.panel_shell.set(shell).unwrap();
            obj.output_spatial.set(spatial_ref).unwrap();
            tokio::spawn({
                let obj = Arc::downgrade(&obj);
                async move {
                    drop_future.await;
                    if let Some(obj) = obj.upgrade() {
                        let Some(seat) = obj.seat.upgrade() else {
                            tracing::warn!("seat gone, cannot switch panel shell");
                            return;
                        };
                        let shell = PanelItemUi::create(
                            obj.output_spatial.get().unwrap().clone(),
                            &seat,
                            &obj.toplevel(),
                            false
                        )
                        .await;
                        obj.toplevel().switch_panel_shell(shell).await;
                    }
                }
            });
            if let Some(title) = toplevel.title() {
                _ = obj.panel_shell().toplevel_title(title);
            }
            if let Some(app_id) = toplevel.app_id() {
                _ = obj.panel_shell().toplevel_app_id(app_id);
            }
            obj
        })
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
        let Some(SurfaceUpdateTarget::Child { id }) = surface.surface_id.get().cloned() else {
            return;
        };
        if info.id != id {
            tracing::warn!("id mismatch between child state and surf");
        }
        self.children
            .insert(id, (Arc::downgrade(surface), info.clone()));

        self.panel_shell().create_child(info.clone()).unwrap();
    }

    pub fn reposition_child(&self, surface: &Arc<Surface>, geometry: Geometry) {
        let Some(SurfaceUpdateTarget::Child { id }) = surface.surface_id.get() else {
            return;
        };

        if let Some(mut child) = self.children.get_mut(id) {
            child.1.geometry = geometry;
        }
        self.panel_shell().move_child(*id, geometry).unwrap();
    }

    pub fn update_child_z_order(&self, surface: &Arc<Surface>, z_order: i32) {
        let Some(SurfaceUpdateTarget::Child { id }) = surface.surface_id.get() else {
            return;
        };

        if let Some(mut child) = self.children.get_mut(id) {
            child.1.z_order = z_order;
            let info = child.1.clone();
            drop(child);
            // TODO: this seems very wrong, idk if we ever communicate the z order here
            self.panel_shell().move_child(*id, info.geometry).unwrap();
        }
    }

    pub fn remove_child(&self, surface: &Surface) {
        let Some(SurfaceUpdateTarget::Child { id }) = surface.surface_id.get() else {
            return;
        };
        self.children.remove(id);

        self.panel_shell().destroy_child(*id).unwrap();
    }
}
impl PanelItemHandler for XdgBackend {
    async fn pointer_motion(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        delta: Option<Vec2F>,
        position: Vec2F,
        _timestamp: Option<Timestamp>,
    ) {
        let Some(surface) = self.surface_from_id(&surface) else {
            return;
        };
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::PointerMotion {
                surface,
                position,
                delta,
            }));
    }

    async fn pointer_button(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        button: u32,
        pressed: bool,
        _timestamp: Option<Timestamp>,
    ) {
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

    async fn pointer_scroll_discrete(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        delta: Vec2F,
        source: ScrollSource,
        _timestamp: Option<Timestamp>,
    ) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollDiscrete {
                    surface,
                    delta,
                    source,
                }));
        }
    }

    async fn pointer_scroll_pixels(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        delta: Vec2F,
        source: ScrollSource,
        _timestamp: Option<Timestamp>,
    ) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollDiscrete {
                    surface,
                    delta,
                    source,
                }));
        }
    }

    async fn pointer_scroll_stop(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        _timestamp: Option<Timestamp>,
    ) {
        if let Some(surface) = self.surface_from_id(&surface) {
            let _ = self
                .toplevel()
                .wl_surface()
                .message_sink
                .send(Message::Seat(SeatMessage::PointerScrollStop { surface }));
        }
    }

    async fn key(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        key: u32,
        pressed: bool,
        modifier_state: ModifierState,
        keymap: Keymap,
        _timestamp: Option<Timestamp>,
    ) {
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
                    keymap,
                    key,
                    pressed,
                    modifier_state,
                }));
        }
    }

    async fn touch_down(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceId,
        id: u32,
        position: Vec2F,
        _timestamp: Option<Timestamp>,
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
                    position,
                }));
        }
    }

    async fn touch_move(
        &self,
        _ctx: gluon::Context,
        id: u32,
        position: Vec2F,
        _timestamp: Option<Timestamp>,
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
            .send(Message::Seat(SeatMessage::TouchMove { id, position }));
    }

    async fn touch_up(&self, _ctx: gluon::Context, id: u32, _timestamp: Option<Timestamp>) {
        tracing::debug!("Backend: Touch up {}", id);
        let toplevel = self.toplevel();
        let _ = toplevel
            .wl_surface()
            .message_sink
            .send(Message::Seat(SeatMessage::TouchUp { id }));
    }

    async fn close_toplevel(&self, _ctx: gluon::Context) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::CloseToplevel(self.toplevel().clone()));
    }

    async fn resize_toplevel_to_app_request(&self, _ctx: gluon::Context) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::ResizeToplevel {
                toplevel: self.toplevel().clone(),
                size: None,
            });
    }

    async fn request_toplevel_resize(&self, _ctx: gluon::Context, new_size: Size2) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::ResizeToplevel {
                toplevel: self.toplevel().clone(),
                size: Some(new_size),
            });
    }

    async fn toplevel_focused(&self, _ctx: gluon::Context, focused: bool) {
        let _ = self
            .toplevel()
            .wl_surface()
            .message_sink
            .send(Message::SetToplevelVisualActive {
                toplevel: self.toplevel().clone(),
                active: focused,
            });
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
