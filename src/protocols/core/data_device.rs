use crate::{
	client::{Client, MessageSink},
	error::{WaylandError, WaylandResult},
	protocols::{
		core::surface::{Surface, SurfaceRole},
		selection::{self, Kind, Selection, Source},
	},
};
use parking_lot::Mutex;
use std::{
	os::fd::{AsFd, OwnedFd},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_data_source::WlDataSource;
use waynest_protocols::server::core::wayland::{
	wl_data_device::{Error as DeviceError, WlDataDevice},
	wl_data_device_manager::{DndAction, WlDataDeviceManager},
	wl_data_offer::{Error as OfferError, WlDataOffer},
	wl_data_source::Error as SourceError,
};
use waynest_server::Client as _;

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataDeviceManager {
	pub version: u32,
}
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
				version: self.version,
				mime_types: Mutex::new(Vec::new()),
				actions: Mutex::new(DndAction::empty()),
				used: AtomicBool::new(false),
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
		let device = client.insert(
			id,
			DataDevice {
				id,
				version: self.version,
			},
		)?;
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
	pub version: u32,
	mime_types: Mutex<Vec<String>>,
	pub actions: Mutex<DndAction>,
	/// a source is good for one selection or one drag
	used: AtomicBool,
}
impl DataSource {
	pub fn mime_types(&self) -> Vec<String> {
		self.mime_types.lock().clone()
	}
	pub async fn cancel(&self, client: &mut Client) -> WaylandResult<()> {
		if client.holds(self.id, self) {
			self.cancelled(client, self.id).await?;
		}
		Ok(())
	}
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
		selection::source_destroyed(&Source::Data(source.clone()));
		client
			.display()?
			.devices
			.source_destroyed(client, &source)
			.await?;
		client.remove(self.id);
		Ok(())
	}

	async fn set_actions(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		dnd_actions: DndAction,
	) -> WaylandResult<()> {
		if self.used.load(Ordering::Relaxed) {
			return Err(WaylandError::Fatal {
				object_id: self.id,
				code: SourceError::InvalidSource as u32,
				message: "actions set on a source that's already in use",
			});
		}
		*self.actions.lock() = dnd_actions;
		Ok(())
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct DataDevice {
	pub id: ObjectId,
	version: u32,
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
				role: OfferRole::Selection { source, owner },
			},
		)?;

		self.data_offer(client, self.id, offer_id).await?;
		for mime_type in mime_types {
			offer.offer(client, offer_id, mime_type).await?;
		}
		self.selection(client, self.id, Some(offer_id)).await
	}

	/// introduces an offer for a drag about to enter one of this client's surfaces
	pub async fn offer_drag(
		&self,
		client: &mut Client,
		source: Arc<DataSource>,
	) -> WaylandResult<Arc<DataOffer>> {
		let offer_id = client.display()?.next_server_id();
		let mime_types = source.mime_types();
		let actions = *source.actions.lock();
		let offer = client.insert(
			offer_id,
			DataOffer {
				id: offer_id,
				role: OfferRole::Drag(Mutex::new(DragOffer {
					source,
					version: self.version,
					phase: DragPhase::Hovering,
					accepted: None,
					actions: DndAction::empty(),
					preferred: DndAction::empty(),
					action: DndAction::empty(),
				})),
			},
		)?;
		self.data_offer(client, self.id, offer_id).await?;
		for mime_type in mime_types {
			offer.offer(client, offer_id, mime_type).await?;
		}
		if self.version >= 3 {
			offer.source_actions(client, offer_id, actions).await?;
		}
		Ok(offer)
	}
}
impl WlDataDevice for DataDevice {
	type Connection = Client;

	async fn start_drag(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		source: Option<ObjectId>,
		origin: ObjectId,
		icon: Option<ObjectId>,
		serial: u32,
	) -> WaylandResult<()> {
		let source = source
			.map(|id| client.try_get::<DataSource>(id))
			.transpose()?;
		if let Some(source) = &source
			&& source.used.swap(true, Ordering::Relaxed)
		{
			return Err(WaylandError::Fatal {
				object_id: self.id,
				code: DeviceError::UsedSource as u32,
				message: "drag source was already used",
			});
		}
		let icon = icon.map(|id| client.try_get::<Surface>(id)).transpose()?;
		if let Some(icon) = &icon {
			icon.assign_role(SurfaceRole::DndIcon)
				.map_err(|_| WaylandError::Fatal {
					object_id: self.id,
					code: DeviceError::Role as u32,
					message: "drag icon surface already has another role",
				})?;
		}
		let origin = client.try_get::<Surface>(origin)?;
		let devices = &client.display()?.devices;
		devices
			.start_drag(client, source, origin, icon, serial)
			.await
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
				source.used.store(true, Ordering::Relaxed);
				let mime_types = source.mime_types();
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
	pub id: ObjectId,
	role: OfferRole,
}
#[derive(Debug)]
enum OfferRole {
	Selection { source: Source, owner: MessageSink },
	Drag(Mutex<DragOffer>),
}

/// drags never leave their client, so the source is always right here
#[derive(Debug)]
struct DragOffer {
	source: Arc<DataSource>,
	version: u32,
	phase: DragPhase,
	accepted: Option<String>,
	actions: DndAction,
	preferred: DndAction,
	action: DndAction,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragPhase {
	Hovering,
	/// the drag moved on without dropping here, so the offer is dead
	Left,
	Dropped,
	Finished,
}
impl DragOffer {
	/// before v3 there are no actions, accepting a mime type is enough
	fn droppable(&self) -> bool {
		self.accepted.is_some() && (self.version < 3 || !self.action.is_empty())
	}
	/// the destination's preferred action wins if the source allows it, otherwise the first one both allow
	fn choose_action(&mut self) -> Option<DndAction> {
		let both = *self.source.actions.lock() & self.actions;
		let action = if both.contains(self.preferred) && !self.preferred.is_empty() {
			self.preferred
		} else {
			[DndAction::Copy, DndAction::Move, DndAction::Ask]
				.into_iter()
				.find(|a| both.contains(*a))
				.unwrap_or(DndAction::empty())
		};
		(action != self.action).then(|| {
			self.action = action;
			action
		})
	}
}
impl DataOffer {
	pub fn drag_left(&self) {
		if let OfferRole::Drag(d) = &self.role {
			let mut d = d.lock();
			if d.phase == DragPhase::Hovering {
				d.phase = DragPhase::Left;
			}
		}
	}
	pub fn drag_droppable(&self) -> bool {
		matches!(&self.role, OfferRole::Drag(d) if d.lock().droppable())
	}
	pub fn drag_dropped(&self) {
		if let OfferRole::Drag(d) = &self.role {
			let mut d = d.lock();
			if d.phase == DragPhase::Hovering {
				d.phase = DragPhase::Dropped;
			}
		}
	}

	fn invalid(&self, code: OfferError, message: &'static str) -> WaylandError {
		WaylandError::Fatal {
			object_id: self.id,
			code: code as u32,
			message,
		}
	}
}
impl WlDataOffer for DataOffer {
	type Connection = Client;

	async fn accept(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		_serial: u32,
		mime_type: Option<String>,
	) -> WaylandResult<()> {
		let OfferRole::Drag(d) = &self.role else {
			return Ok(());
		};
		let source = {
			let mut d = d.lock();
			if d.phase != DragPhase::Hovering || d.accepted == mime_type {
				return Ok(());
			}
			d.accepted = mime_type.clone();
			d.source.clone()
		};
		if client.holds(source.id, &*source) {
			source.target(client, source.id, mime_type).await?;
		}
		Ok(())
	}

	async fn receive(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		mime_type: String,
		fd: OwnedFd,
	) -> WaylandResult<()> {
		match &self.role {
			OfferRole::Selection { source, owner } => {
				Selection::receive(source, owner, mime_type, fd)
			}
			OfferRole::Drag(d) => {
				let (phase, source) = {
					let d = d.lock();
					(d.phase, d.source.clone())
				};
				// a dead offer just drops the fd, so the reader sees EOF
				if matches!(phase, DragPhase::Hovering | DragPhase::Dropped)
					&& client.holds(source.id, &*source)
				{
					source
						.send(client, source.id, mime_type, fd.as_fd())
						.await?;
				}
			}
		}
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		// a drop that never finished didn't happen as far as the source is concerned
		if let OfferRole::Drag(d) = &self.role {
			let (phase, source, version) = {
				let d = d.lock();
				(d.phase, d.source.clone(), d.version)
			};
			if phase == DragPhase::Dropped && version >= 3 {
				source.cancel(client).await?;
			}
		}
		Ok(())
	}

	async fn finish(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		let OfferRole::Drag(d) = &self.role else {
			return Err(self.invalid(OfferError::InvalidFinish, "finish on a selection offer"));
		};
		let source = {
			let mut d = d.lock();
			if d.phase != DragPhase::Dropped {
				return Err(self.invalid(OfferError::InvalidFinish, "finish before the drop"));
			}
			if d.action.is_empty() {
				return Err(self.invalid(OfferError::InvalidFinish, "finish without an action"));
			}
			d.phase = DragPhase::Finished;
			d.source.clone()
		};
		if client.holds(source.id, &*source) && source.version >= 3 {
			source.dnd_finished(client, source.id).await?;
		}
		Ok(())
	}

	async fn set_actions(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		dnd_actions: DndAction,
		preferred_action: DndAction,
	) -> WaylandResult<()> {
		let OfferRole::Drag(d) = &self.role else {
			return Err(self.invalid(OfferError::InvalidOffer, "actions on a selection offer"));
		};
		if preferred_action.bits().count_ones() > 1 {
			return Err(self.invalid(OfferError::InvalidAction, "more than one preferred action"));
		}
		let (changed, source) = {
			let mut d = d.lock();
			if d.phase != DragPhase::Hovering {
				return Err(
					self.invalid(OfferError::InvalidOffer, "actions on a finished drag offer")
				);
			}
			d.actions = dnd_actions;
			d.preferred = preferred_action;
			(d.choose_action(), d.source.clone())
		};
		if let Some(action) = changed {
			self.action(client, self.id, action).await?;
			if client.holds(source.id, &*source) && source.version >= 3 {
				source.action(client, source.id, action).await?;
			}
		}
		Ok(())
	}
}
