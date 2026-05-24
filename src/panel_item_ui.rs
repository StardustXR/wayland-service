use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};

use binderbinder::binder_object::BinderObject;
use gluon::Handler;
use mint::{Vector2, Vector3};
use stardust_xr_fusion::{
    drawable::{
        DmatexSubmitInfo, MaterialParameter, Model, ModelPart, ModelPartAspect, import_dmatex_uid,
    },
    fields::{Field, FieldAspect, FieldRefAspect, Shape},
    node::NodeType,
    root::FrameInfo,
    spatial::{Spatial, SpatialAspect, SpatialRef, Transform},
    values::ResourceID,
};
use stardust_xr_gluon::AbortOnDrop;
use stardust_xr_molecules::{FrameSensitive, Grabbable, GrabbableSettings, PointerMode, UIElement};
use stardust_xr_panel_item::protocol::{
    ChildState, Geometry, PanelItem, PanelShell, PanelShellHandler, SurfaceUpdateTarget,
};
use tokio::{
    sync::{RwLock, broadcast::error::RecvError},
    task::JoinSet,
};
use tracing::trace;

use crate::{
    BINDER_DEV, CLIENT, DBUS,
    frame_dispatcher::FRAME_EVENT_PROVIDER,
    panel_item_provider::ACCEPTORS,
    protocols::{
        core::seat::Seat,
        xdg::{backend::XdgBackend, toplevel::Toplevel},
    },
};

#[derive(Handler)]
pub struct PanelItemUi {
    toplevel: Weak<Toplevel>,
    seat: Weak<Seat>,
    root: Spatial,
    model: Model,
    field: Field,
    part: ModelPart,
    input_task: OnceLock<AbortOnDrop>,
    grabbable: RwLock<Grabbable>,
    replaced: AtomicBool,
}

impl std::fmt::Debug for PanelItemUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelItemUi")
            .field("model", &self.model)
            .field("field", &self.field)
            .field("part", &self.part)
            .field("grabbable", &"Grabbable")
            .finish()
    }
}

impl PanelItemUi {
    pub fn new(
        at: SpatialRef,
        seat: &Arc<Seat>,
        toplevel: &Arc<Toplevel>,
    ) -> Arc<BinderObject<XdgBackend>> {
        let client = CLIENT.wait();
        let conn = DBUS.wait();
        let dev = BINDER_DEV.wait();
        let size = toplevel
            .wl_surface()
            .current_buffer_size()
            .unwrap_or([1, 1].into());
        let root = Spatial::create(&at, Transform::identity()).unwrap();
        root.set_spatial_parent_in_place(client.get_root()).unwrap();
        let field = Field::create(
            &root,
            Transform::identity(),
            Shape::Box(Self::get_size(size)),
        )
        .unwrap();
        let id = root.id();
        let grabbable = Grabbable::create(
            conn.clone(),
            format!("/Panel{id:x}"),
            &root,
            Transform::identity(),
            &field,
            GrabbableSettings {
                max_distance: 0.02,
                // linear_momentum: Some(MomentumSettings {
                //     drag: 0.9,
                //     threshold: 0.02,
                // }),
                linear_momentum: None,
                angular_momentum: None,
                magnet: false,
                pointer_mode: PointerMode::Parent,
                reparentable: true,
            },
        )
        .unwrap();
        field
            .set_spatial_parent(&grabbable.content_parent())
            .unwrap();
        let model = Model::create(
            &field,
            Transform::from_scale(Self::get_size(size)),
            &ResourceID::new_namespaced("wayland-service", "panel"),
        )
        .unwrap();
        let part = model.part("Panel").unwrap();
        let obj = dev.register_object(Self {
            toplevel: Arc::downgrade(&toplevel),
            seat: Arc::downgrade(&seat),
            root,
            model,
            field,
            part,
            grabbable: RwLock::new(grabbable),
            input_task: OnceLock::new(),
            replaced: AtomicBool::new(false),
        });
        let panel_shell = PanelShell::from_handler(&obj);
        let backend = dev.register_object(XdgBackend::new(seat, toplevel, panel_shell, at));
        let panel_item = PanelItem::from_handler(&backend);
        let input_task = tokio::spawn({
            let obj = Arc::downgrade(&obj);
            async move {
                let mut recv = FRAME_EVENT_PROVIDER.subscribe();
                loop {
                    let frame_info = match recv.recv().await {
                        Err(RecvError::Closed) => break,
                        Err(RecvError::Lagged(v)) => {
                            tracing::warn!("Missed {v} frame events");
                            continue;
                        }
                        Ok(v) => v,
                    };
                    let Some(obj) = obj.upgrade() else {
                        break;
                    };
                    if obj.replaced.load(Ordering::Relaxed) {
                        break;
                    }
                    obj.update_input(frame_info).await
                }
            }
        });
        _ = obj.input_task.set(input_task.into());
        let drop_future = obj.strong_refs_hit_zero();
        tokio::spawn(async move {
            drop_future.await;
            tracing::debug!("dropping panel item ui: {:?}", obj.root.id());
        });
        Arc::new(backend)
    }
    async fn update_input(&self, frame_info: FrameInfo) {
        let mut grabbable = self.grabbable.write().await;
        if grabbable.handle_events() {
            grabbable.frame(&frame_info);
        }
        if self.replaced.load(Ordering::Relaxed) {
            return;
        }
        let mut join_set = JoinSet::from_iter(ACCEPTORS.read().await.iter().cloned().map(
            |(field, acceptor)| {
                let ref_space = grabbable.content_parent().clone();
                async move {
                    Some((
                        field
                            .distance(&ref_space, [0.0; 3])
                            .await
                            .inspect_err(|err| {
                                tracing::error!("failed to get field distance: {err}")
                            })
                            .ok()?,
                        acceptor,
                    ))
                }
            },
        ));
        while let Some(v) = join_set.join_next().await {
            let Ok(Some((distance, acceptor))) = v else {
                continue;
            };
            trace!(distance);
            if distance <= 0.01 {
                let Some(toplevel) = self.toplevel.upgrade() else {
                    break;
                };
                let Some(seat) = self.seat.upgrade() else {
                    break;
                };
                tracing::info!("connecting to new panel item acceptor");
                let obj = XdgBackend::connect(acceptor, &seat, &toplevel).await;
                toplevel.switch_panel_shell(obj).await;
                self.replaced.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
}

impl PanelItemUi {
    fn get_size(size: impl Into<Vector2<usize>>) -> Vector3<f32> {
        let size = size.into();
        let width_to_height = size.y as f32 / size.x as f32;

        [0.1, 0.1 * width_to_height, 0.001].into()
    }
}

impl PanelShellHandler for PanelItemUi {
    async fn update_surface_dmatex(
        &self,
        _ctx: gluon::Context,
        surface: SurfaceUpdateTarget,
        dmatex_uid: u64,
        acquire_point: u64,
        release_point: u64,
        opaque: bool,
    ) {
        // TODO: remove this when children are implemented
        // if !matches!(surface, SurfaceUpdateTarget::Toplevel) {
        //     tracing::warn!(
        //         "surface update early exit, this will cause these surfaces to freeze since the buffers are never release"
        //     );
        //     return;
        // }
        _ = self
            .part
            .set_material_parameter("opaque", MaterialParameter::Bool(opaque));
        _ = self
            .part
            .set_material_parameter("unlit", MaterialParameter::Bool(true));
        // TODO: fix possible dmatex collision, even if unlikely, probably by just making dmatex a
        // binder object
        let dmatex_id = CLIENT.wait().generate_id();
        _ = import_dmatex_uid(CLIENT.wait(), dmatex_id, dmatex_uid);
        _ = self.part.set_material_parameter(
            "diffuse",
            MaterialParameter::Dmatex(DmatexSubmitInfo {
                dmatex_id,
                acquire_point,
                release_point,
            }),
        )
    }

    async fn toplevel_resized(
        &self,
        _ctx: gluon::Context,
        new_size: stardust_xr_panel_item::protocol::UVec2,
    ) {
        let size = Self::get_size([new_size.x as usize, new_size.y as usize]);
        _ = self.model.set_local_transform(Transform::from_scale(size));
        _ = self.field.set_shape(Shape::Box(size));
    }

    async fn toplevel_max_size(
        &self,
        _ctx: gluon::Context,
        max_size: Option<stardust_xr_panel_item::protocol::UVec2>,
    ) {
    }

    async fn toplevel_min_size(
        &self,
        _ctx: gluon::Context,
        min_size: Option<stardust_xr_panel_item::protocol::UVec2>,
    ) {
    }

    async fn toplevel_fullscreen(&self, _ctx: gluon::Context, _fullscreen_active: bool) {}

    // TODO: maybe impl?
    async fn toplevel_title(&self, _ctx: gluon::Context, _title: String) {}

    // TODO: maybe impl?
    async fn toplevel_app_id(&self, _ctx: gluon::Context, _app_id: String) {}

    async fn set_cursor_visuals(&self, _ctx: gluon::Context, _geometry: Option<Geometry>) {}

    // TODO: impl for subsurfaces
    async fn create_child(&self, _ctx: gluon::Context, _child: ChildState) {}

    async fn move_child(&self, _ctx: gluon::Context, _child_id: u64, _geometry: Geometry) {}

    async fn destroy_child(&self, _ctx: gluon::Context, _child_id: u64) {}
}
