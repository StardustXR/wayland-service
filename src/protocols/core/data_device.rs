use crate::{
	client::{Client, Message, MessageSink},
	error::WaylandResult,
};
use std::{
	os::fd::{AsFd, OwnedFd},
	sync::{Arc, Mutex},
};
use waynest::ObjectId;
use waynest_protocols::server::core::wayland::{
	wl_data_device::*, wl_data_device_manager::*, wl_data_offer::WlDataOffer, wl_data_source::*,
};
use waynest_server::Client as _;

// TODO: this is the most barebones possible implementation of clipboard support (copy
// and paste of a single selection), just enough for one client to set a selection and
// another to read it. Known gaps, in rough order of how much they'll bite:
// - Drag-and-drop is entirely unimplemented (start_drag, actions/action, enter/motion/
//   drop/leave are all no-ops or unused).
// - The previous wl_data_source never gets its `cancelled` event when a new selection
//   replaces it, so old clients think they still own the clipboard.
// - Disconnected clients are never removed from CLIENTS, so the broadcast list only
//   grows; sends to dead clients are silently dropped (mpsc errors are ignored) but the
//   Vec leaks entries for the life of the process.
// - Only one global clipboard selection is tracked (no per-seat scoping), and only one
//   wl_data_device per client is supported (mirrors the existing single-seat/output
//   assumption elsewhere in this codebase via Display's OnceLocks).
// - set_selection with `source: None` (clearing the selection) doesn't notify anyone.
// - No serial validation anywhere.
//
// This whole in-process broadcast scheme is a stand-in and should be swapped out for a
// real implementation built on stardust's native clipboard/data-transfer items, so
// clipboard contents are shared with the rest of the stardust scene graph instead of
// only being visible to other clients of this one wayland-service instance.

/// All currently-connected clients' message sinks, so a new selection can be broadcast
/// to every wl_data_device that exists.
static CLIENTS: Mutex<Vec<MessageSink>> = Mutex::new(Vec::new());

struct ClipboardSelection {
	source: Arc<DataSource>,
	mime_types: Vec<String>,
	owner: MessageSink,
}
/// The current clipboard contents, set by the last client to call set_selection.
static CLIPBOARD: Mutex<Option<ClipboardSelection>> = Mutex::new(None);

pub fn register_client(sink: MessageSink) {
	CLIENTS.lock().unwrap().push(sink);
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataDeviceManager;
impl WlDataDeviceManager for DataDeviceManager {
	type Connection = Client;

	async fn create_data_source(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		client.insert(
			id,
			DataSource {
				id,
				mime_types: Mutex::new(Vec::new()),
			},
		)?;
		Ok(())
	}

	async fn get_data_device(
		&self,
		client: &mut Client,
		_sender_id: ObjectId,
		id: ObjectId,
		_seat: ObjectId,
	) -> WaylandResult<()> {
		client.insert(id, DataDevice { id })?;
		let _ = client.display().data_device.set(id);

		// If a clipboard selection already exists, hand this brand-new device an offer
		// for it immediately, otherwise it'd never find out about the current selection.
		let existing = CLIPBOARD.lock().unwrap().as_ref().map(|selection| {
			(
				selection.source.clone(),
				selection.mime_types.clone(),
				selection.owner.clone(),
			)
		});
		if let Some((source, mime_types, owner)) = existing {
			offer_selection(client, id, source, mime_types, owner).await?;
		}

		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataSource {
	id: ObjectId,
	mime_types: Mutex<Vec<String>>,
}
impl WlDataSource for DataSource {
	type Connection = Client;

	async fn offer(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		mime_type: String,
	) -> WaylandResult<()> {
		self.mime_types.lock().unwrap().push(mime_type);
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}

	async fn set_actions(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_dnd_actions: DndAction,
	) -> WaylandResult<()> {
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataDevice {
	id: ObjectId,
}
impl WlDataDevice for DataDevice {
	type Connection = Client;

	async fn start_drag(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_source: Option<ObjectId>,
		_origin: ObjectId,
		_icon: Option<ObjectId>,
		_serial: u32,
	) -> WaylandResult<()> {
		// TODO: drag-and-drop isn't implemented at all.
		Ok(())
	}

	async fn set_selection(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		source: Option<ObjectId>,
		_serial: u32,
	) -> WaylandResult<()> {
		let Some(source_id) = source else {
			// TODO: doesn't notify anyone that the selection was cleared.
			*CLIPBOARD.lock().unwrap() = None;
			return Ok(());
		};
		let source = client.try_get::<DataSource>(source_id)?;
		let mime_types = source.mime_types.lock().unwrap().clone();
		let owner = client.message_sink();

		*CLIPBOARD.lock().unwrap() = Some(ClipboardSelection {
			source: source.clone(),
			mime_types: mime_types.clone(),
			owner: owner.clone(),
		});

		for sink in CLIENTS.lock().unwrap().iter() {
			let _ = sink.send(Message::ClipboardSelection {
				source: source.clone(),
				mime_types: mime_types.clone(),
				owner: owner.clone(),
			});
		}

		Ok(())
	}

	async fn release(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}

/// Creates a wl_data_offer for `source` on `device_id` and sends the `data_offer`,
/// `offer` (per mime type) and `selection` events for it, in that order. `owner` is
/// the message sink of the client that actually owns `source`, stashed on the offer so
/// `receive()` later knows who to ask for the data.
async fn offer_selection(
	client: &mut Client,
	device_id: ObjectId,
	source: Arc<DataSource>,
	mime_types: Vec<String>,
	owner: MessageSink,
) -> WaylandResult<()> {
	let device = client.try_get::<DataDevice>(device_id)?;
	let offer_id = client.display().next_server_id();
	let offer = client.insert(
		offer_id,
		DataOffer {
			id: offer_id,
			source,
			owner,
		},
	)?;

	device.data_offer(client, device_id, offer_id).await?;
	for mime_type in mime_types {
		offer.offer(client, offer_id, mime_type).await?;
	}
	device.selection(client, device_id, Some(offer_id)).await?;

	Ok(())
}

/// Handles `Message::ClipboardSelection`/`Message::ClipboardSend`, dispatched from
/// socket.rs's per-client message loop.
pub async fn handle_message(client: &mut Client, message: Message) -> WaylandResult<()> {
	match message {
		Message::ClipboardSelection {
			source,
			mime_types,
			owner,
		} => {
			let device_id = client.display().data_device.get().copied();
			if let Some(device_id) = device_id {
				offer_selection(client, device_id, source, mime_types, owner).await?;
			}
		}
		Message::ClipboardSend {
			source,
			mime_type,
			fd,
		} => {
			source
				.send(client, source.id, mime_type, fd.as_fd())
				.await?;
		}
		_ => unreachable!("handle_message only called for clipboard messages"),
	}
	Ok(())
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataOffer {
	id: ObjectId,
	source: Arc<DataSource>,
	owner: MessageSink,
}
impl WlDataOffer for DataOffer {
	type Connection = Client;

	async fn accept(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_serial: u32,
		_mime_type: Option<String>,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn receive(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		mime_type: String,
		fd: OwnedFd,
	) -> WaylandResult<()> {
		let _ = self.owner.send(Message::ClipboardSend {
			source: self.source.clone(),
			mime_type,
			fd,
		});
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}

	async fn finish(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn set_actions(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_dnd_actions: DndAction,
		_preferred_action: DndAction,
	) -> WaylandResult<()> {
		Ok(())
	}
}
