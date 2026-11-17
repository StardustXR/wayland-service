use stardust_xr_fusion::keymap::Keymap;
use stardust_xr_panels::panel_item::ModifierState;
use std::{
	os::fd::AsFd,
	sync::{Arc, Weak},
};
use tokio::sync::Mutex;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_keyboard::*;
use waynest_server::Client as _;

use crate::{
	KEYMAP_STORE, client::Client, error::WaylandResult, protocols::core::surface::Surface,
};

/// a wl_keyboard is one physical keyboard as far as the client can tell,
/// so held keys follow focus around instead of belonging to a surface
enum KeyboardState {
	/// no keymap sent yet, the client can't make sense of anything else until then
	Fresh,
	Unfocused {
		keymap: u64,
	},
	/// pressed is exactly what the client believes is held down
	Focused {
		keymap: u64,
		surface: Weak<Surface>,
		pressed: Vec<u32>,
	},
}
impl KeyboardState {
	fn keymap(&self) -> Option<u64> {
		match self {
			Self::Fresh => None,
			Self::Unfocused { keymap } | Self::Focused { keymap, .. } => Some(*keymap),
		}
	}
	fn set_keymap(&mut self, id: u64) {
		match self {
			Self::Fresh => *self = Self::Unfocused { keymap: id },
			Self::Unfocused { keymap } | Self::Focused { keymap, .. } => *keymap = id,
		}
	}
	fn is_focused_on(&self, surface: &Arc<Surface>) -> bool {
		matches!(self, Self::Focused { surface: s, .. } if s.as_ptr() == Arc::as_ptr(surface))
	}
	/// the client already forgot about everything on a surface it destroyed, so no leave
	fn drop_dead_focus(&mut self, client: &Client) {
		if let Self::Focused {
			keymap, surface, ..
		} = self && alive(client, surface).is_none()
		{
			*self = Self::Unfocused { keymap: *keymap };
		}
	}
}

// panel items can keep a surface's Arc around after the client destroyed it
fn alive(client: &Client, surface: &Weak<Surface>) -> Option<Arc<Surface>> {
	let s = surface.upgrade()?;
	client.get::<Surface>(s.id).filter(|c| Arc::ptr_eq(c, &s))
}

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Keyboard {
	pub id: ObjectId,
	state: Mutex<KeyboardState>,
}

impl Keyboard {
	pub fn new(id: ObjectId) -> Self {
		Self {
			id,
			state: Mutex::new(KeyboardState::Fresh),
		}
	}

	fn time(client: &Client) -> WaylandResult<u32> {
		Ok(client.display()?.creation_time.elapsed().as_millis() as u32)
	}

	/// has to be the wayland key, so -8 or whatever
	pub async fn handle_keyboard_key(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		keymap: Keymap,
		key: u32,
		pressed: bool,
		modifier_state: ModifierState,
	) -> WaylandResult<()> {
		let Ok(Some(keymap_id)) = KEYMAP_STORE.wait().get_keymap_id(keymap.clone()).await else {
			tracing::warn!("invalid keymap");
			return Ok(());
		};
		let mut state = self.state.lock().await;
		if state.keymap() != Some(keymap_id) {
			let Ok(Some(fd)) = KEYMAP_STORE.wait().get(keymap).await else {
				return Ok(());
			};
			self.keymap(client, self.id, KeymapFormat::XkbV1, fd.fd.as_fd(), fd.size)
				.await?;
			state.set_keymap(keymap_id);
		}
		state.drop_dead_focus(client);

		if pressed && !state.is_focused_on(&surface) {
			let carried = match &mut *state {
				KeyboardState::Focused {
					surface: old,
					pressed,
					..
				} => {
					let old = old.upgrade().map(|s| s.id);
					let carried = std::mem::take(pressed);
					if let Some(old) = old {
						let serial = client.next_event_serial();
						self.leave(client, self.id, serial, old).await?;
					}
					carried
				}
				_ => Vec::new(),
			};

			let serial = client.next_event_serial();
			self.enter(
				client,
				self.id,
				serial,
				surface.id,
				carried.iter().flat_map(|k| k.to_ne_bytes()).collect(),
			)
			.await?;
			let serial = client.next_event_serial();
			self.modifiers(
				client,
				self.id,
				serial,
				modifier_state.depressed,
				modifier_state.latched,
				modifier_state.locked,
				modifier_state.layout_group,
			)
			.await?;

			*state = KeyboardState::Focused {
				keymap: keymap_id,
				surface: Arc::downgrade(&surface),
				pressed: carried,
			};
		}

		let KeyboardState::Focused { pressed: held, .. } = &mut *state else {
			return Ok(());
		};
		if pressed {
			if held.contains(&key) {
				return Ok(());
			}
			held.push(key);
		} else {
			let Some(i) = held.iter().position(|k| *k == key) else {
				return Ok(());
			};
			held.swap_remove(i);
		}

		let serial = client.next_event_serial();
		let time = Self::time(client)?;
		self.key(
			client,
			self.id,
			serial,
			time,
			key,
			if pressed {
				KeyState::Pressed
			} else {
				KeyState::Released
			},
		)
		.await?;

		let serial = client.next_event_serial();
		self.modifiers(
			client,
			self.id,
			serial,
			modifier_state.depressed,
			modifier_state.latched,
			modifier_state.locked,
			modifier_state.layout_group,
		)
		.await?;

		Ok(())
	}

	pub async fn reset(&self, client: &mut Client) -> WaylandResult<()> {
		let mut state = self.state.lock().await;
		state.drop_dead_focus(client);
		let KeyboardState::Focused { pressed, .. } = &mut *state else {
			return Ok(());
		};

		for key in std::mem::take(pressed) {
			let serial = client.next_event_serial();
			let time = Self::time(client)?;
			self.key(client, self.id, serial, time, key, KeyState::Released)
				.await?;
		}
		let serial = client.next_event_serial();
		self.modifiers(client, self.id, serial, 0, 0, 0, 0).await
	}
}

impl WlKeyboard for Keyboard {
	type Connection = Client;

	/// https://wayland.app/protocols/wayland#wl_keyboard:request:release
	async fn release(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		Ok(())
	}
}
