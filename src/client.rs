use std::{os::fd::OwnedFd, sync::Arc};

use pin_project_lite::pin_project;
use stardust_xr_fusion::types::Size2;
use tokio::{net::UnixStream, sync::mpsc};
use tokio_stream::Stream;
use waynest::{Connection, ProtocolError, Socket};
use waynest_server::{Store, StoreError};

use crate::{
    error::WaylandError,
    protocols::{
        core::{
            buffer::Buffer,
            callback::Callback,
            data_device::DataSource,
            seat::SeatMessage,
            surface::Surface,
        },
        presentation::MonotonicTimestamp,
        xdg::toplevel::Toplevel,
    },
};

impl<T: Clone> From<StoreError<T>> for WaylandError {
    fn from(_value: StoreError<T>) -> Self {
        Self::FailedToInsertObject
    }
}

pub enum Message {
    Frame(Vec<Arc<Callback>>),
    ReleaseBuffer(Arc<Buffer>),
    CloseToplevel(Arc<Toplevel>),
    ResizeToplevel {
        toplevel: Arc<Toplevel>,
        size: Option<Size2>,
    },
    ReconfigureToplevel(Arc<Toplevel>),
    SetToplevelVisualActive {
        toplevel: Arc<Toplevel>,
        active: bool,
    },
    Seat(SeatMessage),
    SendPresentationFeedback {
        surface: Arc<Surface>,
        display_timestamp: MonotonicTimestamp,
        refresh_cycle: u64,
    },
    // TODO: this whole clipboard implementation is the bare minimum to get copy/paste
    // working across clients. It's missing: cancelling the previous selection's
    // wl_data_source (the `cancelled` event), removing disconnected clients from the
    // broadcast list, drag-and-drop (wl_data_device.start_drag is still a no-op), and
    // any kind of `dnd_action`/`finish` negotiation. See protocols/core/data_device.rs.
    ClipboardSelection {
        source: Arc<DataSource>,
        mime_types: Vec<String>,
        owner: MessageSink,
    },
    ClipboardSend {
        source: Arc<DataSource>,
        mime_type: String,
        fd: OwnedFd,
    },
}

pin_project! {
    pub struct Client {
        store: Store<Client, WaylandError>,
        #[pin]
        connection: Socket,
        next_event_serial: u32,
    }
}
impl Connection for Client {
    type Error = WaylandError;

    fn fd(&mut self) -> Result<std::os::unix::prelude::OwnedFd, <Self as Connection>::Error> {
        Ok(self.connection.fd()?)
    }
}
impl Stream for Client {
    type Item = <Socket as Stream>::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // <Socket as Stream>::poll_next(self.project().connection, cx)
        self.project().connection.poll_next(cx)
    }
}
impl futures_sink::Sink<waynest::Message> for Client {
    type Error = ProtocolError;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().connection.poll_ready(cx)
    }

    fn start_send(
        self: std::pin::Pin<&mut Self>,
        item: waynest::Message,
    ) -> Result<(), Self::Error> {
        self.project().connection.start_send(item)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().connection.poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.project().connection.poll_close(cx)
    }
}
impl Client {
    pub fn new(unix_stream: UnixStream) -> tokio::io::Result<Self> {
        Ok(Self {
            store: Store::new(),
            connection: Socket::new(unix_stream.into_std()?)?,
            next_event_serial: 0,
        })
    }
    pub fn next_event_serial(&mut self) -> u32 {
        let prev = self.next_event_serial;
        self.next_event_serial = self.next_event_serial.wrapping_add(1);
        prev
    }
}

impl waynest_server::Client for Client {
    type Store = Store<Client, WaylandError>;

    fn store(&self) -> &Self::Store {
        &self.store
    }

    fn store_mut(&mut self) -> &mut Self::Store {
        &mut self.store
    }
}

pub type MessageSink = mpsc::UnboundedSender<Message>;
