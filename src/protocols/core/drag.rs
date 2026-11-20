use super::{
	data_device::{DataDevice, DataOffer, DataSource},
	seat::Devices,
	surface::Surface,
};
use crate::{
	client::Client, error::WaylandResult, protocols::xdg::backend::XdgBackend,
	util::WaylandTime as _,
};
use gluon_ipc::Node;
use mint::Vector2;
use stardust_xr_fusion::types::Timestamp;
use stardust_xr_panels::panel_item::{Geometry, SurfaceUpdateTarget};
use std::sync::{Arc, Weak};
use waynest_protocols::server::core::wayland::{
	wl_data_device::WlDataDevice, wl_data_source::WlDataSource as _,
};

/// drags only ever go between surfaces of the client that started them,
/// there's nothing in 3D yet to carry one over to another app
#[derive(Default)]
pub enum Drag {
	#[default]
	Idle,
	/// the input feeds the drag instead of wl_pointer or wl_touch until it's released
	Dragging {
		input: DragInput,
		/// null for drags that stay inside the client, which then get no offers
		source: Option<Arc<DataSource>>,
		origin: Weak<Surface>,
		focus: Option<DragFocus>,
		icon: Option<DragIcon>,
	},
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragInput {
	Pointer,
	Touch(u32),
}
pub struct DragFocus {
	surface: Weak<Surface>,
	offers: Vec<(Arc<DataDevice>, Option<Arc<DataOffer>>)>,
}

/// stands in for the cursor of whatever window the drag is over,
/// the toolkit already drew it to sit under the pointer
pub struct DragIcon {
	surface: Arc<Surface>,
	/// every wl_surface.offset sent while dragging, the icon's corner sits this far from the pointer
	offset: Vector2<i32>,
	shown_on: Weak<Node<XdgBackend>>,
}
impl DragIcon {
	fn new(surface: Arc<Surface>) -> Self {
		let _ = surface.surface_id.set(SurfaceUpdateTarget::Cursor);
		Self {
			surface,
			offset: [0; 2].into(),
			shown_on: Weak::new(),
		}
	}
	fn show_over(&mut self, over: &Surface) {
		let Some(panel_item) = over.panel_item() else {
			return;
		};
		if self.shown_on.as_ptr() == Arc::as_ptr(&panel_item) {
			return;
		}
		self.hide();
		*self.surface.toplevel.write() = over.toplevel.read().clone();
		self.shown_on = Arc::downgrade(&panel_item);
		self.refresh();
	}
	fn refresh(&self) {
		let Some(panel_item) = self.shown_on.upgrade() else {
			return;
		};
		let size = self.surface.current_size().unwrap_or([16; 2].into());
		set_cursor(
			&panel_item,
			Some(Geometry {
				origin: [-self.offset.x, -self.offset.y].into(),
				size: [size.x as u32, size.y as u32].into(),
			}),
		);
		self.surface.resubmit_buffer();
	}
	/// the client sets its own cursor again once the pointer enters after the drag
	fn hide(&mut self) {
		if let Some(panel_item) = self.shown_on.upgrade() {
			set_cursor(&panel_item, None);
		}
		self.shown_on = Weak::new();
	}
}
impl Drop for DragIcon {
	fn drop(&mut self) {
		self.hide();
	}
}
fn set_cursor(panel_item: &Node<XdgBackend>, geometry: Option<Geometry>) {
	if let Some(shell) = panel_item.panel_shell()
		&& let Err(e) = shell.set_cursor_visuals(geometry)
	{
		tracing::error!("failed to send drag icon to panel shell: {e}");
	}
}

impl Devices {
	pub async fn start_drag(
		&self,
		client: &mut Client,
		source: Option<Arc<DataSource>>,
		origin: Arc<Surface>,
		icon: Option<Arc<Surface>>,
		serial: u32,
	) -> WaylandResult<()> {
		let input = match self.grab_on(client, &origin).await {
			Some(input) if client.issued(serial) => input,
			_ => {
				if let Some(source) = source {
					source.cancel(client).await?;
				}
				return Ok(());
			}
		};
		let mut drag = self.drag.lock().await;
		if matches!(*drag, Drag::Dragging { .. }) {
			if let Some(source) = source {
				source.cancel(client).await?;
			}
			return Ok(());
		}
		match input {
			DragInput::Pointer => {
				for pointer in self.pointers(client) {
					pointer.yield_to_drag(client).await?;
				}
			}
			DragInput::Touch(id) => {
				for touch in self.touches(client) {
					touch.yield_to_drag(id);
				}
			}
		}
		*drag = Drag::Dragging {
			input,
			source,
			origin: Arc::downgrade(&origin),
			focus: None,
			icon: icon.map(|i| {
				let mut icon = DragIcon::new(i);
				icon.show_over(&origin);
				icon
			}),
		};
		Ok(())
	}

	/// the implicit grab a drag has to start from, on any surface of the origin's toplevel
	/// since toolkits pass their toplevel even when the press landed on a subsurface
	async fn grab_on(&self, client: &Client, origin: &Surface) -> Option<DragInput> {
		for pointer in self.pointers(client) {
			if pointer.grabbed_within(origin).await {
				return Some(DragInput::Pointer);
			}
		}
		self.touches(client)
			.iter()
			.find_map(|t| t.point_within(origin))
			.map(DragInput::Touch)
	}

	/// false if no drag took this input, so it should go out as normal
	pub async fn drag_motion(
		&self,
		client: &mut Client,
		input: DragInput,
		surface: Option<Arc<Surface>>,
		position: Vector2<f32>,
		time: Timestamp,
	) -> WaylandResult<bool> {
		let mut drag = self.drag.lock().await;
		let Drag::Dragging {
			input: i,
			source,
			origin,
			focus,
			icon,
		} = &mut *drag
		else {
			return Ok(false);
		};
		if *i != input {
			return Ok(false);
		}
		// touch motion has no surface of its own, it stays relative to where it went down
		let Some(surface) = surface
			.filter(|s| icon.as_ref().is_none_or(|i| !Arc::ptr_eq(&i.surface, s)))
			.or_else(|| focus.as_ref().and_then(|f| f.surface.upgrade()))
			.or_else(|| origin.upgrade())
			.filter(|s| client.holds(s.id, &**s))
		else {
			return Ok(true);
		};
		if let Some(icon) = icon {
			icon.show_over(&surface);
		}

		if let Some(f) = focus
			&& f.surface.as_ptr() == Arc::as_ptr(&surface)
		{
			for (device, _) in &f.offers {
				if client.holds(device.id, &**device) {
					device
						.motion(
							client,
							device.id,
							time.ms(),
							(position.x as f64).into(),
							(position.y as f64).into(),
						)
						.await?;
				}
			}
			return Ok(true);
		}

		if let Some(old) = focus.take() {
			leave(client, old).await?;
		}
		let mut offers = Vec::new();
		for device in client.display()?.data_devices(client) {
			let offer = match source {
				Some(s) => Some(device.offer_drag(client, s.clone()).await?),
				None => None,
			};
			let serial = client.next_event_serial();
			device
				.enter(
					client,
					device.id,
					serial,
					surface.id,
					(position.x as f64).into(),
					(position.y as f64).into(),
					offer.as_ref().map(|o| o.id),
				)
				.await?;
			offers.push((device, offer));
		}
		*focus = Some(DragFocus {
			surface: Arc::downgrade(&surface),
			offers,
		});
		Ok(true)
	}

	/// false if no drag took this input, so it should go out as normal
	pub async fn drag_release(&self, client: &mut Client, input: DragInput) -> WaylandResult<bool> {
		let mut drag = self.drag.lock().await;
		if !matches!(&*drag, Drag::Dragging { input: i, .. } if *i == input) {
			return Ok(false);
		}
		let Drag::Dragging { source, focus, .. } = std::mem::take(&mut *drag) else {
			return Ok(false);
		};
		drop(drag);

		let Some(focus) = focus else {
			if let Some(source) = source {
				source.cancel(client).await?;
			}
			return Ok(true);
		};
		let accepted = source.is_none()
			|| focus
				.offers
				.iter()
				.any(|(_, o)| o.as_ref().is_some_and(|o| o.drag_droppable()));
		if !accepted {
			leave(client, focus).await?;
			if let Some(source) = source {
				source.cancel(client).await?;
			}
			return Ok(true);
		}

		for (device, offer) in &focus.offers {
			if let Some(offer) = offer {
				offer.drag_dropped();
			}
			if client.holds(device.id, &**device) {
				WlDataDevice::drop(&**device, client, device.id).await?;
			}
		}
		if let Some(source) = source
			&& client.holds(source.id, &*source)
			&& source.version >= 3
		{
			source.dnd_drop_performed(client, source.id).await?;
		}
		leave(client, focus).await?;
		Ok(true)
	}

	/// a destroyed source ends its drag without a cancelled, the client already knows
	pub async fn source_destroyed(
		&self,
		client: &mut Client,
		source: &Arc<DataSource>,
	) -> WaylandResult<()> {
		let mut drag = self.drag.lock().await;
		if !matches!(&*drag, Drag::Dragging { source: Some(s), .. } if Arc::ptr_eq(s, source)) {
			return Ok(());
		}
		let Drag::Dragging { focus, .. } = std::mem::take(&mut *drag) else {
			return Ok(());
		};
		drop(drag);
		if let Some(focus) = focus {
			leave(client, focus).await?;
		}
		Ok(())
	}

	pub async fn offset_drag_icon(&self, surface: &Surface, d: Vector2<i32>) {
		let mut drag = self.drag.lock().await;
		if let Drag::Dragging {
			icon: Some(icon), ..
		} = &mut *drag
			&& std::ptr::eq(Arc::as_ptr(&icon.surface), surface)
		{
			icon.offset = [icon.offset.x + d.x, icon.offset.y + d.y].into();
			icon.refresh();
		}
	}

	/// the toplevel's input went away, so a drag from it can't ever see its release
	pub async fn reset_drag(
		&self,
		client: &mut Client,
		toplevel_surface: &Surface,
	) -> WaylandResult<()> {
		let mut drag = self.drag.lock().await;
		let from_here = matches!(&*drag, Drag::Dragging { origin, .. }
			if origin.upgrade().is_none_or(|o| o.same_toplevel(toplevel_surface)));
		if !from_here {
			return Ok(());
		}
		let Drag::Dragging { source, focus, .. } = std::mem::take(&mut *drag) else {
			return Ok(());
		};
		drop(drag);
		if let Some(focus) = focus {
			leave(client, focus).await?;
		}
		if let Some(source) = source {
			source.cancel(client).await?;
		}
		Ok(())
	}
}

async fn leave(client: &mut Client, focus: DragFocus) -> WaylandResult<()> {
	for (device, offer) in focus.offers {
		if let Some(offer) = offer {
			offer.drag_left();
		}
		if client.holds(device.id, &*device) {
			device.leave(client, device.id).await?;
		}
	}
	Ok(())
}
