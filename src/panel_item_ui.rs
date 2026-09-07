use std::{
	future::ready,
	sync::{
		Arc, OnceLock, Weak,
		atomic::{AtomicBool, Ordering},
	},
};

use gluon::{Handler, Interface, Node, RefExt, ToRef};
use mint::{Vector2, Vector3};
use stardust_xr_fusion::{
	client::FrameInfo,
	dmatex::{DmatexRef, DmatexSubmitRelease},
	drawable::{Lines, LinesExt, MaterialParameter, Model, ModelExt, ModelPart},
	fields::{Field, FieldExt, FieldRef, FieldSample, Shape},
	query::{InterfaceDependency, QueriedInterface, QueryableId},
	spatial::{Spatial, SpatialExt, SpatialRef, Transform},
	spatial_query::{
		Point, PointsQuery, PointsQueryHandle, PointsQueryHandler, PointsQueryHandlerHandler,
	},
	types::{Resource, Size2, rgba_linear},
};
use stardust_xr_molecules::{
	FrameSensitive, UIElement,
	grabbable::{Grabbable, GrabbableSettings, PointerMode},
	lines::arrow,
};
use stardust_xr_panel_item::{
	panel_item::{
		ChildState, Geometry, PanelItem, PanelShell, PanelShellHandler, SurfaceUpdateTarget,
	},
	panel_item_acceptor::PanelItemAcceptor,
};
use tokio::sync::{RwLock, broadcast::error::RecvError};

use crate::{
	CLIENT,
	frame_dispatcher::FRAME_EVENT_PROVIDER,
	protocols::{
		core::seat::Seat,
		xdg::{backend::XdgBackend, toplevel::Toplevel},
	},
	util::AbortOnDrop,
};

#[derive(Debug, Handler)]
struct ItemHandlerQuery {
	toplevel: Weak<Toplevel>,
	seat: Weak<Seat>,
	replaced: AtomicBool,
	handle: OnceLock<PointsQueryHandle>,
	/// the acceptor currently in range, along with the field sample the
	/// server pushed for it (distance/gradient/closest_point), kept fresh by
	/// `moved` so it can be visualized without us sampling the field ourselves.
	/// only captured on release.
	acceptor: RwLock<Option<(PanelItemAcceptor, FieldSample)>>,
	/// When true tries once to automatically connect to a PanelItemAcceptor on intersection
	auto_insert: AtomicBool,
}
impl ItemHandlerQuery {
	async fn new(
		toplevel: Weak<Toplevel>,
		seat: Weak<Seat>,
		ref_space: SpatialRef,
		size: impl Into<Vector2<usize>>,
		auto_insert: bool,
	) -> Node<Self> {
		let (node, query_handler) = PointsQueryHandler::new_node(Self {
			toplevel,
			seat,
			replaced: AtomicBool::new(false),
			handle: OnceLock::new(),
			acceptor: RwLock::new(None),
			auto_insert: AtomicBool::new(auto_insert),
		})
		.unwrap();
		tracing::debug!("creating object: {:?}", query_handler.to_ref());
		let handle = CLIENT
			.wait()
			.spatial_query_interface()
			.points_query(PointsQuery {
				handler: query_handler.into(),
				interfaces: vec![InterfaceDependency {
					id: PanelItemAcceptor::ID.into(),
					optional: false,
				}],
				reference_spatial: ref_space,
				points: Self::get_points(size),
			})
			.await
			.unwrap()
			.unwrap();
		node.handle.set(handle).unwrap();
		node
	}
	fn get_points(size: impl Into<Vector2<usize>>) -> Vec<Point> {
		let mut size = PanelItemUi::get_size(size);
		size.z *= 4.0;
		vec![
			Point {
				point: [0.0; 3].into(),
				margin: size.z * 0.5,
			},
			Point {
				point: [size.x * 0.5, size.y * 0.5, 0.0].into(),
				margin: size.z * 0.5,
			},
			Point {
				point: [-size.x * 0.5, size.y * 0.5, 0.0].into(),
				margin: size.z * 0.5,
			},
			Point {
				point: [size.x * 0.5, -size.y * 0.5, 0.0].into(),
				margin: size.z * 0.5,
			},
			Point {
				point: [-size.x * 0.5, -size.y * 0.5, 0.0].into(),
				margin: size.z * 0.5,
			},
		]
	}
}
impl ItemHandlerQuery {
	/// Actually connect to the acceptor currently in range, if any. Should only
	/// be called once the grabbable holding this UI has been released, so that
	/// dragging the panel through an acceptor's field doesn't immediately snap
	/// it in.
	async fn try_capture(&self) {
		let Some((acceptor, _sample)) = self.acceptor.read().await.clone() else {
			return;
		};
		let Some(toplevel) = self.toplevel.upgrade() else {
			tracing::warn!("failed to upgrade toplevel");
			return;
		};
		let Some(seat) = self.seat.upgrade() else {
			tracing::warn!("failed to upgrade seat");
			return;
		};
		tracing::info!("connecting to new panel item acceptor");
		let obj = XdgBackend::connect(acceptor, &seat, &toplevel).await;
		toplevel.switch_panel_shell(obj).await;
		self.replaced.store(true, Ordering::Relaxed);
	}
}
impl PointsQueryHandlerHandler for ItemHandlerQuery {
	async fn entered(
		&self,
		_ctx: gluon::Context,
		_id: QueryableId,
		_field: FieldRef,
		_spatial: SpatialRef,
		interfaces: Vec<QueriedInterface>,
		sample: FieldSample,
	) {
		tracing::info!("entered");
		let v = interfaces
			.into_iter()
			.find(|v| v.interface_id == PanelItemAcceptor::ID);
		if let Some(v) = v {
			let acceptor = PanelItemAcceptor::from_ref(v.interface);
			*self.acceptor.write().await = Some((acceptor, sample));
		}
		if self.auto_insert.swap(false, Ordering::Relaxed) {
			self.try_capture().await;
		}
	}

	fn interfaces_changed(
		&self,
		_ctx: gluon::Context,
		_id: QueryableId,
		_interfaces: Vec<QueriedInterface>,
	) -> impl Future<Output = ()> + Send + Sync {
		ready(())
	}

	async fn moved(&self, _ctx: gluon::Context, _id: QueryableId, sample: FieldSample) {
		if let Some(entry) = self.acceptor.write().await.as_mut() {
			entry.1 = sample;
		}
	}

	async fn left(&self, _ctx: gluon::Context, _id: QueryableId) {
		*self.acceptor.write().await = None;
	}
}

#[derive(Handler)]
pub struct PanelItemUi {
	root: Spatial,
	model: Model,
	model_spatial: Spatial,
	field: Field,
	part: ModelPart,
	input_task: OnceLock<AbortOnDrop>,
	grabbable: RwLock<Grabbable>,
	query: Node<ItemHandlerQuery>,
	acceptor_indicator: Lines,
	showing_indicator: AtomicBool,
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
	pub async fn create(
		at: SpatialRef,
		seat: &Arc<Seat>,
		toplevel: &Arc<Toplevel>,
		auto_insert: bool,
	) -> Arc<Node<XdgBackend>> {
		let client = CLIENT.wait();
		let size = toplevel
			.wl_surface()
			.current_buffer_size()
			.unwrap_or([1, 1].into());
		let (root, root_ref) = Spatial::new(client, &at, Transform::IDENTITY)
			.await
			.unwrap();
		root.set_parent_in_place(client.root().clone()).unwrap();
		let (field_spatial, field_spatial_ref) =
			Spatial::new(client, &root_ref, Transform::IDENTITY)
				.await
				.unwrap();
		let (field, _) = Field::new(
			client,
			&field_spatial,
			Shape::Box {
				size: Self::get_size(size),
			},
		)
		.await
		.unwrap();
		let grabbable = Grabbable::new(
			client,
			root_ref,
			Transform::IDENTITY,
			field.clone(),
			GrabbableSettings {
				max_distance: 0.02,
				linear_momentum: None,
				angular_momentum: None,
				pointer_mode: PointerMode::Align,
			},
		)
		.await
		.unwrap();
		field_spatial
			.set_parent(grabbable.content_parent().spatial_ref().await.unwrap())
			.unwrap();
		let (model_spatial, _) = Spatial::new(
			client,
			&field_spatial_ref,
			Transform::from_scale(Self::get_size(size)),
		)
		.await
		.unwrap();
		let model = Model::new(
			client,
			&model_spatial,
			Resource::Namespaced {
				namespace: crate::APP_ID.into(),
				path: "panel".into(),
			},
		)
		.await
		.unwrap();
		let part = model.get_part("Panel").await.unwrap().unwrap();
		let acceptor_indicator = Lines::new(client, &field_spatial, vec![]).await.unwrap();
		let query = ItemHandlerQuery::new(
			Arc::downgrade(toplevel),
			Arc::downgrade(seat),
			field_spatial_ref,
			size,
			auto_insert,
		)
		.await;
		let (panel_shell_handler, panel_shell) = PanelShell::new_node(Self {
			root,
			model,
			field,
			part,
			grabbable: RwLock::new(grabbable),
			input_task: OnceLock::new(),
			model_spatial,
			query,
			acceptor_indicator,
			showing_indicator: AtomicBool::new(false),
		})
		.unwrap();
		// The proxy is dropped: this panel item's shell is in-process, so nothing
		// remote ever reaches the backend and it is only ever used through its
		// handler. Capturing into an acceptor builds a fresh node in
		// `XdgBackend::connect` and hands *that* proxy out.
		let (backend, _backend_proxy) =
			PanelItem::new_node(XdgBackend::new(seat, toplevel, panel_shell.into(), at)).unwrap();
		let input_task = tokio::spawn({
			let obj = Arc::downgrade(&panel_shell_handler);
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
					if obj.query.replaced.load(Ordering::Relaxed) {
						break;
					}
					// tracing::info!("got frame event");
					obj.update_input(frame_info).await
				}
			}
		});
		_ = panel_shell_handler.input_task.set(input_task.into());
		// The node moves in rather than being parked in a field: that keeps the
		// handler alive without keeping the *node* alive, since a `Node` holds no
		// `Ref` to itself and still dies once the last shell proxy goes.
		tokio::spawn(async move {
			panel_shell_handler.death_notification().await;
			tracing::debug!(
				"dropping panel item ui: {:?}",
				panel_shell_handler.root.to_ref()
			);
		});
		Arc::new(backend)
	}
	async fn update_input(&self, frame_info: FrameInfo) {
		let mut grabbable = self.grabbable.write().await;
		if grabbable.handle_events() {
			grabbable.frame(&frame_info);
		}
		// disable auto insert on grab
		let just_grabbed = grabbable.grab_action().actor_stopped();
		// only try to capture into an acceptor once the user lets go, so
		// dragging the panel through an acceptor's field doesn't snap it in.
		let just_released = grabbable.grab_action().actor_stopped();
		drop(grabbable);
		if just_grabbed {
			self.query.auto_insert.store(false, Ordering::Relaxed);
		}
		if just_released {
			self.query.try_capture().await;
		}

		self.refresh_acceptor_indicator().await;
	}

	/// Draws an arrow toward the in-range acceptor, if any, using the
	/// distance/gradient/closest_point the server already pushed to us via
	/// `entered`/`moved` on the query. No RPC of our own needed here, so this
	/// is cheap enough to run inline in the per-frame grab-handling loop above.
	async fn refresh_acceptor_indicator(&self) {
		match self.query.acceptor.read().await.as_ref() {
			Some((_, sample)) => {
				self.showing_indicator.store(true, Ordering::Relaxed);
				_ = self.acceptor_indicator.set_lines(vec![arrow(
					[0.0, 0.0, 0.0],
					sample.closest_point,
					0.002,
					0.01,
					rgba_linear!(0.2, 1.0, 0.4, 1.0),
				)]);
			}
			None => {
				if self.showing_indicator.swap(false, Ordering::Relaxed) {
					_ = self.acceptor_indicator.set_lines(vec![]);
				}
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
		dmatex: DmatexRef,
		acquire_point: u64,
		release_point: DmatexSubmitRelease,
		opaque: bool,
	) {
		// TODO: remove this when children are implemented
		if !matches!(surface, SurfaceUpdateTarget::Toplevel) {
			tracing::warn!("surface update early exit");
			return;
		}
		_ = self
			.part
			.set_material_parameter("opaque", MaterialParameter::Bool { value: opaque })
			.await;
		_ = self
			.part
			.set_material_parameter("unlit", MaterialParameter::Bool { value: true })
			.await;
		self.part
			.set_material_parameter(
				"diffuse",
				MaterialParameter::Dmatex {
					dmatex,
					acquire_point,
					release_point,
				},
			)
			.await
			.unwrap();
	}

	async fn toplevel_resized(&self, _ctx: gluon::Context, new_size: Size2) {
		let size = Self::get_size([new_size.x as usize, new_size.y as usize]);
		_ = self
			.model_spatial
			.set_local_transform(Transform::from_scale(size));
		_ = self.field.set_shape(Shape::Box { size });
	}

	async fn toplevel_max_size(&self, _ctx: gluon::Context, _max_size: Option<Size2>) {}

	async fn toplevel_min_size(&self, _ctx: gluon::Context, _min_size: Option<Size2>) {}

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
