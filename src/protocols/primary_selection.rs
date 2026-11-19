use crate::{
	client::{Client, MessageSink},
	error::WaylandResult,
	protocols::selection::{self, Kind, Selection, Source},
};
use parking_lot::Mutex;
use std::{os::fd::OwnedFd, sync::Arc};
use waynest::ObjectId;
use waynest_protocols::server::unstable::wp_primary_selection_unstable_v1::{
	zwp_primary_selection_device_manager_v1::*, zwp_primary_selection_device_v1::*,
	zwp_primary_selection_offer_v1::*,
};
pub use waynest_protocols::server::unstable::wp_primary_selection_unstable_v1::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct PrimarySelectionManager(pub ObjectId);
impl ZwpPrimarySelectionDeviceManagerV1 for PrimarySelectionManager {
	type Connection = Client;

	async fn create_source(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
	) -> WaylandResult<()> {
		client.insert(
			id,
			PrimarySource {
				id,
				mime_types: Mutex::new(Vec::new()),
			},
		)?;
		Ok(())
	}

	async fn get_device(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		id: ObjectId,
		_seat: ObjectId,
	) -> WaylandResult<()> {
		let device = client.insert(id, PrimaryDevice { id })?;
		let display = client.display()?;
		display.primary_devices.lock().push(Arc::downgrade(&device));
		if display.devices.keyboard_focused(client).await {
			device
				.offer_selection(client, selection::current(Kind::Primary))
				.await?;
		}
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.0);
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct PrimarySource {
	pub id: ObjectId,
	mime_types: Mutex<Vec<String>>,
}
impl ZwpPrimarySelectionSourceV1 for PrimarySource {
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
		let source = client.try_get::<PrimarySource>(self.id)?;
		selection::source_destroyed(&Source::Primary(source));
		client.remove(self.id);
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct PrimaryDevice {
	pub id: ObjectId,
}
impl PrimaryDevice {
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
			PrimaryOffer {
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
impl ZwpPrimarySelectionDeviceV1 for PrimaryDevice {
	type Connection = Client;

	async fn set_selection(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		source: Option<ObjectId>,
		serial: u32,
	) -> WaylandResult<()> {
		let selection = match source {
			Some(id) => {
				let source = client.try_get::<PrimarySource>(id)?;
				let mime_types = source.mime_types.lock().clone();
				Some(Selection::new(
					client,
					Source::Primary(source),
					mime_types,
					serial,
				)?)
			}
			None => None,
		};
		selection::set(client, Kind::Primary, selection, serial).await
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}

pub async fn offer_all(client: &mut Client, selection: Option<Selection>) -> WaylandResult<()> {
	for device in client.display()?.primary_devices(client) {
		device.offer_selection(client, selection.clone()).await?;
	}
	Ok(())
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct PrimaryOffer {
	id: ObjectId,
	source: Source,
	owner: MessageSink,
}
impl ZwpPrimarySelectionOfferV1 for PrimaryOffer {
	type Connection = Client;

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
}
