use super::{buffer::Buffer, callback::Callback};
use crate::{
	client::{Client, Message, MessageSink},
	error::WaylandResult,
	frame_dispatcher::FRAME_EVENT_PROVIDER,
	protocols::{
		core::buffer::BufferSubmit,
		presentation::{MonotonicTimestamp, PresentationFeedback},
		xdg::{backend::XdgBackend, toplevel::Toplevel},
	},
	util::{
		BufferedState, SurfaceCommitAwareBuffer, SurfaceCommitAwareBufferManager,
		registry::Registry,
	},
};
use gluon_ipc::Node;
use mint::Vector2;
use parking_lot::{Mutex, RwLock};
use stardust_xr_panels::panel_item::{Geometry, Rect, SurfaceUpdateTarget};
use std::{
	fmt::Display,
	sync::{Arc, OnceLock, Weak},
};
use tokio::sync::broadcast::error::RecvError;
use tracing::info;
use waynest::ObjectId;
use waynest_protocols::server::{
	core::wayland::{wl_output::Transform, wl_surface::*},
	stable::presentation_time::wp_presentation_feedback::{Kind, WpPresentationFeedback},
};
use waynest_server::Client as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
	Cursor,
	Subsurface,
	XdgToplevel,
	XdgPopup,
}
impl Display for SurfaceRole {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			SurfaceRole::Cursor => f.write_str("SurfaceRole::Cursor"),
			SurfaceRole::Subsurface => f.write_str("SurfaceRole::Subsurface"),
			SurfaceRole::XdgToplevel => f.write_str("SurfaceRole::XdgToplevel"),
			SurfaceRole::XdgPopup => f.write_str("SurfaceRole::XdgPopup"),
		}
	}
}
impl SurfaceRole {
	fn is_xdg(self) -> bool {
		matches!(self, Self::XdgToplevel | Self::XdgPopup)
	}
}

/// a wl_surface keeps its role for life, but the object playing it can come and go,
/// so the same role can be picked back up once the last one retired
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleState {
	Unassigned,
	/// an xdg_surface exists but hasn't made its role object yet,
	/// carrying the xdg role this surface already had if any
	XdgSurface(Option<SurfaceRole>),
	Playing(SurfaceRole),
	Retired(SurfaceRole),
}
impl RoleState {
	fn begin_xdg(self) -> Result<Self, RoleError> {
		match self {
			Self::Unassigned => Ok(Self::XdgSurface(None)),
			Self::Retired(r) if r.is_xdg() => Ok(Self::XdgSurface(Some(r))),
			Self::XdgSurface(_) | Self::Playing(_) => Err(RoleError::Occupied),
			Self::Retired(_) => Err(RoleError::Mismatch),
		}
	}
	fn assign(self, role: SurfaceRole) -> Result<Self, RoleError> {
		match self {
			// a cursor has no role object, so setting it again is just setting it again
			Self::Playing(SurfaceRole::Cursor) if role == SurfaceRole::Cursor => Ok(self),
			Self::Playing(r) if r == role => Err(RoleError::Occupied),
			Self::XdgSurface(prev) if role.is_xdg() && prev.is_none_or(|p| p == role) => {
				Ok(Self::Playing(role))
			}
			Self::Unassigned if !role.is_xdg() => Ok(Self::Playing(role)),
			Self::Retired(r) if r == role && !role.is_xdg() => Ok(Self::Playing(role)),
			_ => Err(RoleError::Mismatch),
		}
	}
	// the xdg_surface outlives its role object, so xdg roles fall back to it
	fn retire(self) -> Self {
		match self {
			Self::Playing(r) if r.is_xdg() => Self::XdgSurface(Some(r)),
			Self::Playing(r) => Self::Retired(r),
			other => other,
		}
	}
	fn end_xdg(self) -> Result<Self, RoleError> {
		match self {
			Self::XdgSurface(Some(r)) => Ok(Self::Retired(r)),
			Self::XdgSurface(None) => Ok(Self::Unassigned),
			Self::Playing(_) => Err(RoleError::Occupied),
			other => Ok(other),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleError {
	/// a role object is still alive on this surface
	Occupied,
	/// the surface already has a different role, and roles are forever
	Mismatch,
}

#[derive(Debug)]
pub struct SurfaceState {
	pub buffer: Option<Arc<Buffer>>,
	pub density: f32,
	pub geometry: Option<Geometry>,
	pub min_size: Option<Vector2<u32>>,
	pub max_size: Option<Vector2<u32>>,

	/// both in surface coords, so buffer pixels / density
	pub viewport_src: Option<Rect>,
	pub viewport_dst: Option<Vector2<u32>>,

	frame_callbacks: Vec<Arc<Callback>>,
}
impl Default for SurfaceState {
	fn default() -> Self {
		Self {
			buffer: Default::default(),
			density: 1.0,
			geometry: None,
			min_size: None,
			max_size: None,
			viewport_src: None,
			viewport_dst: None,
			frame_callbacks: Vec::new(),
		}
	}
}
impl BufferedState for SurfaceState {
	fn apply(&mut self, pending: &mut Self) {
		self.buffer = pending.buffer.clone();
		self.density = pending.density;
		self.geometry = pending.geometry;
		self.min_size = pending.min_size;
		self.max_size = pending.max_size;
		self.viewport_src = pending.viewport_src;
		self.viewport_dst = pending.viewport_dst;
		self.frame_callbacks.append(&mut pending.frame_callbacks);
	}

	fn get_initial_pending(&self) -> Self {
		Self {
			buffer: self.buffer.clone(),
			density: self.density,
			geometry: self.geometry,
			min_size: self.min_size,
			max_size: self.max_size,
			viewport_src: self.viewport_src,
			viewport_dst: self.viewport_dst,
			frame_callbacks: Vec::new(),
		}
	}
}
impl SurfaceState {
	pub fn has_valid_buffer(&self) -> bool {
		self.buffer
			.as_ref()
			.is_some_and(|b| b.size().x > 0 && b.size().y > 0)
	}
	/// in buffer pixels like everything else here, with the viewport's crop and scale applied
	pub fn size(&self) -> Option<Vector2<usize>> {
		let d = self.density;
		if let Some(dst) = self.viewport_dst {
			return Some([(dst.x as f32 * d) as usize, (dst.y as f32 * d) as usize].into());
		}
		if let Some(src) = self.viewport_src {
			return Some([(src.size.x * d) as usize, (src.size.y * d) as usize].into());
		}
		self.buffer.as_ref().map(|b| b.size())
	}
	pub fn uv_src(&self) -> Option<Rect> {
		let src = self.viewport_src?;
		let b = self.buffer.as_ref()?.size();
		let (w, h) = (b.x as f32 / self.density, b.y as f32 / self.density);
		Some(Rect {
			origin: [src.origin.x / w, src.origin.y / h].into(),
			size: [src.size.x / w, src.size.y / h].into(),
		})
	}
}

// if returning false, don't run this callback again... just remove it
pub type OnCommitCallback = Box<dyn FnMut(&Surface) -> bool + Send + Sync>;
// Filter that decides whether to apply pending state. Returns true to allow commit, false to defer.
pub type CommitFilter = Box<dyn Fn() -> bool + Send + Sync>;

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Surface {
	pub id: ObjectId,
	pub surface_id: OnceLock<SurfaceUpdateTarget>,
	state: Arc<Mutex<SurfaceCommitAwareBuffer<SurfaceState>>>,
	pub message_sink: MessageSink,
	role: Mutex<RoleState>,
	// pub panel_item: Mutex<Weak<BinderObject<XdgBackend>>>,
	// pub panel_item: Mutex<Weak<PanelItem>>,
	requires_parent_sync: Mutex<Option<CommitFilter>>,
	on_commit_handlers: Mutex<Vec<OnCommitCallback>>,
	on_updated_current_state_handlers: Mutex<Vec<OnCommitCallback>>,
	presentation_feedback: Mutex<Vec<Arc<PresentationFeedback>>>,
	state_buffer_manager: Arc<SurfaceCommitAwareBufferManager>,
	children: Registry<Surface>,
	parent: Mutex<Weak<Surface>>,
	/// used to store unapplied surface updates, so they can be applied later
	buffered_surface_update: Mutex<Option<BufferSubmit>>,
	current_buffer_submit: Mutex<Option<BufferSubmit>>,
	// TODO: make this async
	pub toplevel: RwLock<Weak<Toplevel>>,
}
impl std::fmt::Debug for Surface {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Surface")
			.field("id", &self.id)
			.field("surface_id", &self.surface_id)
			.field("state", &self.state)
			.field("message_sink", &self.message_sink)
			.field("role", &self.role)
			.field("commit_filter", &self.requires_parent_sync.lock().is_some())
			.field(
				"on_commit_handlers",
				&format!("<{} handlers>", self.on_commit_handlers.lock().len()),
			)
			.field("presentation_feedback", &self.presentation_feedback)
			.finish()
	}
}
impl Surface {
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn new(client: &Client, id: ObjectId) -> WaylandResult<Arc<Self>> {
		let message_sink = client.message_sink()?;
		let surface = Arc::new_cyclic(|surface| {
			let manager = SurfaceCommitAwareBufferManager::new(surface.clone());
			Surface {
				id,
				surface_id: OnceLock::new(),
				state: SurfaceCommitAwareBuffer::new_from_manager(
					Default::default(),
					manager.clone(),
				),
				message_sink,
				role: Mutex::new(RoleState::Unassigned),
				requires_parent_sync: Mutex::new(None),
				on_commit_handlers: Mutex::new(Vec::new()),
				on_updated_current_state_handlers: Mutex::new(Vec::new()),
				presentation_feedback: Mutex::default(),
				state_buffer_manager: manager,
				children: Registry::new(),
				parent: Mutex::new(Weak::new()),
				toplevel: RwLock::new(Weak::new()),
				buffered_surface_update: Mutex::new(None),
				current_buffer_submit: Mutex::new(None),
			}
		});
		surface.add_updated_current_state_handler(|surface| {
			surface.buffer_update();
			true
		});
		tokio::spawn({
			let surface = Arc::downgrade(&surface);
			async move {
				let mut frame_recv = FRAME_EVENT_PROVIDER.subscribe();
				loop {
					let _frame_info = match frame_recv.recv().await {
						Err(RecvError::Closed) => break,
						Err(RecvError::Lagged(v)) => {
							tracing::warn!("Missed {v} frame events");
							continue;
						}
						Ok(v) => v,
					};
					let Some(surface) = surface.upgrade() else {
						break;
					};
					// TODO: add predicted display time to the stardust
					// protocol and dispatch presentation feedback
					surface.frame_event();
				}
			}
		});
		Ok(surface)
	}

	fn transition_role(
		&self,
		f: impl FnOnce(RoleState) -> Result<RoleState, RoleError>,
	) -> Result<(), RoleError> {
		let mut role = self.role.lock();
		*role = f(*role)?;
		Ok(())
	}
	pub fn begin_xdg(&self) -> Result<(), RoleError> {
		self.transition_role(RoleState::begin_xdg)
	}
	pub fn assign_role(&self, role: SurfaceRole) -> Result<(), RoleError> {
		self.transition_role(|s| s.assign(role))
	}
	pub fn retire_role(&self) {
		let mut role = self.role.lock();
		*role = role.retire();
	}
	pub fn end_xdg(&self) -> Result<(), RoleError> {
		self.transition_role(RoleState::end_xdg)
	}
	pub fn plays(&self, role: SurfaceRole) -> bool {
		*self.role.lock() == RoleState::Playing(role)
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn state_lock(
		&self,
	) -> parking_lot::MutexGuard<'_, SurfaceCommitAwareBuffer<SurfaceState>> {
		self.state.lock()
	}
	pub fn currently_has_valid_buffer(&self) -> bool {
		self.state.lock().current().has_valid_buffer()
	}
	pub fn panel_item(&self) -> Option<Arc<Node<XdgBackend>>> {
		self.toplevel.read().upgrade()?.panel_item()
	}

	/// Set a filter that controls whether current state in SurfaceCommitAwareBuffers is updated on
	/// apply.
	/// Only one filter can be set at a time (typically by the surface role).
	/// The filter returns true if the current state needs to be updated on the parents commit.
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn set_parent_syncronized_filter<F: Fn() -> bool + Send + Sync + 'static>(
		&self,
		filter: F,
	) {
		*self.requires_parent_sync.lock() = Some(Box::new(filter));
	}

	/// Remove the parent syncronized filter, allowing normal commit behavior
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn clear_parent_syncronized_filter(&self) {
		*self.requires_parent_sync.lock() = None;
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn add_commit_handler<F: FnMut(&Surface) -> bool + Send + Sync + 'static>(
		&self,
		handler: F,
	) {
		let mut handlers = self.on_commit_handlers.lock();
		handlers.push(Box::new(handler));
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn add_updated_current_state_handler<F: FnMut(&Surface) -> bool + Send + Sync + 'static>(
		&self,
		handler: F,
	) {
		let mut handlers = self.on_updated_current_state_handlers.lock();
		handlers.push(Box::new(handler));
	}

	// #[tracing::instrument(level = "debug", skip_all)]
	// pub fn update_graphics(
	//     &self,
	//     dmatexes: &ImportedDmatexs,
	//     materials: &mut Assets<BevyMaterial>,
	//     images: &mut Assets<Image>,
	// ) {
	//     let Some(buffer) = self.state.lock().current().buffer.clone() else {
	//         return;
	//     };
	//
	//     let material = self.material.get_or_init(|| {
	//         // // Set default shader parameters
	//         // let mut params = mat_wrapper.0.get_all_param_info();
	//         // params.set_vec2("uv_scale", stereokit_rust::maths::Vec2::new(1.0, 1.0));
	//         // params.set_vec2("uv_offset", stereokit_rust::maths::Vec2::new(0.0, 0.0));
	//         // params.set_float("fcFactor", 1.0);
	//         // params.set_float("ripple", 4.0);
	//         // params.set_float("alpha_min", 0.0);
	//         // params.set_float("alpha_max", 1.0);
	//
	//         materials.add(BevyMaterial {
	//             unlit: true,
	//             ..Default::default()
	//         })
	//     });
	//
	//     if let Some(new_tex) = buffer.buffer.update_tex(dmatexes, images) {
	//         let material = materials.get_mut(material).unwrap();
	//         material.base_color_texture.replace(new_tex);
	//         material.alpha_mode = if buffer.buffer.is_transparent() {
	//             AlphaMode::Premultiplied
	//         } else {
	//             AlphaMode::Opaque
	//         };
	//     }
	//
	//     self.apply_surface_materials();
	// }

	#[tracing::instrument("debug", skip_all)]
	pub fn current_size(&self) -> Option<Vector2<usize>> {
		self.state.lock().current().size()
	}
	#[tracing::instrument("debug", skip_all)]
	pub fn current_buffer_usage(&self) -> Option<Arc<Buffer>> {
		self.state.lock().current().buffer.clone()
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub fn add_presentation_feedback(&self, feedback: Arc<PresentationFeedback>) {
		self.presentation_feedback.lock().push(feedback);
	}

	pub fn submit_presentation_feedback(
		self: &Arc<Self>,
		display_timestamp: MonotonicTimestamp,
		refresh_cycle: u64,
	) {
		let _ = self.message_sink.send(Message::SendPresentationFeedback {
			surface: self.clone(),
			display_timestamp,
			refresh_cycle,
		});
	}

	#[tracing::instrument(level = "debug", skip_all)]
	pub async fn send_presentation_feedback(
		&self,
		client: &mut Client,
		display_timestamp: MonotonicTimestamp,
		refresh_cycle: u64,
	) -> WaylandResult<()> {
		let feedbacks = self
			.presentation_feedback
			.lock()
			.drain(..)
			.collect::<Vec<_>>();
		for feedback in feedbacks {
			if let Some(display_id) = client.display()?.output.get().map(|display| display.id) {
				feedback.sync_output(client, feedback.0, display_id).await?;
			}
			let cycle_lo = refresh_cycle as u32;
			let cycle_hi = (refresh_cycle >> 32) as u32;
			feedback
				.presented(
					client,
					feedback.0,
					display_timestamp.secs_hi(),
					display_timestamp.secs_lo(),
					display_timestamp.subsec_nanos(),
					0,
					cycle_hi,
					cycle_lo,
					Kind::empty(),
				)
				.await?;
		}
		Ok(())
	}

	pub fn set_parent(self: &Arc<Self>, parent: &Arc<Surface>) {
		// Copy parent's panel_item to subsurface (like popups do)
		let toplevel = parent.toplevel.read();
		*self.toplevel.write() = toplevel.clone();
		drop(toplevel);

		self.unparent();
		*self.parent.lock() = Arc::downgrade(parent);
		parent.children.add_raw(self);
	}
	pub fn unparent(&self) {
		let old = std::mem::take(&mut *self.parent.lock());
		if let Some(old) = old.upgrade() {
			old.children.remove(self);
		}
	}

	pub fn requires_surface_syncronization(&self) -> bool {
		if !self.plays(SurfaceRole::Subsurface) {
			return false;
		};
		self.requires_parent_sync
			.lock()
			.as_ref()
			.map(|v| v())
			.unwrap_or(false)
	}
	pub fn get_state_buffer_manager(&self) -> Arc<SurfaceCommitAwareBufferManager> {
		self.state_buffer_manager.clone()
	}
	/// popups and subsurfaces count as part of their toplevel
	pub fn same_toplevel(&self, other: &Surface) -> bool {
		let target = other.toplevel.read().clone();
		target.strong_count() > 0 && Weak::ptr_eq(&self.toplevel.read(), &target)
	}
	pub fn parent(&self) -> Option<Arc<Surface>> {
		self.parent.lock().upgrade()
	}
	pub fn update_current_state_recursive(&self) {
		info!("update current state");
		self.state_buffer_manager.update_current();
		self.run_updated_state_handlers();
		for child in self.children.get_valid_contents() {
			if child.requires_surface_syncronization() {
				child.update_current_state_recursive();
			}
		}
	}
}
impl Surface {
	pub(super) fn buffer_update(&self) {
		let (buffer, src) = {
			let state = self.state.lock();
			(state.current().buffer.clone(), state.current().uv_src())
		};
		if let Some(buffer) = buffer
			&& let Some(submit) = buffer.update()
		{
			if let Some(panel_item) = self.panel_item()
				&& let Some(surface_id) = self.surface_id.get()
			{
				if let Some(shell) = panel_item.panel_shell()
					&& let Err(e) = shell.update_surface_dmatex(
						*surface_id,
						submit.dmatex(),
						submit.acquire(),
						submit.release(),
						!buffer.is_transparent(),
						src,
					) {
					tracing::error!("failed to send surface update to panel shell: {e}");
				}
				self.current_buffer_submit.lock().replace(submit);
			} else {
				self.current_buffer_submit.lock().take();
				self.buffered_surface_update.lock().replace(submit);
			}
		}
	}
	pub fn apply_buffered_surface_update_recursive(&self) {
		if let Some(submit) = self.buffered_surface_update.lock().take()
			&& let Some(panel_item) = self.panel_item()
			&& let Some(surface_id) = self.surface_id.get()
		{
			if let Some(shell) = panel_item.panel_shell()
				&& let Err(e) = shell.update_surface_dmatex(
					*surface_id,
					submit.dmatex(),
					submit.acquire(),
					submit.release(),
					!submit.buffer().is_transparent(),
					self.state.lock().current().uv_src(),
				) {
				tracing::error!("failed to send surface update to panel shell: {e}");
			}
			self.current_buffer_submit.lock().replace(submit);
		}
		for child in self.children.get_valid_contents() {
			child.apply_buffered_surface_update_recursive();
		}
	}
	pub fn reapply_buffer_recursive(&self) {
		let submit = self.current_buffer_submit.lock().take();
		if let Some(submit) = submit.and_then(BufferSubmit::reapply) {
			if let Some(panel_item) = self.panel_item()
				&& let Some(surface_id) = self.surface_id.get()
			{
				if let Some(shell) = panel_item.panel_shell()
					&& let Err(e) = shell.update_surface_dmatex(
						*surface_id,
						submit.dmatex(),
						submit.acquire(),
						submit.release(),
						!submit.buffer().is_transparent(),
						self.state.lock().current().uv_src(),
					) {
					tracing::error!("failed to send surface update to panel shell: {e}");
				}
				self.current_buffer_submit.lock().replace(submit);
			} else {
				self.current_buffer_submit.lock().take();
				self.buffered_surface_update.lock().replace(submit);
			}
		}
		for child in self.children.get_valid_contents() {
			child.reapply_buffer_recursive();
		}
	}

	#[tracing::instrument(level = "debug", skip_all)]
	fn frame_event(&self) {
		let callbacks = std::mem::take(&mut self.state_lock().current.frame_callbacks);
		if !callbacks.is_empty() {
			let _ = self.message_sink.send(Message::Frame(callbacks));
		}
	}
	fn on_commit(&self) {
		self.state.lock().apply();
		let mut handlers = self.on_commit_handlers.lock();
		handlers.retain_mut(|f| (f)(self));

		if self.requires_surface_syncronization() {
			self.update_current_state_recursive();
		} else {
			self.run_updated_state_handlers();
		}
	}
	fn run_updated_state_handlers(&self) {
		let mut handlers = self.on_updated_current_state_handlers.lock();
		handlers.retain_mut(|f| (f)(self));
	}
}

impl WlSurface for Surface {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_surface:request:attach
	#[tracing::instrument(level = "debug", skip_all)]
	async fn attach(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		buffer: Option<ObjectId>,
		_x: i32,
		_y: i32,
	) -> WaylandResult<()> {
		self.state.lock().pending.buffer = buffer.and_then(|b| {
			let buffer = client.get::<Buffer>(b)?;
			Some(buffer)
		});
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:damage
	#[tracing::instrument(level = "debug", skip_all)]
	async fn damage(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_x: i32,
		_y: i32,
		_width: i32,
		_height: i32,
	) -> WaylandResult<()> {
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:frame
	#[tracing::instrument(level = "debug", skip_all)]
	async fn frame(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		callback_id: ObjectId,
	) -> WaylandResult<()> {
		let callback = client.insert(callback_id, Callback(callback_id))?;
		self.state.lock().pending.frame_callbacks.push(callback);
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:set_opaque_region
	#[tracing::instrument(level = "debug", skip_all)]
	async fn set_opaque_region(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_region: Option<ObjectId>,
	) -> WaylandResult<()> {
		// nothing we can really do to repaint behind this so ignore it
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:set_input_region
	#[tracing::instrument(level = "debug", skip_all)]
	async fn set_input_region(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_region: Option<ObjectId>,
	) -> WaylandResult<()> {
		// too complicated to implement this for now so who the hell cares
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:commit
	#[tracing::instrument(level = "debug", skip_all)]
	async fn commit(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		tracing::trace!("commit started");
		self.on_commit();

		tracing::trace!("commit done");
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:set_buffer_transform
	#[tracing::instrument(level = "debug", skip_all)]
	async fn set_buffer_transform(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_transform: Transform,
	) -> WaylandResult<()> {
		// we just don't have the output transform or fullscreen at all so this optimization is never needed
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:set_buffer_scale
	#[tracing::instrument(level = "debug", skip_all)]
	async fn set_buffer_scale(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		scale: i32,
	) -> WaylandResult<()> {
		self.state.lock().pending.density = scale as f32;
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:damage_buffer
	#[tracing::instrument(level = "debug", skip_all)]
	async fn damage_buffer(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_x: i32,
		_y: i32,
		_width: i32,
		_height: i32,
	) -> WaylandResult<()> {
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:offset
	#[tracing::instrument(level = "debug", skip_all)]
	async fn offset(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_x: i32,
		_y: i32,
	) -> WaylandResult<()> {
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_surface:request:destroy
	#[tracing::instrument(level = "debug", skip_all)]
	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::{RoleError, RoleState, SurfaceRole::*};

	#[test]
	fn reshown_popup() {
		let s = RoleState::Unassigned.begin_xdg().unwrap();
		let s = s.assign(XdgPopup).unwrap().retire().end_xdg().unwrap();
		assert_eq!(s, RoleState::Retired(XdgPopup));
		let s = s.begin_xdg().unwrap();
		assert_eq!(s.assign(XdgPopup), Ok(RoleState::Playing(XdgPopup)));
		assert_eq!(s.assign(XdgToplevel), Err(RoleError::Mismatch));
	}

	#[test]
	fn live_role_blocks() {
		let s = RoleState::Unassigned.begin_xdg().unwrap();
		assert_eq!(s.begin_xdg(), Err(RoleError::Occupied));
		let s = s.assign(XdgToplevel).unwrap();
		assert_eq!(s.assign(XdgToplevel), Err(RoleError::Occupied));
		assert_eq!(s.end_xdg(), Err(RoleError::Occupied));
		assert_eq!(s.begin_xdg(), Err(RoleError::Occupied));
	}

	#[test]
	fn roles_dont_switch() {
		let sub = RoleState::Unassigned.assign(Subsurface).unwrap();
		assert_eq!(sub.retire().begin_xdg(), Err(RoleError::Mismatch));
		assert_eq!(sub.retire().assign(Subsurface), Ok(sub));
		assert_eq!(sub.retire().assign(Cursor), Err(RoleError::Mismatch));
		let cursor = RoleState::Unassigned.assign(Cursor).unwrap();
		assert_eq!(cursor.assign(Cursor), Ok(cursor));
		assert_eq!(
			RoleState::Unassigned.assign(XdgPopup),
			Err(RoleError::Mismatch)
		);
	}
}
