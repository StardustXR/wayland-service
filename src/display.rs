#![allow(unused)]

use crate::{
	client::MessageSink,
	error::WaylandResult,
	protocols::core::{
		callback::{Callback, WlCallback},
		data_device::{self, DataDevice},
		output::Output,
		seat::{Devices, Seat},
	},
	registry::Registry,
	util::CounterU32,
};
use parking_lot::Mutex;
use std::{
	sync::{Arc, OnceLock, Weak},
	time::Instant,
};
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_display::*;
use waynest_server::Client as _;

/// ids from here up are ours, below are the client's
pub const SERVER_ID_START: u32 = 0xff000000;

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Display {
	pub message_sink: MessageSink,
	pub pid: Option<i32>,
	pub seat: OnceLock<Arc<Seat>>,
	pub devices: Devices,
	pub outputs: Mutex<Vec<Weak<Output>>>,
	pub data_devices: Mutex<Vec<Weak<DataDevice>>>,
	id_counter: CounterU32,
	pub creation_time: Instant,
}
impl Display {
	pub fn new(message_sink: MessageSink, pid: Option<i32>) -> Self {
		Self {
			message_sink,
			pid,
			seat: OnceLock::new(),
			devices: Devices::default(),
			outputs: Mutex::new(Vec::new()),
			data_devices: Mutex::new(Vec::new()),
			id_counter: CounterU32::new(SERVER_ID_START),
			creation_time: Instant::now(),
		}
	}
	pub fn outputs(&self, client: &crate::client::Client) -> Vec<Arc<Output>> {
		client.live(&self.outputs, |o| o.id)
	}
	pub fn data_devices(&self, client: &crate::client::Client) -> Vec<Arc<DataDevice>> {
		client.live(&self.data_devices, |d| d.id)
	}
	pub fn next_server_id(&self) -> ObjectId {
		unsafe { ObjectId::from_raw(self.id_counter.get()) }
	}
}
impl Drop for Display {
	fn drop(&mut self) {
		data_device::unregister_client(&self.message_sink);
	}
}
impl WlDisplay for Display {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_display:request:sync
	async fn sync(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
		callback_id: ObjectId,
	) -> WaylandResult<()> {
		let serial = client.next_event_serial();
		Callback(callback_id)
			.done(client, callback_id, serial)
			.await?;

		self.delete_id(client, sender_id, callback_id.as_raw())
			.await?;
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_display:request:get_registry
	async fn get_registry(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		registry_id: ObjectId,
	) -> WaylandResult<()> {
		let registry = client.insert(registry_id, Registry)?;

		registry.advertise_globals(client, registry_id).await?;

		Ok(())
	}
}
