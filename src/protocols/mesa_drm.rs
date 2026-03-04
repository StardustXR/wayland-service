use drm_fourcc::DrmFourcc;
use mint::Vector2;
use rustc_hash::FxHashSet;
use stardust_xr_cme::format::DmatexFormat;
use stardust_xr_fusion::drawable::DmatexPlane;
use std::os::fd::OwnedFd;
use waynest::ObjectId;
use waynest_protocols::server::mesa::drm::wl_drm::*;

use crate::{
    CLIENT,
    client::Client,
    error::WaylandResult,
    protocols::{
        core::buffer::{Buffer, BufferBacking},
        dmabuf::buffer_backing::DmabufBacking,
    },
    vulkan_ctx::VK,
};

#[derive(Debug, waynest_server::RequestDispatcher, Default)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct MesaDrm {
    version: u32,
}
impl MesaDrm {
    pub async fn new(client: &mut Client, id: ObjectId, version: u32) -> WaylandResult<MesaDrm> {
        let sd_client = CLIENT.wait();
        let vk = VK.wait();
        let dev_id = vk.render_dev.drm_node_id();
        let drm = MesaDrm { version };

        let path = format!("/dev/dri/renderD{}", dev_id & 0xFF);
        drm.device(client, id, path).await?;

        // this is basically just enabling ancient dmabufs lel
        if drm.version >= 2 {
            drm.capabilities(client, id, Capability::Prime as u32)
                .await?;
        }

        // DRM fomrats check
        let formats = DmatexFormat::enumerate(sd_client, &vk.render_dev)
            .await
            .iter()
            .flat_map(|v| v.into_values())
            .map(|f| f.drm_fourcc())
            .collect::<FxHashSet<_>>();
        for format in formats {
            drm.format(client, id, format as u32).await?;
        }

        Ok(drm)
    }
}
impl WlDrm for MesaDrm {
    type Connection = Client;

    async fn authenticate(
        &self,
        client: &mut Self::Connection,
        sender_id: ObjectId,
        _id: u32,
    ) -> WaylandResult<()> {
        self.authenticated(client, sender_id).await
    }

    async fn create_buffer(
        &self,
        _client: &mut Self::Connection,
        _sender_id: ObjectId,
        _id: ObjectId,
        _name: u32,
        _width: i32,
        _height: i32,
        _stride: u32,
        _format: u32,
    ) -> WaylandResult<()> {
        tracing::error!("Tried to create non-prime wl_drm buffer!");
        Ok(())
    }

    async fn create_planar_buffer(
        &self,
        _client: &mut Self::Connection,
        _sender_id: ObjectId,
        _id: ObjectId,
        _name: u32,
        _width: i32,
        _height: i32,
        _format: u32,
        _offset0: i32,
        _stride0: i32,
        _offset1: i32,
        _stride1: i32,
        _offset2: i32,
        _stride2: i32,
    ) -> WaylandResult<()> {
        tracing::error!("Tried to create non-prime wl_drm buffer!");
        Ok(())
    }

    async fn create_prime_buffer(
        &self,
        client: &mut Self::Connection,
        _sender_id: ObjectId,
        buffer_id: ObjectId,
        name: OwnedFd,
        width: i32,
        height: i32,
        format: u32,
        offset0: i32,
        stride0: i32,
        _offset1: i32,
        _stride1: i32,
        _offset2: i32,
        _stride2: i32,
    ) -> WaylandResult<()> {
        // TODO: actual error checking
        let Ok(fourcc) = DrmFourcc::try_from(format) else {
            tracing::error!("Failed to convert DrmFourcc");
            return Ok(());
        };
        let _ = DmabufBacking::new(
            vec![DmatexPlane {
                dmabuf_fd: name.into(),
                offset: offset0 as u32,
                row_size: stride0 as u32,
                array_element_size: 0,
                depth_slice_size: 0,
            }],
            72057594037927935, // because drmfourcc is so broken it doesn't actually export this, this is Invalid btw
            Vector2 {
                x: width as u32,
                y: height as u32,
            },
            fourcc,
        )
        .await
        .inspect_err(|e| tracing::error!("Failed to import dmabuf because {e}"))
        .map(|backing| Buffer::new(client, buffer_id, BufferBacking::Dmabuf(backing)));

        Ok(())
    }
}
