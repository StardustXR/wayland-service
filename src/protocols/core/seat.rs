use crate::client::Client;
use crate::error::WaylandResult;
use crate::protocols::core::{
	keyboard::{Keyboard, WlKeyboard as _},
	pointer::Pointer,
	surface::{Surface, SurfaceRole},
	touch::Touch,
};
use mint::Vector2;
use stardust_xr_fusion::keymap::Keymap;
use stardust_xr_panels::panel_item::{ModifierState, ScrollSource};
use std::sync::Arc;
use std::sync::OnceLock;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_seat::*;
use waynest_server::Client as _;

#[derive(Debug)]
pub enum SeatMessage {
	PointerMotion {
		surface: Arc<Surface>,
		delta: Option<Vector2<f32>>,
		position: Vector2<f32>,
	},
	PointerButton {
		surface: Arc<Surface>,
		button: u32,
		pressed: bool,
	},
	PointerScrollDiscrete {
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
	},
	PointerScrollPixels {
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
	},
	PointerScrollStop {
		surface: Arc<Surface>,
	},
	KeyboardKey {
		surface: Arc<Surface>,
		keymap: Keymap,
		key: u32,
		pressed: bool,
		modifier_state: ModifierState,
	},
	TouchDown {
		surface: Arc<Surface>,
		id: u32,
		position: Vector2<f32>,
	},
	TouchMove {
		id: u32,
		position: Vector2<f32>,
	},
	TouchUp {
		id: u32,
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
	pointer: OnceLock<Arc<Pointer>>,
	keyboard: OnceLock<Arc<Keyboard>>,
	touch: OnceLock<Arc<Touch>>,
}

impl Seat {
	pub async fn new(client: &mut Client, id: ObjectId, version: u32) -> WaylandResult<Self> {
		let seat = Self {
			version,
			pointer: OnceLock::new(),
			keyboard: OnceLock::new(),
			touch: OnceLock::new(),
		};

		if version >= 2 {
			seat.name(client, id, "theonlyseat".into()).await?;
		}

		tracing::debug!("Advertising seat capabilities with id {}", id);
		let capabilities = Capability::Pointer | Capability::Keyboard | Capability::Touch;
		WlSeat::capabilities(&seat, client, id, capabilities).await?;
		tracing::debug!("Capabilities advertised: {:?}", capabilities);

		Ok(seat)
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
			} => {
				if let Some(pointer) = self.live_pointer(client) {
					if let Some(delta) = delta {
						pointer
							.handle_relative_pointer_motion(client, delta)
							.await?;
					}
					pointer
						.handle_absolute_pointer_motion(client, surface, position)
						.await?;
				}
			}
			SeatMessage::PointerButton {
				surface,
				button,
				pressed,
			} => {
				if pressed {
					self.click_focus(client, &surface).await?;
				}
				if let Some(pointer) = self.live_pointer(client) {
					pointer
						.handle_pointer_button(client, surface, button, pressed)
						.await?;
				}
			}
			SeatMessage::PointerScrollDiscrete {
				surface,
				delta,
				source,
			} => {
				if let Some(pointer) = self.live_pointer(client) {
					pointer
						.handle_pointer_scroll_discrete(client, surface, delta, source)
						.await?;
				}
			}
			SeatMessage::PointerScrollPixels {
				surface,
				delta,
				source,
			} => {
				if let Some(pointer) = self.live_pointer(client) {
					pointer
						.handle_pointer_scroll_pixels(client, surface, delta, source)
						.await?;
				}
			}
			SeatMessage::PointerScrollStop { surface } => {
				if let Some(pointer) = self.live_pointer(client) {
					pointer.handle_pointer_scroll_stop(client, surface).await?;
				}
			}
			SeatMessage::KeyboardKey {
				surface,
				keymap,
				key,
				pressed,
				modifier_state,
			} => {
				if let Some(keyboard) = self.live_keyboard(client) {
					keyboard
						.handle_keyboard_key(client, surface, keymap, key, pressed, modifier_state)
						.await?;
				}
			}
			SeatMessage::TouchDown {
				surface,
				id,
				position,
			} => {
				self.click_focus(client, &surface).await?;
				if let Some(touch) = self.live_touch(client) {
					touch
						.handle_touch_down(client, surface, id, position)
						.await?;
				}
			}
			SeatMessage::TouchMove { id, position } => {
				if let Some(touch) = self.live_touch(client) {
					touch.handle_touch_move(client, id, position).await?;
				}
			}
			SeatMessage::TouchUp { id } => {
				if let Some(touch) = self.live_touch(client) {
					touch.handle_touch_up(client, id).await?;
				}
			}
			SeatMessage::KeyboardFocus { surface, focused } => {
				if let Some(keyboard) = self.live_keyboard(client) {
					if focused {
						keyboard.focus_toplevel(client, surface).await?;
					} else {
						keyboard.unfocus_toplevel(client, &surface).await?;
					}
				}
			}
			SeatMessage::Reset { surface } => {
				if let Some(pointer) = self.live_pointer(client) {
					pointer.reset(client, &surface).await?;
				}
				if let Some(keyboard) = self.live_keyboard(client) {
					keyboard.reset(client, &surface).await?;
				}
				if let Some(touch) = self.live_touch(client) {
					touch.reset(client, &surface).await?;
				}
			}
		}
		Ok(())
	}

	/// subsurfaces are part of their parent, focus goes to the popup or toplevel they belong to
	async fn click_focus(&self, client: &mut Client, surface: &Arc<Surface>) -> WaylandResult<()> {
		let Some(keyboard) = self.live_keyboard(client) else {
			return Ok(());
		};
		let mut target = surface.clone();
		while target.plays(SurfaceRole::Subsurface)
			&& let Some(parent) = target.parent()
		{
			target = parent;
		}
		keyboard.focus(client, target).await
	}
	fn live_pointer(&self, client: &Client) -> Option<&Arc<Pointer>> {
		self.pointer.get().filter(|p| client.holds(p.id, &***p))
	}
	fn live_keyboard(&self, client: &Client) -> Option<&Arc<Keyboard>> {
		self.keyboard.get().filter(|k| client.holds(k.id, &***k))
	}
	fn live_touch(&self, client: &Client) -> Option<&Arc<Touch>> {
		self.touch.get().filter(|t| client.holds(t.id, &***t))
	}

	pub async fn cursor_surface(&self) -> Option<Arc<Surface>> {
		self.pointer.get()?.cursor_surface().await
	}
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
		let _ = self.pointer.set(pointer);
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_seat:request:get_keyboard
	async fn get_keyboard(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		tracing::info!("Getting keyboard");
		let keyboard = client.insert(id, Keyboard::new(id))?;
		// stardust doesn't repeat keys itself, so clients have to
		if self.version >= 4 {
			keyboard.repeat_info(client, id, 25, 600).await?;
		}
		let _ = self.keyboard.set(keyboard);
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
		let _ = self.touch.set(touch);
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
