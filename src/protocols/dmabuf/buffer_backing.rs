use crate::{CLIENT, vulkan_ctx::VK};

use super::buffer_params::BufferParams;
use drm_fourcc::DrmFourcc;
use mint::Vector2;
use stardust_xr_fusion::dmatex::{
    AlphaMode, DisjointDmatexPlane, DmatexExt, DmatexFormat, DmatexPlanes, DmatexRef, DmatexSize,
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
    timeline: Arc<TimelineSyncObj>,
    fds: Arc<Vec<AsyncFd<OwnedFd>>>,
    dmatex: DmatexRef,
    next_acquire_point: AtomicU64,
}

impl DmabufBacking {
    pub async fn new(
        planes: Vec<DisjointDmatexPlane>,
        modifier: u64,
        size: Vector2<u32>,
        format: DrmFourcc,
    ) -> Result<Self, DmatexImportError> {
        tracing::info!("Creating new DmabufBacking");
        let client = CLIENT.wait();
        let vk = VK.wait();
        let timeline = Arc::new(
            TimelineSyncObj::new(vk.render_dev.drm_node())
                .map_err(DmatexImportError::TimelineCreationError)?,
        );
        let mut last_dev_ino = None;
        let mut disjoint = false;
        for dev_ino in planes
            .iter()
            .filter_map(|p| rustix::fs::fstat(&p.dmabuf_fd).ok())
            .map(|stat| (stat.st_dev, stat.st_ino))
        {
            if let Some(last_dev_ino) = last_dev_ino {
                if last_dev_ino != dev_ino {
                    disjoint = true;
                    break;
                }
            }
            last_dev_ino = Some(dev_ino);
        }
        let planes = if disjoint {
            DmatexPlanes::Disjoint { planes }
        } else {
            DmatexPlanes::Simple {
                planes: planes.iter().map(|v| v.plane).collect(),
                dmabuf_fd: planes.into_iter().next().unwrap().dmabuf_fd,
            }
        };
        let fds = match &planes {
            DmatexPlanes::Simple {
                dmabuf_fd,
                planes: _,
            } => vec![
                AsyncFd::new(
                    dmabuf_fd
                        .try_clone()
                        .map_err(DmatexImportError::DmabufFdCloneError)?,
                )
                .unwrap(),
            ]
            .into(),
            DmatexPlanes::Disjoint { planes } => planes
                .iter()
                .map(|v| {
                    v.dmabuf_fd
                        .try_clone()
                        .map(|fd| AsyncFd::new(fd).unwrap())
                        .map_err(DmatexImportError::DmabufFdCloneError)
                })
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        };
        let dmatex = DmatexRef::import(
            client,
            DmatexSize::Size2D { size },
            DmatexFormat {
                drm_fourcc: format as u32,
                drm_modifier: modifier,
                is_srgb: true,
                alpha_mode: AlphaMode::PremultipliedElectrical,
                ycbcr_info: None,
            },
            1,
            planes,
            timeline
                .export()
                .map_err(DmatexImportError::TimelineExportError)?,
        )
        .await
        .map_err(DmatexImportError::DmatexImportError)?;

        Ok(DmabufBacking {
            size,
            format,
            timeline,
            fds,
            dmatex,
            next_acquire_point: AtomicU64::new(0),
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

    pub fn update(&self) -> (DmatexRef, u64, u64) {
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
        (self.dmatex.clone(), acquire, release)
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
#[derive(Debug, thiserror::Error)]
pub enum DmatexImportError {
    #[error("Format modifier combination not found")]
    InvalidFormat,
    #[error("No modifier (no planes)")]
    NoModifier,
    #[error("Failed to enumerate Server Dmatex formats: {0}")]
    FailedToEnumerateServerFormats(stardust_xr_fusion::Error),
    #[error("Failed to import Dmatex into server: {0}")]
    DmatexImportError(stardust_xr_fusion::Error),
    #[error("Failed to create TimelineSyncObj: {0}")]
    TimelineCreationError(rustix::io::Errno),
    #[error("Failed to export TimelineSyncObj: {0}")]
    TimelineExportError(rustix::io::Errno),
    #[error("Failed clone Dmabuf fd: {0}")]
    DmabufFdCloneError(std::io::Error),
}
