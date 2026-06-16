use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::{net::UnixStream, sync::mpsc};
use tokio_stream::StreamExt as _;
use tracing::debug_span;
use waynest::ObjectId;
use waynest_protocols::server::core::wayland::wl_display::WlDisplay as _;
use waynest_server::{Client as _, Listener};

use crate::{
    client::{Client, Message},
    display::Display,
    error::{WaylandError, WaylandResult},
    util::AbortOnDrop,
};

pub struct Wayland {
    _lockfile: File,
    _abort_handle: AbortOnDrop,
    socket_path: PathBuf,
    lock_path: PathBuf,
}
impl Wayland {
    pub fn new(socket_path: &Path) -> WaylandResult<Self> {
        let (socket_path, _lockfile, lock_path) = create_socket(socket_path).ok_or(
            WaylandError::Io(std::io::ErrorKind::AddrNotAvailable.into()),
        )?;
        let listener = waynest_server::Listener::new_with_path(&socket_path).unwrap();
        let socket_path = listener.socket_path().to_path_buf();
        let _abort_handle = tokio::spawn(
            // || "Wayland socket accept loop",
            Self::handle_wayland_loop(listener),
        )
        .into();

        Ok(Self {
            _lockfile,
            _abort_handle,
            socket_path,
            lock_path,
        })
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
    async fn handle_wayland_loop(mut listener: Listener) -> WaylandResult<()> {
        let mut clients = Vec::new();
        loop {
            if let Ok(Some(stream)) = listener.try_next().await {
                debug_span!("Accept wayland client").in_scope(|| {
                    if let Ok(client) = WaylandClient::from_stream(stream) {
                        clients.push(client);
                    }
                });
            }
            clients.retain(|client| !client.abort_handle.is_finished());
        }

        #[allow(unreachable_code)]
        Ok(())
    }
}
impl Drop for Wayland {
    fn drop(&mut self) {
        let mut lock_name = self.socket_path.file_name().unwrap().to_os_string();
        lock_name.push(".lock");
        fs::remove_file(&self.socket_path).unwrap();
        fs::remove_file(self.socket_path.with_file_name(lock_name)).unwrap();
    }
}

fn create_socket(socket_path: &Path) -> Option<(PathBuf, File, PathBuf)> {
    let socket_path = if socket_path.is_relative() {
        directories::BaseDirs::new()
            .unwrap()
            .runtime_dir()
            .unwrap()
            .join(socket_path)
    } else {
        socket_path.to_path_buf()
    };
    let mut lock_name = socket_path.file_name().unwrap().to_os_string();
    lock_name.push(".lock");
    let lock_path = socket_path.with_file_name(lock_name);
    let lock_file = File::create(&lock_path).ok()?;
    lock_file.try_lock().ok()?;
    Some((socket_path, lock_file, lock_path))
}

struct WaylandClient {
    abort_handle: AbortOnDrop,
}
impl WaylandClient {
    pub fn from_stream(socket: UnixStream) -> WaylandResult<Self> {
        let pid = socket.peer_cred().ok().and_then(|c| c.pid());
        let exe = pid.and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok());

        let mut client = Client::new(socket)?;
        let (message_sink, message_source) = mpsc::unbounded_channel();

        client.insert(ObjectId::DISPLAY, Display::new(message_sink, pid))?;

        let pid_printable = pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "??".to_string());
        let exe_printable = exe
            .and_then(|exe| {
                exe.file_name()
                    .and_then(|exe| exe.to_str())
                    .map(|exe| exe.to_string())
            })
            .unwrap_or_else(|| "??".to_string());
        tracing::info!("Wayland client \"{exe_printable}\" connected, pid={pid_printable}");
        let abort_handle = tokio::spawn(
            // || format!("Wayland client \"{exe_printable}\" dispatch, pid={pid_printable}"),
            Self::dispatch_loop(client, message_source),
        )
        .into();

        // let abort_handle = tokio::spawn(async {}).into();

        Ok(WaylandClient { abort_handle })
    }

    async fn dispatch_loop(
        mut client: Client,
        mut render_message_rx: mpsc::UnboundedReceiver<Message>,
    ) -> WaylandResult<()> {
        loop {
            tokio::select! {
                biased;
                // send all queued up messages
                msg = render_message_rx.recv() => {
                    let Some(msg) = msg else {
                        // Render message channel closed, end the dispatch loop
                        return Ok(());
                    };
                    Self::handle_render_message(&mut client, msg).await?;
                }
                // handle the next message
                msg = client.try_next() => {
                    let Some(mut msg) = msg? else {
                        // Client disconnected, end the dispatch loop
                        return Ok(());
                    };
                    let msg_clone = msg.clone();
                    tracing::trace!(?msg, "dispatching wayland event");
                    if let Err(e) = client
                        .get_raw(msg.object_id())
                        .ok_or(WaylandError::MissingObject(msg.object_id()))?
                        .dispatch_request(&mut client, msg.object_id(), &mut msg)
                        .await
                    {
                        if let WaylandError::Fatal { object_id, code, message } = e {
                            client.display().error(&mut client, ObjectId::DISPLAY, object_id, code, message.to_string()).await?;
                        }
                        tracing::error!(?msg_clone,"Wayland: {e}");
                        return Err(e);
                    }
                }
            };
        }
    }

    async fn handle_render_message(client: &mut Client, message: Message) -> WaylandResult<()> {
        use waynest_protocols::server::core::wayland::wl_buffer::WlBuffer;
        use waynest_protocols::server::core::wayland::wl_callback::WlCallback;
        use waynest_protocols::server::core::wayland::wl_display::WlDisplay;
        use waynest_protocols::server::stable::xdg_shell::xdg_toplevel::XdgToplevel;

        match message {
            Message::Frame(callbacks) => {
                let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
                let now = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
                let ms = (now.as_millis() % (u32::MAX as u128)) as u32;
                for callback in callbacks {
                    callback.done(client, callback.0, ms).await?;
                    client
                        .get::<Display>(ObjectId::DISPLAY)
                        .unwrap()
                        .delete_id(client, ObjectId::DISPLAY, callback.0.as_raw())
                        .await?;
                    client.remove(callback.0);
                }
            }
            Message::ReleaseBuffer(buffer) => {
                buffer.release(client, buffer.id).await?;
            }
            Message::CloseToplevel(toplevel) => {
                toplevel.close(client, toplevel.id).await?;
            }
            Message::ResizeToplevel { toplevel, size } => {
                toplevel.set_size(size);
                toplevel.reconfigure(client).await?;
            }
            Message::ReconfigureToplevel(toplevel) => {
                toplevel.reconfigure(client).await?;
            }
            Message::SetToplevelVisualActive { toplevel, active } => {
                toplevel.set_activated(active);
                toplevel.reconfigure(client).await?;
            }
            Message::Seat(seat_message) => {
                if let Some(seat) = client.get::<Display>(ObjectId::DISPLAY).unwrap().seat.get() {
                    seat.handle_message(client, seat_message).await?;
                }
            }
            Message::SendPresentationFeedback {
                surface,
                display_timestamp,
                refresh_cycle,
            } => {
                surface
                    .send_presentation_feedback(client, display_timestamp, refresh_cycle)
                    .await?;
            }
        }
        Ok(())
    }
}
