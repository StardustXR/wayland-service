use crate::{
	client::{Client, MessageSink},
	error::WaylandResult,
	protocols::selection::{self, Kind, Selection, Source},
};
use parking_lot::Mutex;
use std::{os::fd::OwnedFd, sync::Arc};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_data_source::WlDataSource;
use waynest_protocols::server::core::wayland::{
	wl_data_device::*, wl_data_device_manager::*, wl_data_offer::WlDataOffer,
};
use waynest_server::Client as _;

// TODO: drag-and-drop is unimplemented, start_drag cancels the source immediately and
// actions/action, enter/motion/drop/leave are all no-ops or unused.

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
		let device = client.insert(id, DataDevice { id })?;
		let display = client.display()?;
		display.data_devices.lock().push(Arc::downgrade(&device));
		if display.devices.keyboard_focused(client).await {
			device
				.offer_selection(client, selection::current(Kind::Clipboard))
				.await?;
		}
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataSource {
	pub id: ObjectId,
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
		self.mime_types.lock().push(mime_type);
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		let source = client.try_get::<DataSource>(self.id)?;
		selection::source_destroyed(&Source::Data(source));
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
	pub id: ObjectId,
}
impl DataDevice {
	/// a null selection just sends a null selection event
	async fn offer_selection(
		&self,
		client: &mut Client,
		selection: Option<Selection>,
	) -> WaylandResult<()> {
		let Some(Selection {
			source,
			mime_types,
			owner,
			..
		}) = selection
		else {
			return self.selection(client, self.id, None).await;
		};
		let offer_id = client.display()?.next_server_id();
		let offer = client.insert(
			offer_id,
			DataOffer {
				id: offer_id,
				source,
				owner,
			},
		)?;

		self.data_offer(client, self.id, offer_id).await?;
		for mime_type in mime_types {
			offer.offer(client, offer_id, mime_type).await?;
		}
		self.selection(client, self.id, Some(offer_id)).await
	}
}
impl WlDataDevice for DataDevice {
	type Connection = Client;

	async fn start_drag(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		source: Option<ObjectId>,
		_origin: ObjectId,
		_icon: Option<ObjectId>,
		_serial: u32,
	) -> WaylandResult<()> {
		// drag-and-drop isn't supported, so end the drag before it starts
		if let Some(source) = source {
			client
				.try_get::<DataSource>(source)?
				.cancelled(client, source)
				.await?;
		}
		Ok(())
	}

	async fn set_selection(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		source: Option<ObjectId>,
		serial: u32,
	) -> WaylandResult<()> {
		let selection = match source {
			Some(id) => {
				let source = client.try_get::<DataSource>(id)?;
				let mime_types = source.mime_types.lock().clone();
				Some(Selection::new(
					client,
					Source::Data(source),
					mime_types,
					serial,
				)?)
			}
			None => None,
		};
		selection::set(client, Kind::Clipboard, selection, serial).await
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

pub async fn offer_all(client: &mut Client, selection: Option<Selection>) -> WaylandResult<()> {
	for device in client.display()?.data_devices(client) {
		device.offer_selection(client, selection.clone()).await?;
	}
	Ok(())
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataOffer {
	id: ObjectId,
	source: Source,
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
		Selection::receive(&self.source, &self.owner, mime_type, fd);
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
