use crate::client::Client;
use crate::error::WaylandResult;
use crate::protocols::core::{
    keyboard::Keyboard, pointer::Pointer, surface::Surface, touch::Touch,
};
use mint::Vector2;
use stardust_xr_panel_item::protocol::ScrollSource;
use std::sync::Arc;
use std::sync::OnceLock;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_seat::*;
use waynest_server::Client as _;

#[derive(Debug)]
pub enum SeatMessage {
    AbsolutePointerMotion {
        surface: Arc<Surface>,
        position: Vector2<f32>,
    },
    RelativePointerMotion {
        delta: Vector2<f32>,
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
        keymap_id: u64,
        key: u32,
        pressed: bool,
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
    Reset,
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
            SeatMessage::AbsolutePointerMotion { surface, position } => {
                if let Some(pointer) = self.pointer.get() {
                    pointer
                        .handle_absolute_pointer_motion(client, surface, position)
                        .await?;
                }
            }
            SeatMessage::RelativePointerMotion { delta } => {
                if let Some(pointer) = self.pointer.get() {
                    pointer
                        .handle_relative_pointer_motion(client, delta)
                        .await?;
                }
            }
            SeatMessage::PointerButton {
                surface,
                button,
                pressed,
            } => {
                if let Some(pointer) = self.pointer.get() {
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
                if let Some(pointer) = self.pointer.get() {
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
                if let Some(pointer) = self.pointer.get() {
                    pointer
                        .handle_pointer_scroll_pixels(client, surface, delta, source)
                        .await?;
                }
            }
            SeatMessage::PointerScrollStop { surface } => {
                if let Some(pointer) = self.pointer.get() {
                    pointer.handle_pointer_scroll_stop(client, surface).await?;
                }
            }
            SeatMessage::KeyboardKey {
                surface,
                keymap_id,
                key,
                pressed,
            } => {
                if let Some(keyboard) = self.keyboard.get() {
                    keyboard
                        .handle_keyboard_key(client, surface, keymap_id, key - 8, pressed)
                        .await?;
                }
            }
            SeatMessage::TouchDown {
                surface,
                id,
                position,
            } => {
                if let Some(touch) = self.touch.get() {
                    touch
                        .handle_touch_down(client, surface, id, position)
                        .await?;
                }
            }
            SeatMessage::TouchMove { id, position } => {
                if let Some(touch) = self.touch.get() {
                    touch.handle_touch_move(client, id, position).await?;
                }
            }
            SeatMessage::TouchUp { id } => {
                if let Some(touch) = self.touch.get() {
                    touch.handle_touch_up(client, id).await?;
                }
            }
            SeatMessage::Reset => {
                if let Some(pointer) = self.pointer.get() {
                    pointer.reset(client).await?;
                }
                if let Some(keyboard) = self.keyboard.get() {
                    keyboard.reset(client).await?;
                }
                if let Some(touch) = self.touch.get() {
                    touch.reset(client).await?;
                }
            }
        }
        Ok(())
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
        let touch = client.insert(id, Touch(id))?;
        let _ = self.touch.set(touch);
        Ok(())
    }

    /// https://wayland.app/protocols/wayland#wl_seat:request:release
    async fn release(
        &self,
        _client: &mut Self::Connection,
        _sender_id: ObjectId,
    ) -> WaylandResult<()> {
        Ok(())
    }
}
