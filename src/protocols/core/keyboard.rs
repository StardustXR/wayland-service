use dashmap::{DashMap, DashSet};
use memfd::MemfdOptions;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use stardust_xr_fusion::items::panel::get_keymap;
use stardust_xr_panel_item::protocol::KeymapId;
use std::{
    collections::HashSet,
    io::Write,
    os::{
        fd::{AsFd, IntoRawFd},
        unix::io::{FromRawFd, OwnedFd},
    },
    sync::{Arc, LazyLock, Weak},
};
use tokio::sync::{RwLock, RwLockReadGuard};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_keyboard::*;

use crate::{CLIENT, client::Client, error::WaylandResult, protocols::core::surface::Surface};

#[derive(Default)]
struct ModifierState {
    pressed_keys: HashSet<u32>,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
}
pub struct KeymapManager(LazyLock<RwLock<FxHashMap<u64, String>>>);
impl KeymapManager {
    const fn new() -> Self {
        Self(LazyLock::new(RwLock::default))
    }
    pub async fn register(&self, keymap: String) -> KeymapId {
        let id = if let Some((key, _)) = self.0.read().await.iter().find(|(_, v)| *v == &keymap) {
            *key
        } else {
            let id = CLIENT.wait().generate_id();
            self.0.write().await.insert(id, keymap);
            id
        };

        KeymapId { id }
    }
    pub async fn get(&self, id: u64) -> Option<RwLockReadGuard<'_, str>> {
        if let Ok(v) =
            RwLockReadGuard::try_map(self.0.read().await, |v| v.get(&id).map(|s| s.as_str()))
        {
            return Some(v);
        }
        let sd_client = CLIENT.wait();
        tracing::info!("getting keymap from the stardust server");
        if let Ok(keymap_data) = get_keymap(sd_client, id).await {
            self.0.write().await.insert(id, keymap_data);
            return Some(RwLockReadGuard::map(self.0.read().await, |v| {
                v.get(&id).unwrap().as_str()
            }));
        }
        None
    }
}
pub static KEYMAPS: KeymapManager = KeymapManager::new();

impl ModifierState {
    fn update_key(&mut self, key: u32, pressed: bool) -> bool {
        let changed = if pressed {
            self.pressed_keys.insert(key)
        } else {
            self.pressed_keys.remove(&key)
        };

        if changed {
            self.update_modifiers();
        }
        changed
    }

    fn update_modifiers(&mut self) {
        let mut mods = 0;

        // TODO: use the actual keymap lol
        // Update modifier state based on currently pressed keys
        for key in &self.pressed_keys {
            match *key {
                input_event_codes::KEY_LEFTSHIFT!() | input_event_codes::KEY_RIGHTSHIFT!() => {
                    mods |= 1
                }
                input_event_codes::KEY_LEFTCTRL!() | input_event_codes::KEY_RIGHTCTRL!() => {
                    mods |= 4
                }
                input_event_codes::KEY_LEFTALT!() => mods |= 8,
                input_event_codes::KEY_RIGHTALT!() => mods |= 128,
                input_event_codes::KEY_LEFTMETA!() | input_event_codes::KEY_RIGHTMETA!() => {
                    mods |= 64
                }
                input_event_codes::KEY_CAPSLOCK!() => {
                    mods |= 2;
                    self.mods_locked ^= 2;
                }
                _ => {}
            }
        }

        self.mods_depressed = mods;
    }
}

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Keyboard {
    pub id: ObjectId,
    focused_surface: Mutex<Weak<Surface>>,
    modifier_state: Mutex<ModifierState>,
    pressed_keys: DashMap<ObjectId, DashSet<u32>>,
    current_keymap_id: Mutex<u64>,
}

impl Keyboard {
    pub fn new(id: ObjectId) -> Self {
        Self {
            id,
            focused_surface: Mutex::new(Weak::new()),
            modifier_state: Mutex::new(ModifierState::default()),
            pressed_keys: DashMap::default(),
            current_keymap_id: Mutex::new(0),
        }
    }

    async fn send_keymap(&self, client: &mut Client, keymap: &[u8]) -> WaylandResult<()> {
        let mut file = MemfdOptions::default()
            .create("stardust-keymap")?
            .into_file();
        file.set_len(keymap.len() as u64)?;
        file.write_all(keymap)?;
        file.flush()?;

        let fd = unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) };

        // Send keymap to client
        self.keymap(
            client,
            self.id,
            KeymapFormat::XkbV1,
            fd.as_fd(),
            keymap.len() as u32,
        )
        .await?;

        Ok(())
    }

    /// has to be the wayland key, so -8 or whatever
    pub async fn handle_keyboard_key(
        &self,
        client: &mut Client,
        surface: Arc<Surface>,
        keymap_id: u64,
        key: u32,
        pressed: bool,
    ) -> WaylandResult<()> {
        // KEYMAP UPDATES
        {
            let mut old_keymap_id = self.current_keymap_id.lock();

            if *old_keymap_id != keymap_id {
                if let Some(keymap_data) = KEYMAPS.get(keymap_id).await {
                    self.send_keymap(client, keymap_data.as_bytes()).await?;
                }
            };
            *old_keymap_id = keymap_id;
        }

        // PRESSED KEYS UPDATE
        let pressed_keys = self.pressed_keys.entry(surface.id).or_default();
        if pressed {
            pressed_keys.insert(key);
        } else {
            pressed_keys.remove(&key);
        }
        // println!("pressed keys: {:?}", &*pressed_keys);

        // FOCUS UPDATES
        let mut focused = self.focused_surface.lock();
        let mut modifier_state = self.modifier_state.lock();

        let refocus = focused.as_ptr() != Arc::as_ptr(&surface);
        // If we're entering a new surface
        if refocus {
            // Send leave to old surface if it exists and is still alive
            if let Some(old_surface) = focused.upgrade() {
                let serial = client.next_event_serial();
                self.leave(client, self.id, serial, old_surface.id).await?;
                // println!("Left surface {}", old_surface.id);
            }

            // Send enter to new surface
            let serial = client.next_event_serial();
            self.enter(
                client,
                self.id,
                serial,
                surface.id,
                pressed_keys.iter().flat_map(|k| k.to_ne_bytes()).collect(),
            )
            .await?;

            let serial = client.next_event_serial();
            self.modifiers(
                client,
                self.id,
                serial,
                modifier_state.mods_depressed,
                modifier_state.mods_latched,
                modifier_state.mods_locked,
                modifier_state.group,
            )
            .await?;
            // println!("Entered new surface {}", surface.id);

            // Update focused surface
            *focused = Arc::downgrade(&surface);
        }

        // KEY EVENT SENDING
        let serial = client.next_event_serial();
        // println!(
        // 	"Sent key {key} {}",
        // 	if pressed { "pressed" } else { "released" }
        // );
        self.key(
            client,
            self.id,
            serial,
            client.display().creation_time.elapsed().as_millis() as u32, // time
            key,
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
        )
        .await?;

        // MODIFIER UPDATES
        // Update modifier state and send modifiers event if changed
        if modifier_state.update_key(key, pressed) {
            // println!("Update modifiers");
            let serial = client.next_event_serial();
            self.modifiers(
                client,
                self.id,
                serial,
                modifier_state.mods_depressed,
                modifier_state.mods_latched,
                modifier_state.mods_locked,
                modifier_state.group,
            )
            .await?;
        }

        Ok(())
    }

    pub async fn reset(&self, client: &mut Client) -> WaylandResult<()> {
        let mut modifier_state = self.modifier_state.lock();
        modifier_state.pressed_keys.clear();
        modifier_state.mods_depressed = 0;
        modifier_state.mods_latched = 0;
        modifier_state.mods_locked = 0;
        modifier_state.group = 0;

        let serial = client.next_event_serial();
        self.modifiers(
            client,
            self.id,
            serial,
            modifier_state.mods_depressed,
            modifier_state.mods_latched,
            modifier_state.mods_locked,
            modifier_state.group,
        )
        .await
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
