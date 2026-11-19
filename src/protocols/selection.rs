use crate::{
	client::{Client, Message, MessageSink},
	error::WaylandResult,
	protocols::{
		core::data_device::{self, DataSource, WlDataSource as _},
		primary_selection::{self, PrimarySource, ZwpPrimarySelectionSourceV1 as _},
	},
};
use parking_lot::Mutex;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

// TODO: this in-process broadcast scheme is a stand-in and should be swapped out for a
// real implementation built on stardust's native clipboard/data-transfer items, so
// selections are shared with the rest of the stardust scene graph instead of only
// being visible to other clients of this one wayland-service instance.

/// every connected client, so a new selection reaches every device
static CLIENTS: Mutex<Vec<MessageSink>> = Mutex::new(Vec::new());
static CLIPBOARD: Mutex<Option<Selection>> = Mutex::new(None);
static PRIMARY: Mutex<Option<Selection>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
pub enum Kind {
	Clipboard,
	Primary,
}
impl Kind {
	fn board(self) -> &'static Mutex<Option<Selection>> {
		match self {
			Self::Clipboard => &CLIPBOARD,
			Self::Primary => &PRIMARY,
		}
	}
}

#[derive(Debug, Clone)]
pub enum Source {
	Data(Arc<DataSource>),
	Primary(Arc<PrimarySource>),
}
impl Source {
	fn kind(&self) -> Kind {
		match self {
			Self::Data(_) => Kind::Clipboard,
			Self::Primary(_) => Kind::Primary,
		}
	}
	pub fn is(&self, other: &Source) -> bool {
		match (self, other) {
			(Self::Data(a), Self::Data(b)) => Arc::ptr_eq(a, b),
			(Self::Primary(a), Self::Primary(b)) => Arc::ptr_eq(a, b),
			_ => false,
		}
	}
	/// the owner may have destroyed the source since a message about it was queued
	fn alive(&self, client: &Client) -> bool {
		match self {
			Self::Data(s) => client.holds(s.id, &**s),
			Self::Primary(s) => client.holds(s.id, &**s),
		}
	}
	pub async fn cancelled(&self, client: &mut Client) -> WaylandResult<()> {
		match self {
			Self::Data(s) => s.cancelled(client, s.id).await,
			Self::Primary(s) => s.cancelled(client, s.id).await,
		}
	}
	async fn send(&self, client: &mut Client, mime_type: String, fd: OwnedFd) -> WaylandResult<()> {
		match self {
			Self::Data(s) => s.send(client, s.id, mime_type, fd.as_fd()).await,
			Self::Primary(s) => s.send(client, s.id, mime_type, fd.as_fd()).await,
		}
	}
}

#[derive(Debug, Clone)]
pub struct Selection {
	pub source: Source,
	pub mime_types: Vec<String>,
	pub owner: MessageSink,
	serial: u32,
}
impl Selection {
	pub fn new(
		client: &Client,
		source: Source,
		mime_types: Vec<String>,
		serial: u32,
	) -> WaylandResult<Self> {
		Ok(Self {
			source,
			mime_types,
			owner: client.message_sink()?,
			serial,
		})
	}
	fn cancel(self) {
		let _ = self.owner.send(Message::SelectionCancel(self.source));
	}
	/// a stale offer just drops the fd, so the reader sees EOF
	pub fn receive(source: &Source, owner: &MessageSink, mime_type: String, fd: OwnedFd) {
		if current_is(source) {
			let _ = owner.send(Message::SelectionSend {
				source: source.clone(),
				mime_type,
				fd,
			});
		}
	}
}

pub fn current(kind: Kind) -> Option<Selection> {
	kind.board().lock().clone()
}
fn current_is(source: &Source) -> bool {
	source
		.kind()
		.board()
		.lock()
		.as_ref()
		.is_some_and(|s| s.source.is(source))
}

pub fn register_client(sink: MessageSink) {
	CLIENTS.lock().push(sink);
}
pub fn unregister_client(sink: &MessageSink) {
	CLIENTS.lock().retain(|s| !s.same_channel(sink));
	for kind in [Kind::Clipboard, Kind::Primary] {
		let mut board = kind.board().lock();
		if board.take_if(|s| s.owner.same_channel(sink)).is_some() {
			broadcast(kind, &board);
		}
	}
}
pub fn source_destroyed(source: &Source) {
	let kind = source.kind();
	let mut board = kind.board().lock();
	if board.take_if(|s| s.source.is(source)).is_some() {
		broadcast(kind, &board);
	}
}

// called with the board locked so every client sees selections in the order it changed
fn broadcast(kind: Kind, board: &Option<Selection>) {
	for sink in CLIENTS.lock().iter() {
		let _ = sink.send(Message::Selection(kind, board.clone()));
	}
}

/// sets or clears the selection, unless the serial was never sent or a newer selection won,
/// in which case the source gets cancelled
pub async fn set(
	client: &mut Client,
	kind: Kind,
	selection: Option<Selection>,
	serial: u32,
) -> WaylandResult<()> {
	let rejected = {
		let mut board = kind.board().lock();
		let stale = !client.issued(serial)
			|| board
				.as_ref()
				.is_some_and(|s| s.serial.wrapping_sub(serial) as i32 > 0);
		if stale {
			selection
		} else {
			let old = std::mem::replace(&mut *board, selection);
			let changed = old.is_some() || board.is_some();
			if let Some(old) = old
				&& !board.as_ref().is_some_and(|s| s.source.is(&old.source))
			{
				old.cancel();
			}
			if changed {
				broadcast(kind, &board);
			}
			None
		}
	};
	if let Some(s) = rejected {
		tracing::debug!("ignoring {kind:?} set_selection with stale serial {serial}");
		s.source.cancelled(client).await?;
	}
	Ok(())
}

async fn offer(client: &mut Client, kind: Kind, selection: Option<Selection>) -> WaylandResult<()> {
	match kind {
		Kind::Clipboard => data_device::offer_all(client, selection).await,
		Kind::Primary => primary_selection::offer_all(client, selection).await,
	}
}

/// the spec wants this right before a client's keyboard enter
pub async fn offer_current(client: &mut Client) -> WaylandResult<()> {
	for kind in [Kind::Clipboard, Kind::Primary] {
		offer(client, kind, current(kind)).await?;
	}
	Ok(())
}

pub async fn handle_message(client: &mut Client, message: Message) -> WaylandResult<()> {
	match message {
		// only a client with keyboard focus may see the selection, the rest get it on enter
		Message::Selection(kind, selection) => {
			if client.display()?.devices.keyboard_focused(client).await {
				offer(client, kind, selection).await?;
			}
		}
		Message::SelectionSend {
			source,
			mime_type,
			fd,
		} => {
			if source.alive(client) {
				source.send(client, mime_type, fd).await?;
			}
		}
		Message::SelectionCancel(source) => {
			if source.alive(client) {
				source.cancelled(client).await?;
			}
		}
		_ => tracing::error!("non-selection message sent to the selection handler"),
	}
	Ok(())
}
