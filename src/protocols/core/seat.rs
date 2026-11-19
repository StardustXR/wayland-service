use crate::client::Client;
use crate::error::WaylandResult;
use crate::protocols::{
	core::{
		drag::{Drag, DragInput},
		keyboard::{Keyboard, WlKeyboard as _},
		pointer::Pointer,
		surface::{Surface, SurfaceRole},
		touch::Touch,
	},
	xdg::popup::Popup,
};
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_fusion::keymap::Keymap;
use stardust_xr_fusion::types::Timestamp;
use stardust_xr_panels::panel_item::{ModifierState, ScrollSource};
use std::sync::{Arc, Weak};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_seat::*;
use waynest_server::Client as _;

#[derive(Debug)]
pub enum SeatMessage {
	PointerMotion {
		surface: Arc<Surface>,
		delta: Option<Vector2<f32>>,
		position: Vector2<f32>,
		time: Timestamp,
	},
	PointerButton {
		surface: Arc<Surface>,
		button: u32,
		pressed: bool,
		time: Timestamp,
	},
	PointerScrollDiscrete {
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
		time: Timestamp,
	},
	PointerScrollPixels {
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
		time: Timestamp,
	},
	PointerScrollStop {
		surface: Arc<Surface>,
		time: Timestamp,
	},
	KeyboardKey {
		surface: Arc<Surface>,
		keymap: Keymap,
		key: u32,
		pressed: bool,
		modifier_state: ModifierState,
		time: Timestamp,
	},
	TouchDown {
		surface: Arc<Surface>,
		id: u32,
		position: Vector2<f32>,
		time: Timestamp,
	},
	TouchMove {
		id: u32,
		position: Vector2<f32>,
		time: Timestamp,
	},
	TouchUp {
		id: u32,
		time: Timestamp,
	},
	/// from the panel shell focusing or unfocusing a whole toplevel
	KeyboardFocus {
		surface: Arc<Surface>,
		focused: bool,
	},
	/// the toplevel's input got a new panel item, so anything held inside it is stale
	Reset {
		surface: Arc<Surface>,
	},
}

#[derive(Default, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Seat {
	version: u32,
}
impl Seat {
	pub async fn new(client: &mut Client, id: ObjectId, version: u32) -> WaylandResult<Self> {
		let seat = Self { version };

		if version >= 2 {
			seat.name(client, id, "theonlyseat".into()).await?;
		}

		let capabilities = Capability::Pointer | Capability::Keyboard | Capability::Touch;
		WlSeat::capabilities(&seat, client, id, capabilities).await?;

		Ok(seat)
	}
}

/// every pointer, keyboard and touch a client made, from any of its wl_seats,
/// each one gets the input since each tracks its own focus
#[derive(Default)]
pub struct Devices {
	pointers: Mutex<Vec<Weak<Pointer>>>,
	keyboards: Mutex<Vec<Weak<Keyboard>>>,
	touches: Mutex<Vec<Weak<Touch>>>,
	pub(super) drag: tokio::sync::Mutex<Drag>,
}
impl Devices {
	pub(super) fn pointers(&self, client: &Client) -> Vec<Arc<Pointer>> {
		client.live(&self.pointers, |p| p.id)
	}
	fn keyboards(&self, client: &Client) -> Vec<Arc<Keyboard>> {
		client.live(&self.keyboards, |k| k.id)
	}
	pub(super) fn touches(&self, client: &Client) -> Vec<Arc<Touch>> {
		client.live(&self.touches, |t| t.id)
	}

	pub async fn handle_message(
		&self,
		client: &mut Client,
		message: SeatMessage,
	) -> WaylandResult<()> {
		match message {
			SeatMessage::PointerMotion {
				surface,
				position,
				delta,
				time,
			} => {
				if self
					.drag_motion(
						client,
						DragInput::Pointer,
						Some(surface.clone()),
						position,
						time,
					)
					.await?
				{
					return Ok(());
				}
				for pointer in self.pointers(client) {
					if let Some(delta) = delta {
						pointer
							.handle_relative_pointer_motion(client, delta, time)
							.await?;
					}
					pointer
						.handle_absolute_pointer_motion(client, surface.clone(), position, time)
						.await?;
				}
			}
			SeatMessage::PointerButton {
				surface,
				button,
				pressed,
				time,
			} => {
				// presses mid drag go nowhere, any release ends it
				if matches!(
					*self.drag.lock().await,
					Drag::Dragging {
						input: DragInput::Pointer,
						..
					}
				) {
					if !pressed {
						self.drag_release(client, DragInput::Pointer).await?;
					}
					return Ok(());
				}
				if pressed {
					self.click_focus(client, &surface).await?;
				}
				for pointer in self.pointers(client) {
					pointer
						.handle_pointer_button(client, surface.clone(), button, pressed, time)
						.await?;
				}
			}
			SeatMessage::PointerScrollDiscrete {
				surface,
				delta,
				source,
				time,
			} => {
				for pointer in self.pointers(client) {
					pointer
						.handle_pointer_scroll_discrete(
							client,
							surface.clone(),
							delta,
							source,
							time,
						)
						.await?;
				}
			}
			SeatMessage::PointerScrollPixels {
				surface,
				delta,
				source,
				time,
			} => {
				for pointer in self.pointers(client) {
					pointer
						.handle_pointer_scroll_pixels(client, surface.clone(), delta, source, time)
						.await?;
				}
			}
			SeatMessage::PointerScrollStop { surface, time } => {
				for pointer in self.pointers(client) {
					pointer
						.handle_pointer_scroll_stop(client, surface.clone(), time)
						.await?;
				}
			}
			SeatMessage::KeyboardKey {
				surface,
				keymap,
				key,
				pressed,
				modifier_state,
				time,
			} => {
				let surface = grab_target(surface);
				for keyboard in self.keyboards(client) {
					keyboard
						.handle_keyboard_key(
							client,
							surface.clone(),
							keymap.clone(),
							key,
							pressed,
							modifier_state,
							time,
						)
						.await?;
				}
			}
			SeatMessage::TouchDown {
				surface,
				id,
				position,
				time,
			} => {
				self.click_focus(client, &surface).await?;
				for touch in self.touches(client) {
					touch
						.handle_touch_down(client, surface.clone(), id, position, time)
						.await?;
				}
			}
			SeatMessage::TouchMove { id, position, time } => {
				if self
					.drag_motion(client, DragInput::Touch(id), None, position, time)
					.await?
				{
					return Ok(());
				}
				for touch in self.touches(client) {
					touch.handle_touch_move(client, id, position, time).await?;
				}
			}
			SeatMessage::TouchUp { id, time } => {
				if self.drag_release(client, DragInput::Touch(id)).await? {
					return Ok(());
				}
				for touch in self.touches(client) {
					touch.handle_touch_up(client, id, time).await?;
				}
			}
			SeatMessage::KeyboardFocus { surface, focused } => {
				for keyboard in self.keyboards(client) {
					if focused {
						keyboard
							.focus_toplevel(client, grab_target(surface.clone()))
							.await?;
					} else {
						keyboard.unfocus_toplevel(client, &surface).await?;
					}
				}
			}
			SeatMessage::Reset { surface } => {
				self.reset_drag(client, &surface).await?;
				for pointer in self.pointers(client) {
					pointer.reset(client, &surface).await?;
				}
				for keyboard in self.keyboards(client) {
					keyboard.reset(client, &surface).await?;
				}
				for touch in self.touches(client) {
					touch.reset(client, &surface).await?;
				}
			}
		}
		Ok(())
	}

	/// subsurfaces are part of their parent, focus goes to the popup or toplevel they belong to
	async fn click_focus(&self, client: &mut Client, surface: &Arc<Surface>) -> WaylandResult<()> {
		let mut target = surface.clone();
		while target.plays(SurfaceRole::Subsurface)
			&& let Some(parent) = target.parent()
		{
			target = parent;
		}
		let target = grab_target(target);
		for keyboard in self.keyboards(client) {
			keyboard.focus(client, target.clone()).await?;
		}
		Ok(())
	}

	pub async fn keyboard_focused(&self, client: &Client) -> bool {
		for keyboard in self.keyboards(client) {
			if keyboard.is_focused(client).await {
				return true;
			}
		}
		false
	}

	/// for grabs starting or ending, only matters if the toplevel had focus to begin with
	pub async fn refocus_within(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
	) -> WaylandResult<()> {
		for keyboard in self.keyboards(client) {
			keyboard.focus_within(client, surface.clone()).await?;
		}
		Ok(())
	}
}

/// the topmost grabbing popup keeps keyboard focus over the rest of its toplevel
fn grab_target(surface: Arc<Surface>) -> Arc<Surface> {
	Popup::topmost_grab(&surface).unwrap_or(surface)
}
impl WlSeat for Seat {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_seat:request:get_pointer
	async fn get_pointer(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		let pointer = client.insert(id, Pointer::new(id, self.version))?;
		client
			.display()?
			.devices
			.pointers
			.lock()
			.push(Arc::downgrade(&pointer));
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_seat:request:get_keyboard
	async fn get_keyboard(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		let keyboard = client.insert(id, Keyboard::new(id))?;
		// stardust doesn't repeat keys itself, so clients have to
		if self.version >= 4 {
			keyboard.repeat_info(client, id, 25, 600).await?;
		}
		client
			.display()?
			.devices
			.keyboards
			.lock()
			.push(Arc::downgrade(&keyboard));
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_seat:request:get_touch
	async fn get_touch(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		let touch = client.insert(id, Touch::new(id))?;
		client
			.display()?
			.devices
			.touches
			.lock()
			.push(Arc::downgrade(&touch));
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_seat:request:release
	async fn release(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(sender_id);
		Ok(())
	}
}
