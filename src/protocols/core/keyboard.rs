use dashmap::{DashMap, DashSet};
use memfd::MemfdOptions;
use stardust_xr_fusion::keymap::Keymap;
use stardust_xr_panel_item::panel_item::ModifierState;
use std::{
    io::Write,
    os::{
        fd::{AsFd, IntoRawFd},
        unix::io::{FromRawFd, OwnedFd},
    },
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, RwLock};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_keyboard::*;

use crate::{
    KEYMAP_STORE, client::Client, error::WaylandResult, protocols::core::surface::Surface,
};

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Keyboard {
    pub id: ObjectId,
    focused_surface: Mutex<Weak<Surface>>,
    pressed_keys: DashMap<ObjectId, DashSet<u32>>,
    // TODO: maybe just store a hash here to not keep the handle alive?
    current_keymap_id: RwLock<Option<Keymap>>,
}

impl Keyboard {
    pub fn new(id: ObjectId) -> Self {
        Self {
            id,
            focused_surface: Mutex::new(Weak::new()),
            pressed_keys: DashMap::default(),
            current_keymap_id: RwLock::new(None),
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
        keymap: Keymap,
        key: u32,
        pressed: bool,
        modifier_state: ModifierState,
    ) -> WaylandResult<()> {
        if self
            .current_keymap_id
            .read()
            .await
            .as_ref()
            .is_none_or(|v| v != &keymap)
        {
            let Ok(Some(fd)) = KEYMAP_STORE.wait().get(keymap.clone()).await else {
                return Ok(());
            };
            self.keymap(client, self.id, KeymapFormat::XkbV1, fd.fd.as_fd(), fd.size)
                .await?;
            self.current_keymap_id.write().await.replace(keymap);
        };

        // PRESSED KEYS UPDATE
        let pressed_keys = self.pressed_keys.entry(surface.id).or_default();
        if pressed {
            pressed_keys.insert(key);
        } else {
            pressed_keys.remove(&key);
        }
        // println!("pressed keys: {:?}", &*pressed_keys);

        // FOCUS UPDATES
        let mut focused = self.focused_surface.lock().await;

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
                modifier_state.depressed,
                modifier_state.latched,
                modifier_state.locked,
                0,
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

        // println!("Update modifiers");
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
        if self.current_keymap_id.read().await.is_none() {
            return Ok(());
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
