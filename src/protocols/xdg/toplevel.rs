use crate::{client::Client, error::WaylandResult, protocols::core::surface::Surface};

use super::backend::XdgBackend;
use gluon_ipc::Node;
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_panels::panel_item::SurfaceUpdateTarget;
use std::sync::{
	Arc, Weak,
	atomic::{AtomicU64, Ordering},
};
use waynest::ObjectId;
pub use waynest_protocols::server::stable::xdg_shell::xdg_toplevel::*;
use waynest_server::Client as _;

/// the xdg_surface dance a toplevel does before stardust ever sees it,
/// and does again every time the client unmaps it with a null buffer
#[derive(Debug)]
enum Lifecycle {
	/// no configure may be sent until the client's first commit
	AwaitingInitialCommit,
	AwaitingAck,
	Configured,
	/// a panel item is being made, a stale generation means the client unmapped meanwhile
	Mapping {
		generation: u64,
	},
	Mapped {
		panel_item: Arc<Node<XdgBackend>>,
		size: Option<Vector2<usize>>,
	},
}

/// what a commit asks the surface's commit handler to go do
pub enum Commit {
	Nothing,
	Configure,
	Map { generation: u64 },
	Resized(Vector2<usize>),
}

#[derive(Debug, Clone)]
struct ToplevelData {
	parent: Option<u64>,
	app_id: Option<String>,
	title: Option<String>,
	activated: bool,
	fullscreen: bool,
	pub size: Option<Vector2<u32>>,
}
impl Default for ToplevelData {
	fn default() -> Self {
		Self {
			parent: None,
			app_id: None,
			title: None,
			activated: true,
			fullscreen: false,
			size: None,
		}
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Toplevel {
	pub id: ObjectId,
	xdg_surface: Arc<super::surface::Surface>,
	lifecycle: Mutex<Lifecycle>,
	mappings: AtomicU64,
	data: Mutex<ToplevelData>,
}
impl Toplevel {
	pub fn new(
		object_id: ObjectId,
		wl_surface: Arc<Surface>,
		xdg_surface: Arc<super::surface::Surface>,
	) -> Self {
		let _ = wl_surface.surface_id.set(SurfaceUpdateTarget::Toplevel);

		Toplevel {
			id: object_id,
			xdg_surface,
			lifecycle: Mutex::new(Lifecycle::AwaitingInitialCommit),
			mappings: AtomicU64::new(0),
			data: Mutex::new(ToplevelData::default()),
		}
	}

	pub fn commit(&self, has_buffer: bool, size: Option<Vector2<usize>>) -> Commit {
		let mut lifecycle = self.lifecycle.lock();
		match &mut *lifecycle {
			Lifecycle::AwaitingInitialCommit => {
				*lifecycle = Lifecycle::AwaitingAck;
				Commit::Configure
			}
			Lifecycle::Configured if has_buffer => {
				let generation = self.mappings.fetch_add(1, Ordering::Relaxed);
				*lifecycle = Lifecycle::Mapping { generation };
				Commit::Map { generation }
			}
			Lifecycle::Mapping { .. } | Lifecycle::Mapped { .. } if !has_buffer => {
				let unmapped = std::mem::replace(&mut *lifecycle, Lifecycle::AwaitingInitialCommit);
				drop(lifecycle);
				drop(unmapped);
				Commit::Nothing
			}
			Lifecycle::Mapped {
				size: last_size, ..
			} => match size {
				Some(s) if size != *last_size => {
					*last_size = size;
					Commit::Resized(s)
				}
				_ => Commit::Nothing,
			},
			_ => Commit::Nothing,
		}
	}
	pub fn acked(&self) {
		let mut lifecycle = self.lifecycle.lock();
		if matches!(*lifecycle, Lifecycle::AwaitingAck) {
			*lifecycle = Lifecycle::Configured;
		}
	}
	/// hands the panel item back if this mapping went stale, so the caller drops it outside the lock
	pub fn finish_mapping(
		&self,
		generation: u64,
		panel_item: Option<Arc<Node<XdgBackend>>>,
	) -> Result<(), Option<Arc<Node<XdgBackend>>>> {
		let size = self.wl_surface().current_size();
		let mut lifecycle = self.lifecycle.lock();
		if !matches!(*lifecycle, Lifecycle::Mapping { generation: g } if g == generation) {
			return Err(panel_item);
		}
		*lifecycle = match panel_item {
			Some(panel_item) => Lifecycle::Mapped { panel_item, size },
			None => Lifecycle::Configured,
		};
		Ok(())
	}
	fn unmap(&self) {
		let unmapped = std::mem::replace(
			&mut *self.lifecycle.lock(),
			Lifecycle::AwaitingInitialCommit,
		);
		drop(unmapped);
	}

	/// with `replacing`, only swaps if that's still the current panel item,
	/// a late death notice must not clobber a toplevel that was remapped since
	pub async fn switch_panel_shell(
		&self,
		replacing: Option<&Weak<Node<XdgBackend>>>,
		new_item: Arc<Node<XdgBackend>>,
	) {
		let old = match &mut *self.lifecycle.lock() {
			Lifecycle::Mapped { panel_item, .. }
				if replacing.is_none_or(|r| r.as_ptr() == Arc::as_ptr(panel_item)) =>
			{
				// TODO: modify release target point so we can send an actual update
				// without the client having to make a new commit
				std::mem::replace(panel_item, new_item)
			}
			_ => return,
		};
		drop(old);
		self.wl_surface().reapply_buffer_recursive();
	}
	pub fn panel_item(&self) -> Option<Arc<Node<XdgBackend>>> {
		match &*self.lifecycle.lock() {
			Lifecycle::Mapped { panel_item, .. } => Some(panel_item.clone()),
			_ => None,
		}
	}
	pub fn is_mapped(&self) -> bool {
		matches!(*self.lifecycle.lock(), Lifecycle::Mapped { .. })
	}

	pub fn wl_surface(&self) -> &Arc<Surface> {
		&self.xdg_surface.wl_surface
	}

	pub fn title(&self) -> Option<String> {
		self.data.lock().title.clone()
	}
	pub fn app_id(&self) -> Option<String> {
		self.data.lock().app_id.clone()
	}
	pub fn parent(&self) -> Option<u64> {
		self.data.lock().parent
	}

	pub fn set_size(&self, size: Option<Vector2<u32>>) {
		self.data.lock().size = size;
	}

	pub fn set_activated(&self, activated: bool) {
		self.data.lock().activated = activated;
	}

	// Helper to clamp size against constraints
	fn clamp_size(&self, size: Vector2<u32>) -> Vector2<u32> {
		let lock = self.wl_surface().state_lock();
		let state = lock.current();
		let mut clamped = size;

		if let Some(min_size) = state.min_size {
			clamped.x = clamped.x.max(min_size.x);
			clamped.y = clamped.y.max(min_size.y);
		}
		if let Some(max_size) = state.max_size {
			clamped.x = clamped.x.min(max_size.x);
			clamped.y = clamped.y.min(max_size.y);
		}
		clamped
	}

	pub async fn reconfigure(&self, client: &mut Client) -> WaylandResult<()> {
		if matches!(*self.lifecycle.lock(), Lifecycle::AwaitingInitialCommit) {
			return Ok(());
		}
		let data = self.data.lock().clone();

		// Use the explicitly set size, applying constraints
		let size = data.size.map(|s| self.clamp_size(s));

		let mut states = vec![
			State::TiledTop,
			State::TiledLeft,
			State::TiledRight,
			State::TiledBottom,
			if data.fullscreen {
				State::Fullscreen
			} else {
				State::Maximized
			},
		];
		if data.activated {
			states.push(State::Activated);
		}

		self.configure(
			client,
			self.id,
			size.map(|v| v.x as i32).unwrap_or(0),
			size.map(|v| v.y as i32).unwrap_or(0),
			states
				.into_iter()
				.flat_map(|x| (x as u32).to_ne_bytes())
				.collect(),
		)
		.await?;
		self.xdg_surface.reconfigure(client).await?;
		Ok(())
	}
}
impl XdgToplevel for Toplevel {
	type Connection = crate::client::Client;

	async fn set_parent(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		parent: Option<ObjectId>,
	) -> WaylandResult<()> {
		// Handle case where parent is specified
		if let Some(parent) = parent {
			// Per spec: parent must be another xdg_toplevel surface
			if let Some(parent_toplevel) = client.get::<Toplevel>(parent) {
				if !parent_toplevel.is_mapped() {
					// Per spec: parent surfaces must be mapped before being used as a parent
					// Setting an unmapped window as parent should raise a protocol error
					// For now we just unset the parent as a fallback
					self.data.lock().parent.take();
					return Ok(());
				}

				// Per spec: store parent to ensure this surface is stacked above parent
				// and other ancestor surfaces. Used for proper window stacking order.
				self.data
					.lock()
					.parent
					// TODO: figure out an actual uid for this
					.replace(0);
			}
		} else {
			// Per spec: null parent unsets the parent, making this a top-level window
			// This allows converting child windows back to independent top-level windows
			self.data.lock().parent.take();
		}

		Ok(())
	}

	async fn set_title(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		title: String,
	) -> WaylandResult<()> {
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_title(title.clone());
			}
		}
		self.data.lock().title.replace(title);
		Ok(())
	}

	async fn set_app_id(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		app_id: String,
	) -> WaylandResult<()> {
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_app_id(app_id.clone());
			}
		}
		self.data.lock().app_id.replace(app_id);
		Ok(())
	}

	async fn show_window_menu(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_seat: ObjectId,
		_serial: u32,
		_x: i32,
		_y: i32,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn r#move(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_seat: ObjectId,
		_serial: u32,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn resize(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_seat: ObjectId,
		_serial: u32,
		_edges: ResizeEdge,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn set_max_size(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		let size = if width == 0 && height == 0 {
			None
		} else {
			Some([width as u32, height as u32].into())
		};
		self.wl_surface().state_lock().pending.max_size = size;
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_max_size(size);
			}
		}
		Ok(())
	}

	async fn set_min_size(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		let size = if width == 0 && height == 0 {
			None
		} else {
			Some([width as u32, height as u32].into())
		};
		self.wl_surface().state_lock().pending.min_size = size;
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_min_size(size);
			}
		}
		Ok(())
	}

	async fn set_maximized(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn unset_maximized(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn set_fullscreen(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		_output: Option<ObjectId>,
	) -> WaylandResult<()> {
		self.data.lock().fullscreen = true;
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_fullscreen(true);
			}
		}
		self.reconfigure(client).await
	}

	async fn unset_fullscreen(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		self.data.lock().fullscreen = false;
		if let Some(panel_item) = self.panel_item() {
			if let Some(shell) = panel_item.panel_shell() {
				_ = shell.toplevel_fullscreen(false);
			}
		}
		self.reconfigure(client).await
	}

	async fn set_minimized(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		self.unmap();
		self.wl_surface().retire_role();
		Ok(())
	}
}
impl Drop for Toplevel {
	fn drop(&mut self) {
		self.unmap();
	}
}
