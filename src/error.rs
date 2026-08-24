use std::io;

use waynest::ObjectId;

pub type WaylandResult<T, E = WaylandError> = std::result::Result<T, E>;
#[derive(thiserror::Error, Debug)]
pub enum WaylandError {
	// #[error("Listener error: {0}")]
	// Listener(#[from] waynest_server::ListenerError),
	#[error("I/O error: {0}")]
	Io(#[from] io::Error),
	#[error("Decode error: {0}")]
	DecodeError(#[from] waynest::ProtocolError),
	#[error("Client requested unknown global: {0}")]
	UnknownGlobal(u32),
	#[error("No object found with ID {0}")]
	MissingObject(ObjectId),
	#[error("Fatal error on object {object_id} with code {code}: {message}")]
	Fatal {
		object_id: ObjectId,
		code: u32,
		message: &'static str,
	},
	#[error("Memfd error: {0}")]
	MemfdError(#[from] memfd::Error),
	// #[error("Dmabuf import error: {0}")]
	// DmabufImport(#[from] bevy_dmabuf::import::ImportError),
	// #[error("Server error: {0}")]
	// Server(#[from] ServerError),
	#[error("Failed to Insert Object")]
	FailedToInsertObject,
}
