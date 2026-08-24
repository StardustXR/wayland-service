use super::buffer_backing::DmabufBacking;
use crate::{
	client::Client,
	error::{WaylandError, WaylandResult},
	protocols::core::buffer::{Buffer, BufferBacking},
};
use drm_fourcc::DrmFourcc;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use stardust_xr_fusion::dmatex::{DisjointDmatexPlane, DmatexPlane};
use std::{os::fd::OwnedFd, sync::OnceLock};
use waynest::ObjectId;
use waynest_protocols::server::stable::linux_dmabuf_v1::zwp_linux_buffer_params_v1::{
	Error, Flags, ZwpLinuxBufferParamsV1,
};
use waynest_server::Client as _;

/// Parameters for creating a DMA-BUF-based wl_buffer
///
/// This is a temporary object that collects dmabufs and other parameters
/// that together form a single logical buffer. The object may eventually
/// create one wl_buffer unless cancelled by destroying it.
#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct BufferParams {
	pub id: ObjectId,
	pub(super) planes: Mutex<FxHashMap<u32, DisjointDmatexPlane>>,
	pub(super) modifier: OnceLock<u64>,
}

impl BufferParams {
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn new(id: ObjectId) -> Self {
		tracing::info!("Creating new BufferParams with id {:?}", id);
		Self {
			id,
			planes: Mutex::new(FxHashMap::default()),
			modifier: OnceLock::new(),
		}
	}
}

impl ZwpLinuxBufferParamsV1 for BufferParams {
	type Connection = Client;

	async fn destroy(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		tracing::info!("Destroying BufferParams {:?}", self.id);
		Ok(())
	}

	#[tracing::instrument(level = "debug", skip_all)]
	async fn add(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		fd: OwnedFd,
		plane_idx: u32,
		offset: u32,
		stride: u32,
		modifier_hi: u32,
		modifier_lo: u32,
	) -> WaylandResult<()> {
		// let fd_num = fd.as_raw_fd();
		// tracing::info!(
		//     "Adding plane {} with fd {} to BufferParams {:?}",
		//     plane_idx,
		//     fd_num,
		//     self.id
		// );

		let mut planes = self.planes.lock();

		// Check if plane index is already set
		if planes.contains_key(&plane_idx) {
			tracing::error!(
				"Plane {} already exists in BufferParams {:?}",
				plane_idx,
				self.id
			);
			return Err(WaylandError::MissingObject(self.id));
		}

		// Create plane with the provided parameters
		let plane = DisjointDmatexPlane {
			dmabuf_fd: fd,
			plane: DmatexPlane {
				offset: offset as u64,
				row_size: stride as u64,
				array_element_size: 0,
				depth_slice_size: 0,
			},
		};

		let modifier = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
		let stored_modifier = *self.modifier.get_or_init(|| modifier);
		if modifier != stored_modifier {
			tracing::error!(
				"used differing modifers for dmabuf backing planes, previous modifier: {:x}, new modifier: {:x}",
				stored_modifier,
				modifier
			);
			return Err(WaylandError::Fatal {
				object_id: self.id,
				code: Error::InvalidFormat.into(),
				message: "used multiple differing modifiers for one buffer",
			});
		}

		// Store the plane
		planes.insert(plane_idx, plane);
		Ok(())
	}

	#[tracing::instrument(level = "debug", skip_all)]
	async fn create(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		width: i32,
		height: i32,
		format: u32,
		flags: Flags,
	) -> WaylandResult<()> {
		tracing::info!("Creating buffer from BufferParams {:?}", self.id);
		// Create the buffer with DMA-BUF backing using self as the backing
		let size = [width as u32, height as u32].into();
		let buffer = DmabufBacking::from_params(
			client.get::<Self>(self.id).unwrap(),
			size,
			DrmFourcc::try_from(format).unwrap(),
			flags,
		)
		.await
		.inspect_err(|e| tracing::error!("Failed to import dmabuf because {e}"))
		.map(|backing| {
			let id = client.display().next_server_id();
			Buffer::new(client, id, BufferBacking::Dmabuf(backing))
		});

		match buffer {
			Ok(buffer) => self.created(client, self.id, buffer?.id).await,
			Err(_) => {
				client.remove(self.id);
				self.failed(client, self.id).await
			}
		}
	}

	#[tracing::instrument(level = "debug", skip_all)]
	async fn create_immed(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		buffer_id: ObjectId,
		width: i32,
		height: i32,
		format: u32,
		flags: Flags,
	) -> WaylandResult<()> {
		// Create the buffer with DMA-BUF backing using self as the backing
		match DmabufBacking::from_params(
			client.get::<Self>(self.id).unwrap(),
			[width as u32, height as u32].into(),
			DrmFourcc::try_from(format).unwrap(),
			flags,
		)
		.await
		{
			Ok(backing) => {
				Buffer::new(client, buffer_id, BufferBacking::Dmabuf(backing))?;
			}
			Err(e) => {
				tracing::error!("Failed to import dmabuf because {e}");
				return Err(WaylandError::Fatal {
					object_id: buffer_id,
					code: Error::Incomplete as u32,
					message: "Failed to import dmabuf",
				});
			}
		}
		Ok(())
	}
}
