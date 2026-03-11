pub mod buffer_backing;
pub mod buffer_params;
pub mod feedback;

use crate::{
    CLIENT,
    client::Client,
    error::{WaylandError, WaylandResult},
    vulkan_ctx::VK,
};
use buffer_params::BufferParams;
use drm_fourcc::DrmFourcc;
use feedback::DmabufFeedback;
use stardust_xr_cme::format::{DmatexFormat, VulkanoFormatExtension};
use waynest::ObjectId;
use waynest_protocols::server::stable::linux_dmabuf_v1::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use waynest_server::Client as _;

/// Main DMA-BUF interface implementation
///
/// This interface allows clients to create wl_buffers from DMA-BUFs.
/// It handles:
/// - Format/modifier advertisement
/// - Buffer parameter creation
/// - Default/surface-specific feedback
///
/// The implementation ensures:
/// - Coherency for read access in dmabuf data
/// - Proper lifetime management of dmabuf file descriptors
/// - Safe handling of buffer attachments
#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Dmabuf {
    pub(self) version: u32,
    pub(self) formats: Vec<(DrmFourcc, u64)>,
}

impl Dmabuf {
    /// Create a new DMA-BUF interface instance
    pub async fn new(client: &mut Client, id: ObjectId, version: u32) -> WaylandResult<Self> {
        let sd_client = CLIENT.wait();
        let vk = VK.wait();
        let formats = DmatexFormat::enumerate(sd_client, &vk.render_dev)
            .await
            .unwrap()
            .values()
            // we really need something more efficient than this lol
            .filter(|f| format!("{:?}", f.vk_format()).contains("SRGB"))
            .flat_map(|f| {
                f.vk_format()
                    .to_drm_fourcc()
                    .into_iter()
                    .flatten()
                    .cloned()
                    .flat_map(|fourcc| {
                        f.variants()
                            .iter()
                            .map(move |v| (fourcc.clone(), v.modifier))
                    })
            })
            .collect();
        let dmabuf = Self { version, formats };

        if version < 3 {
            for (format, _) in &dmabuf.formats {
                dmabuf.format(client, id, *format as u32).await?;
            }
        }
        // `modifier` is deprecated in version 4
        if version == 3 {
            for (format, modifier) in &dmabuf.formats {
                let format = *format as u32;
                let modifier_hi = (*modifier >> 32) as u32;
                let modifier_lo = *modifier as u32;
                dmabuf
                    .modifier(client, id, format, modifier_hi, modifier_lo)
                    .await?;
            }
        }

        Ok(dmabuf)
    }
}

impl ZwpLinuxDmabufV1 for Dmabuf {
    type Connection = crate::client::Client;

    async fn destroy(
        &self,
        client: &mut Self::Connection,
        sender_id: ObjectId,
    ) -> WaylandResult<()> {
        client.remove(sender_id);
        Ok(())
    }

    async fn create_params(
        &self,
        client: &mut Self::Connection,
        _sender_id: ObjectId,
        params_id: ObjectId,
    ) -> WaylandResult<()> {
        // Create new buffer parameters object
        client.insert(params_id, BufferParams::new(params_id))?;
        Ok(())
    }

    async fn get_default_feedback(
        &self,
        client: &mut Self::Connection,
        sender_id: ObjectId,
        id: ObjectId,
    ) -> WaylandResult<()> {
        if self.version < 3 {
            return Err(WaylandError::Fatal {
                object_id: id,
                code: 71,
                message: "Can't call get_default_feedback on version < 4 of dmabuf",
            });
        }
        // Create feedback object for default (non-surface-specific) settings
        let feedback =
            client.insert(id, DmabufFeedback(client.get::<Dmabuf>(sender_id).unwrap()))?;
        feedback.send_params(client, id).await?;
        Ok(())
    }

    async fn get_surface_feedback(
        &self,
        client: &mut Self::Connection,
        sender_id: ObjectId,
        id: ObjectId,
        _surface: ObjectId,
    ) -> WaylandResult<()> {
        // Create feedback object for surface-specific settings
        // Note: Surface-specific feedback could be optimized based on the surface's
        // requirements, but for now we use the same feedback as default
        self.get_default_feedback(client, sender_id, id).await
    }
}
