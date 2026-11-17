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
		keymap_id: u64,
	},
	/// pressed is exactly what the client believes is held down
	Focused {
		keymap_id: u64,
		surface: Weak<Surface>,
		pressed: Vec<u32>,
	},
}
impl KeyboardState {
	fn keymap_id(&self) -> Option<u64> {
		match self {
			Self::Fresh => None,
			Self::Unfocused { keymap_id } | Self::Focused { keymap_id, .. } => Some(*keymap_id),
		}
	}
	fn set_keymap_id(&mut self, id: u64) {
		match self {
			Self::Fresh => *self = Self::Unfocused { keymap_id: id },
			Self::Unfocused { keymap_id } | Self::Focused { keymap_id, .. } => *keymap_id = id,
		}
	}
	fn is_focused_on(&self, surface: &Arc<Surface>) -> bool {
		matches!(self, Self::Focused { surface: s, .. } if s.as_ptr() == Arc::as_ptr(surface))
	}
	/// whether focus is on any surface of the toplevel `surface` belongs to, popups and subsurfaces included
	fn is_focused_within(&self, surface: &Surface) -> bool {
		let Self::Focused {
			surface: focused, ..
		} = self
		else {
			return false;
		};
		let Some(focused) = focused.upgrade() else {
			return false;
		};
		let target = surface.toplevel.read().clone();
		target.strong_count() > 0 && Weak::ptr_eq(&focused.toplevel.read(), &target)
	}
	/// the client already forgot about everything on a surface it destroyed, so no leave
	fn drop_dead_focus(&mut self, client: &Client) {
		if let Self::Focused {
			keymap_id, surface, ..
		} = self && alive(client, surface).is_none()
		{
			*self = Self::Unfocused {
				keymap_id: *keymap_id,
			};
		}
	}
}

// panel items can keep a surface's Arc around after the client destroyed it
fn alive(client: &Client, surface: &Weak<Surface>) -> Option<Arc<Surface>> {
	let s = surface.upgrade()?;
	client.holds(s.id, &*s).then_some(s)
}

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Keyboard {
	pub id: ObjectId,
	state: Mutex<KeyboardState>,
}

// a keyboard that never saw a key still needs some keymap before it can be focused,
// and whatever stardust last used is the best guess
static LAST_KEYMAP: parking_lot::Mutex<Option<Keymap>> = parking_lot::Mutex::new(None);

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

	/// false if the store doesn't know this keymap
	async fn ensure_keymap(
		&self,
		client: &mut Client,
		state: &mut KeyboardState,
		keymap: Keymap,
	) -> WaylandResult<bool> {
		let Ok(Some(keymap_id)) = KEYMAP_STORE.wait().get_keymap_id(keymap.clone()).await else {
			tracing::warn!("invalid keymap");
			return Ok(false);
		};
		if state.keymap_id() == Some(keymap_id) {
			return Ok(true);
		}
		let Ok(Some(fd)) = KEYMAP_STORE.wait().get(keymap).await else {
			return Ok(false);
		};
		self.keymap(client, self.id, KeymapFormat::XkbV1, fd.fd.as_fd(), fd.size)
			.await?;
		state.set_keymap_id(keymap_id);
		Ok(true)
	}

	/// held keys carry over, it's still the same physical keyboard
	async fn enter_surface(
		&self,
		client: &mut Client,
		state: &mut KeyboardState,
		surface: &Arc<Surface>,
		[depressed, latched, locked, group]: [u32; 4],
	) -> WaylandResult<()> {
		let Some(keymap_id) = state.keymap_id() else {
			return Ok(());
		};
		let carried = match state {
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
		self.modifiers(client, self.id, serial, depressed, latched, locked, group)
			.await?;

		*state = KeyboardState::Focused {
			keymap_id,
			surface: Arc::downgrade(surface),
			pressed: carried,
		};
		Ok(())
	}

	pub async fn focus(&self, client: &mut Client, surface: Arc<Surface>) -> WaylandResult<()> {
		let mut state = self.state.lock().await;
		state.drop_dead_focus(client);
		if state.is_focused_on(&surface) || !client.holds(surface.id, &*surface) {
			return Ok(());
		}
		if state.keymap_id().is_none() {
			// nothing to describe keys with yet, the first key press focuses instead
			let Some(keymap) = LAST_KEYMAP.lock().clone() else {
				return Ok(());
			};
			if !self.ensure_keymap(client, &mut state, keymap).await? {
				return Ok(());
			}
		}
		// no modifier state without a key event, the next one corrects it
		self.enter_surface(client, &mut state, &surface, [0; 4])
			.await
	}

	/// a toplevel being focused shouldn't yank focus out of its own popups
	pub async fn focus_toplevel(
		&self,
		client: &mut Client,
		toplevel_surface: Arc<Surface>,
	) -> WaylandResult<()> {
		if self.state.lock().await.is_focused_within(&toplevel_surface) {
			return Ok(());
		}
		self.focus(client, toplevel_surface).await
	}

	pub async fn unfocus_toplevel(
		&self,
		client: &mut Client,
		toplevel_surface: &Surface,
	) -> WaylandResult<()> {
		let mut state = self.state.lock().await;
		state.drop_dead_focus(client);
		if !state.is_focused_within(toplevel_surface) {
			return Ok(());
		}
		let KeyboardState::Focused {
			keymap_id, surface, ..
		} = &*state
		else {
			return Ok(());
		};
		let (keymap_id, focused) = (*keymap_id, surface.upgrade());
		if let Some(focused) = focused {
			let serial = client.next_event_serial();
			self.leave(client, self.id, serial, focused.id).await?;
		}
		*state = KeyboardState::Unfocused { keymap_id };
		Ok(())
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
		*LAST_KEYMAP.lock() = Some(keymap.clone());
		let mut state = self.state.lock().await;
		if !self.ensure_keymap(client, &mut state, keymap).await? {
			return Ok(());
		}
		state.drop_dead_focus(client);

		let mods = [
			modifier_state.depressed,
			modifier_state.latched,
			modifier_state.locked,
			modifier_state.layout_group,
		];
		if pressed && !state.is_focused_on(&surface) {
			if !client.holds(surface.id, &*surface) {
				return Ok(());
			}
			self.enter_surface(client, &mut state, &surface, mods)
				.await?;
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

		let [depressed, latched, locked, group] = mods;
		let serial = client.next_event_serial();
		self.modifiers(client, self.id, serial, depressed, latched, locked, group)
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
		client: &mut Self::Connection,
		sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(sender_id);
		Ok(())
	}
}
