use std::{
	collections::HashMap,
	future::ready,
	sync::{
		Arc, OnceLock, Weak,
		atomic::{AtomicBool, Ordering},
	},
};

use anyhow::Context;
use gluon_ipc::{Handler, Interface, Node, RefExt, ToRef};
use mint::{Vector2, Vector3};
use parking_lot::Mutex;
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
	Derezzable, FrameSensitive, UIElement,
	grabbable::{Grabbable, GrabbableSettings, PointerMode},
	lines::arrow,
};
use stardust_xr_panels::{
	panel_item::{
		ChildState, Geometry, PanelItem, PanelShell, PanelShellHandler, Rect, SurfaceId,
		SurfaceUpdateTarget,
	},
	panel_item_acceptor::PanelItemAcceptor,
};
use tokio::sync::{RwLock, broadcast::error::RecvError};

use crate::{
	CLIENT,
	client::Message,
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
}
impl ItemHandlerQuery {
	async fn new(
		toplevel: Weak<Toplevel>,
		seat: Weak<Seat>,
		ref_space: SpatialRef,
		size: impl Into<Vector2<usize>>,
	) -> stardust_xr_fusion::Result<Node<Self>> {
		let (node, query_handler) = PointsQueryHandler::new_node(Self {
			toplevel,
			seat,
			replaced: AtomicBool::new(false),
			handle: OnceLock::new(),
			acceptor: RwLock::new(None),
		})?;
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
			.await??;
		_ = node.handle.set(handle);
		Ok(node)
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
		let Some(obj) = XdgBackend::connect(acceptor, &seat, &toplevel).await else {
			return;
		};
		toplevel.switch_panel_shell(None, obj).await;
		self.replaced.store(true, Ordering::Relaxed);
	}
}
impl PointsQueryHandlerHandler for ItemHandlerQuery {
	async fn entered(
		&self,
		_ctx: gluon_ipc::Context,
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
	}

	fn interfaces_changed(
		&self,
		_ctx: gluon_ipc::Context,
		_id: QueryableId,
		_interfaces: Vec<QueriedInterface>,
	) -> impl Future<Output = ()> + Send + Sync {
		ready(())
	}

	async fn moved(&self, _ctx: gluon_ipc::Context, _id: QueryableId, sample: FieldSample) {
		if let Some(entry) = self.acceptor.write().await.as_mut() {
			entry.1 = sample;
		}
	}

	async fn left(&self, _ctx: gluon_ipc::Context, _id: QueryableId) {
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
	/// unscaled panel space, so children can be laid out in meters
	content: SpatialRef,
	children: RwLock<ChildPanels>,
	input_task: OnceLock<AbortOnDrop>,
	grabbable: RwLock<Grabbable>,
	query: Node<ItemHandlerQuery>,
	acceptor_indicator: Lines,
	showing_indicator: AtomicBool,

	derezzable: Mutex<Derezzable>,
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
// impl Drop for PanelItemUi {
// 	fn drop(&mut self) {
//         tracing::error!("uuuuuuhhhh dropping panel item ui, aaaaaaaaaaa");
// 	}
// }

impl PanelItemUi {
	pub async fn create(
		at: SpatialRef,
		seat: &Arc<Seat>,
		toplevel: &Arc<Toplevel>,
	) -> anyhow::Result<Arc<Node<XdgBackend>>> {
		let client = CLIENT.wait();
		let size = toplevel
			.wl_surface()
			.current_size()
			.unwrap_or([1, 1].into());
		let (root, root_ref) = Spatial::new(client, &at, Transform::IDENTITY).await?;
		root.set_parent_in_place(client.root().clone())?;
		let (field_spatial, field_spatial_ref) =
			Spatial::new(client, &root_ref, Transform::IDENTITY).await?;
		let (field, _) = Field::new(
			client,
			&field_spatial,
			Shape::Box {
				size: Self::get_size(size),
			},
		)
		.await?;
		let grabbable = Grabbable::new(
			client,
			root_ref.clone(),
			Transform::IDENTITY,
			field.clone(),
			GrabbableSettings {
				max_distance: 0.05,
				linear_momentum: None,
				angular_momentum: None,
				pointer_mode: PointerMode::Align,
				containable: true,
				poseable: true,
			},
		)
		.await?;
		field_spatial.set_parent(grabbable.content_parent().spatial_ref().await?)?;
		let (model_spatial, _) = Spatial::new(
			client,
			&field_spatial_ref,
			Transform::from_scale(Self::get_size(size)),
		)
		.await?;
		let (model, part) = panel_model(&model_spatial).await?;
		let content_parent = grabbable.content_parent().clone();
		let derezzable = Derezzable::new(client, content_parent.clone(), field.clone()).await?;
		let acceptor_indicator = Lines::new(client, &field_spatial, vec![]).await?;
		let query = ItemHandlerQuery::new(
			Arc::downgrade(toplevel),
			Arc::downgrade(seat),
			field_spatial_ref.clone(),
			size,
		)
		.await?;
		let (panel_shell_handler, panel_shell) = PanelShell::new_node(Self {
			root,
			model,
			field,
			part,
			content: field_spatial_ref,
			children: RwLock::new(ChildPanels {
				size: [size.x as u32, size.y as u32].into(),
				panels: HashMap::new(),
			}),
			grabbable: RwLock::new(grabbable),
			input_task: OnceLock::new(),
			model_spatial,
			query,
			acceptor_indicator,
			showing_indicator: AtomicBool::new(false),
			derezzable: Mutex::new(derezzable),
		})?;
		// The proxy is dropped: this panel item's shell is in-process, so nothing
		// remote ever reaches the backend and it is only ever used through its
		// handler. Capturing into an acceptor builds a fresh node in
		// `XdgBackend::connect` and hands *that* proxy out.
		let (backend, _backend_proxy) =
			PanelItem::new_node(XdgBackend::new(seat, toplevel, panel_shell.into(), at))?;
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
		Ok(Arc::new(backend))
	}
	async fn update_input(&self, frame_info: FrameInfo) {
		let mut grabbable = self.grabbable.write().await;
		if grabbable.handle_events() {
			grabbable.frame(&frame_info);
		}
		// only try to capture into an acceptor once the user lets go, so
		// dragging the panel through an acceptor's field doesn't snap it in.
		let just_released = grabbable.grab_action().actor_stopped();
		drop(grabbable);
		if just_released {
			self.query.try_capture().await;
		}

		self.refresh_acceptor_indicator().await;

		if self.derezzable.lock().receiver.try_recv().is_ok()
			&& let Some(toplevel) = self.query.toplevel.upgrade()
		{
			let _ = toplevel
				.wl_surface()
				.message_sink
				.send(Message::CloseToplevel(toplevel.clone()));
		}
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

struct ChildModel {
	info: ChildState,
	spatial: Spatial,
	_model: Model,
	part: ModelPart,
}

const PANEL_WIDTH: f32 = 0.1;
const PANEL_THICKNESS: f32 = 0.001;

impl PanelItemUi {
	fn get_size(size: impl Into<Vector2<usize>>) -> Vector3<f32> {
		let size = size.into();
		let width_to_height = size.y as f32 / size.x as f32;

		[PANEL_WIDTH, PANEL_WIDTH * width_to_height, PANEL_THICKNESS].into()
	}

	async fn add_child(&self, info: ChildState) -> anyhow::Result<()> {
		// held while the model loads so the child's first frame waits for it instead of getting dropped
		let mut children = self.children.write().await;
		// other shells drop these too, there's nothing to place it relative to
		if let SurfaceId::Child { id } = info.parent
			&& !children.panels.contains_key(&id)
		{
			anyhow::bail!("child {} has no parent {id}", info.id);
		}
		let (spatial, _) = Spatial::new(CLIENT.wait(), &self.content, Transform::IDENTITY).await?;
		let (model, part) = panel_model(&spatial).await?;
		children.panels.insert(
			info.id,
			ChildModel {
				info,
				spatial,
				_model: model,
				part,
			},
		);
		children.layout();
		Ok(())
	}
}

/// sized together so a layout never mixes a new toplevel size with old geometry
struct ChildPanels {
	/// toplevel size in pixels
	size: Vector2<u32>,
	panels: HashMap<u64, ChildModel>,
}
impl ChildPanels {
	/// geometry is relative to the parent surface, so everything is redone whenever any of it moves
	fn layout(&self) {
		let m = PANEL_WIDTH / self.size.x.max(1) as f32;
		for c in self.panels.values() {
			let (origin, depth) = self.placement(&c.info);
			let g = c.info.geometry.size;
			let x = (origin.x as f32 + g.x as f32 / 2.0 - self.size.x as f32 / 2.0) * m;
			let y = (self.size.y as f32 / 2.0 - origin.y as f32 - g.y as f32 / 2.0) * m;
			_ = c
				.spatial
				.set_local_transform(Transform::from_translation_scale(
					[x, y, PANEL_THICKNESS * depth as f32],
					[g.x as f32 * m, g.y as f32 * m, PANEL_THICKNESS],
				));
		}
	}
	/// pixels from the toplevel's corner, and how many surfaces deep so each level sits in front of its parent
	fn placement(&self, info: &ChildState) -> (Vector2<i32>, u32) {
		let mut origin = info.geometry.origin;
		let mut depth = 1;
		let mut parent = info.parent;
		while let SurfaceId::Child { id } = parent
			&& let Some(p) = self.panels.get(&id)
		{
			origin.x += p.info.geometry.origin.x;
			origin.y += p.info.geometry.origin.y;
			depth += 1;
			parent = p.info.parent;
		}
		(origin, depth)
	}
}

async fn panel_model(spatial: &Spatial) -> anyhow::Result<(Model, ModelPart)> {
	let model = Model::new(
		CLIENT.wait(),
		spatial,
		Resource::Namespaced {
			namespace: crate::APP_ID.into(),
			path: "panel".into(),
		},
	)
	.await?;
	let part = model
		.get_part("Panel")
		.await?
		.context("panel model has no Panel part")?;
	Ok((model, part))
}

async fn show(
	part: &ModelPart,
	dmatex: DmatexRef,
	acquire_point: u64,
	release_point: DmatexSubmitRelease,
	opaque: bool,
	src: Option<Rect>,
) {
	_ = part
		.set_material_parameter("opaque", MaterialParameter::Bool { value: opaque })
		.await;
	_ = part
		.set_material_parameter("unlit", MaterialParameter::Bool { value: true })
		.await;
	let src = src.unwrap_or(Rect {
		origin: [0.0; 2].into(),
		size: [1.0; 2].into(),
	});
	_ = part
		.set_material_parameter("uv_offset", MaterialParameter::Vec2 { value: src.origin })
		.await;
	_ = part
		.set_material_parameter("uv_scale", MaterialParameter::Vec2 { value: src.size })
		.await;
	if let Err(e) = part
		.set_material_parameter(
			"diffuse",
			MaterialParameter::Dmatex {
				dmatex,
				acquire_point,
				release_point,
			},
		)
		.await
	{
		tracing::error!("failed to set panel dmatex: {e}");
	}
}

impl PanelShellHandler for PanelItemUi {
	async fn update_surface_dmatex(
		&self,
		_ctx: gluon_ipc::Context,
		surface: SurfaceUpdateTarget,
		dmatex: DmatexRef,
		acquire_point: u64,
		release_point: DmatexSubmitRelease,
		opaque: bool,
		src: Option<Rect>,
	) {
		match surface {
			SurfaceUpdateTarget::Toplevel => {
				show(
					&self.part,
					dmatex,
					acquire_point,
					release_point,
					opaque,
					src,
				)
				.await
			}
			SurfaceUpdateTarget::Child { id } => {
				let children = self.children.read().await;
				if let Some(c) = children.panels.get(&id) {
					show(&c.part, dmatex, acquire_point, release_point, opaque, src).await
				}
			}
			SurfaceUpdateTarget::Cursor => {}
		}
	}

	async fn toplevel_resized(&self, _ctx: gluon_ipc::Context, new_size: Size2) {
		let size = Self::get_size([new_size.x as usize, new_size.y as usize]);
		_ = self
			.model_spatial
			.set_local_transform(Transform::from_scale(size));
		_ = self.field.set_shape(Shape::Box { size });
		let mut children = self.children.write().await;
		children.size = new_size;
		children.layout();
	}

	async fn toplevel_max_size(&self, _ctx: gluon_ipc::Context, _max_size: Option<Size2>) {}

	async fn toplevel_min_size(&self, _ctx: gluon_ipc::Context, _min_size: Option<Size2>) {}

	async fn toplevel_fullscreen(&self, _ctx: gluon_ipc::Context, _fullscreen_active: bool) {}

	// TODO: maybe impl?
	async fn toplevel_title(&self, _ctx: gluon_ipc::Context, _title: String) {}

	// TODO: maybe impl?
	async fn toplevel_app_id(&self, _ctx: gluon_ipc::Context, _app_id: String) {}

	async fn set_cursor_visuals(&self, _ctx: gluon_ipc::Context, _geometry: Option<Geometry>) {}

	async fn create_child(&self, _ctx: gluon_ipc::Context, child: ChildState) {
		if let Err(e) = self.add_child(child).await {
			tracing::error!("failed to create child panel: {e:#}");
		}
	}

	async fn move_child(&self, _ctx: gluon_ipc::Context, child_id: u64, geometry: Geometry) {
		let mut children = self.children.write().await;
		if let Some(c) = children.panels.get_mut(&child_id) {
			c.info.geometry = geometry;
		}
		children.layout();
	}

	/// takes the whole subtree with it, same as other shells
	async fn destroy_child(&self, _ctx: gluon_ipc::Context, child_id: u64) {
		let panels = &mut self.children.write().await.panels;
		let mut gone = vec![child_id];
		while let Some(id) = gone.pop() {
			panels.remove(&id);
			gone.extend(
				panels
					.iter()
					.filter(|(_, c)| c.info.parent == SurfaceId::Child { id })
					.map(|(k, _)| *k),
			);
		}
	}
}
