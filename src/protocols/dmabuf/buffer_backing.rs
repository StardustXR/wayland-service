use crate::{CLIENT, vulkan_ctx::VK};

use super::buffer_params::BufferParams;
use drm_fourcc::DrmFourcc;
use mint::Vector2;
use stardust_xr_fusion::{
    drawable::{self, DmatexPlane, DmatexSize},
    node::NodeError,
};
use std::{
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::io::unix::AsyncFd;
use waynest_protocols::server::stable::linux_dmabuf_v1::zwp_linux_buffer_params_v1::Flags;

/// Parameters for a shared memory buffer
#[derive(Debug)]
pub struct DmabufBacking {
    size: Vector2<u32>,
    format: DrmFourcc,
    _modifier: u64,
    timeline: Arc<TimelineSyncObj>,
    fds: Arc<Vec<AsyncFd<OwnedFd>>>,
    dmatex_id: u64,
    dmatex_uid: u64,
    next_acquire_point: AtomicU64,
}

impl DmabufBacking {
    pub async fn new(
        planes: Vec<DmatexPlane>,
        modifier: u64,
        size: Vector2<u32>,
        format: DrmFourcc,
    ) -> Result<Self, DmatexImportError> {
        tracing::info!("Creating new DmabufBacking");
        let client = CLIENT.wait();
        let vk = VK.wait();
        let dmatex_id = client.generate_id();
        let timeline = Arc::new(
            TimelineSyncObj::create(vk.render_dev.drm_node())
                .map_err(DmatexImportError::TimelineCreationError)?,
        );
        drawable::import_dmatex(
            client,
            dmatex_id,
            DmatexSize::Dim2D(size),
            format as u32,
            modifier,
            true,
            None,
            &planes,
            timeline
                .export()
                .map_err(DmatexImportError::TimelineExportError)?
                .into(),
        )
        .unwrap();
        let dmatex_uid = drawable::export_dmatex_uid(client, dmatex_id)
            .await
            .map_err(DmatexImportError::DmatexExportError)?;
        let fds = planes
            .iter()
            .map(|v| {
                v.dmabuf_fd
                    .0
                    .try_clone()
                    .map(|fd| AsyncFd::new(fd).unwrap())
                    .map_err(DmatexImportError::DmabufFdCloneError)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into();

        Ok(DmabufBacking {
            size,
            format,
            dmatex_uid,
            timeline,
            dmatex_id,
            _modifier: modifier,
            next_acquire_point: AtomicU64::new(0),
            fds,
        })
    }
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn from_params(
        params: Arc<BufferParams>,
        size: Vector2<u32>,
        format: DrmFourcc,
        _flags: Flags,
    ) -> Result<Self, DmatexImportError> {
        let mut planes = Vec::from_iter(std::mem::take(&mut *params.planes.lock()));
        planes.sort_by_key(|(index, _)| *index);
        let planes = planes.into_iter().map(|(_, tex)| tex).collect::<Vec<_>>();
        let modifier = *params.modifier.get().ok_or(DmatexImportError::NoModifier)?;
        Self::new(planes, modifier, size, format).await
    }

    pub fn update(&self) -> (u64, u64, u64) {
        let acquire = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
        let release = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
        tokio::spawn({
            let fds = self.fds.clone();
            let timeline = self.timeline.clone();
            async move {
                for fd in fds.iter() {
                    _ = fd.readable().await;
                }
                unsafe {
                    _ = timeline.signal(acquire);
                }
            }
        });
        (self.dmatex_uid, acquire, release)
    }

    pub fn timeline(&self) -> Arc<TimelineSyncObj> {
        self.timeline.clone()
    }

    pub fn is_transparent(&self) -> bool {
        matches!(
            self.format,
            DrmFourcc::Abgr1555
                | DrmFourcc::Abgr16161616f
                | DrmFourcc::Abgr2101010
                | DrmFourcc::Abgr4444
                | DrmFourcc::Abgr8888
                | DrmFourcc::Argb1555
                | DrmFourcc::Argb16161616f
                | DrmFourcc::Argb2101010
                | DrmFourcc::Argb4444
                | DrmFourcc::Argb8888
                | DrmFourcc::Axbxgxrx106106106106
                | DrmFourcc::Ayuv
                | DrmFourcc::Rgba1010102
                | DrmFourcc::Rgba4444
                | DrmFourcc::Rgba5551
                | DrmFourcc::Rgba8888
        )
    }

    pub fn size(&self) -> Vector2<usize> {
        [self.size.x as usize, self.size.y as usize].into()
    }
}
impl Drop for DmabufBacking {
    fn drop(&mut self) {
        _ = drawable::unregister_dmatex(CLIENT.wait(), self.dmatex_id);
    }
}
#[derive(Debug, thiserror::Error)]
pub enum DmatexImportError {
    #[error("Format modifier combination not found")]
    InvalidFormat,
    #[error("No modifier (no planes)")]
    NoModifier,
    #[error("Failed to enumerate Server Dmatex formats: {0}")]
    FailedToEnumerateServerFormats(NodeError),
    #[error("Failed to export Dmatex: {0}")]
    DmatexExportError(NodeError),
    #[error("Failed to create TimelineSyncObj: {0}")]
    TimelineCreationError(rustix::io::Errno),
    #[error("Failed to export TimelineSyncObj: {0}")]
    TimelineExportError(rustix::io::Errno),
    #[error("Failed clone Dmabuf fd: {0}")]
    DmabufFdCloneError(std::io::Error),
}
