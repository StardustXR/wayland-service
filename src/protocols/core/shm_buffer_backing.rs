use crate::{CLIENT, vulkan_ctx::VK};

use super::shm_pool::ShmPool;
use mint::Vector2;
use stardust_xr_cme::dmatex::Dmatex;
use stardust_xr_cme::format::DmatexFormat;
use stardust_xr_fusion::drawable::{DmatexSize, export_dmatex_uid};
use stardust_xr_fusion::node::NodeError;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use thiserror::Error;
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tokio::sync::mpsc;
use vulkano::DeviceSize;
use vulkano::buffer::{Buffer, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{
    AutoCommandBufferBuilder, CommandBufferSubmitInfo, CommandBufferUsage, CopyBufferToImageInfo,
    SemaphoreSubmitInfo, SubmitInfo,
};
use vulkano::format::Format as VkFormat;
use vulkano::image::ImageUsage;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::sync::semaphore::{
    ExternalSemaphoreHandleType, ExternalSemaphoreHandleTypes, Semaphore, SemaphoreCreateInfo,
};
use waynest_protocols::server::core::wayland::wl_shm::Format;

/// Parameters for a shared memory buffer
pub struct ShmBufferBacking {
    pool: Arc<ShmPool>,
    offset: usize,
    stride: usize,
    size: Vector2<u64>,
    wl_format: Format,
    dmatex: Arc<Dmatex>,
    dmatex_uid: u64,
    staging_buffer: Subbuffer<[u8]>,
    next_acquire_point: AtomicU64,
    timeline_copy: Arc<TimelineSyncObj>,
}

impl std::fmt::Debug for ShmBufferBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmBufferBacking")
            .field("pool", &self.pool)
            .field("offset", &self.offset)
            .field("stride", &self.stride)
            .field("size", &self.size)
            .field("wl_format", &self.wl_format)
            .field("dmatex_uid", &self.dmatex_uid)
            .field("staging_buffer", &self.staging_buffer)
            .finish()
    }
}

impl ShmBufferBacking {
    pub async fn new(
        pool: Arc<ShmPool>,
        offset: usize,
        stride: usize,
        size: Vector2<u64>,
        wl_format: Format,
    ) -> Result<Self, ShmBackingCreationError> {
        let client = CLIENT.wait();
        let vk = VK.wait();
        let texture_format = match wl_format {
            Format::Argb8888 | Format::Xrgb8888 => VkFormat::B8G8R8A8_SRGB,
            _ => return Err(ShmBackingCreationError::UnsupportedFormat),
        };
        // let formats =
        let format = DmatexFormat::enumerate(client, &vk.render_dev)
            .await
            .map_err(ShmBackingCreationError::DmatexFormatEnumerationFailed)?
            .get(&texture_format)
            .cloned()
            .ok_or(ShmBackingCreationError::FormatNotSupportedByDmatex)?;
        let dmatex = Arc::new(Dmatex::new(
            client,
            &vk.dev,
            &vk.render_dev,
            DmatexSize::Dim2D([size.x as u32, size.y as u32].into()),
            &format,
            None,
            ImageUsage::TRANSFER_DST,
        ));

        let staging_buffer = Buffer::new_slice::<u8>(
            vk.mem_alloc.clone(),
            BufferCreateInfo {
                usage: BufferUsage::TRANSFER_SRC,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            // when supporting more formats we need to change this 4
            size.x * size.y * 4 as DeviceSize,
        )
        // To lazy to properly bubble up the error rn
        .unwrap();
        let dmatex_uid = export_dmatex_uid(client, dmatex.dmatex_id).await.unwrap();
        let timeline_copy = Arc::new(
            TimelineSyncObj::import(
                vk.render_dev.drm_node(),
                dmatex.timeline.export().unwrap().as_fd(),
            )
            .unwrap(),
        );
        Ok(Self {
            pool,
            offset,
            stride,
            size,
            wl_format,
            dmatex,
            staging_buffer,
            dmatex_uid,
            timeline_copy,
            next_acquire_point: AtomicU64::new(0),
        })
    }

    pub fn update(&self) -> (u64, u64, u64) {
        let acquire = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
        let release = self.next_acquire_point.fetch_add(1, Ordering::Relaxed);
        // TODO: move this to a blocking thread
        {
            let mut writer = self.staging_buffer.write().unwrap();

            let shm_data = self.pool.data_lock();
            for y in 0..self.size.y {
                let shm_offset = self.offset + (y as usize * self.stride);
                let gpu_offset = (y * self.size.x * 4) as usize;
                let line_len = (self.size.x * 4) as usize;

                writer[gpu_offset..(gpu_offset + line_len)]
                    .copy_from_slice(&shm_data[shm_offset..(shm_offset + line_len)]);
            }
        }
        UPLOAD_QUEUE
            .send(DmatexUpload {
                tex: self.dmatex.clone(),
                staging: self.staging_buffer.clone(),
                acquire,
            })
            .unwrap();
        // self.staging_buffer.
        (self.dmatex_uid, acquire, release)
    }

    pub fn timeline(&self) -> Arc<TimelineSyncObj> {
        self.timeline_copy.clone()
    }

    pub fn is_transparent(&self) -> bool {
        match self.wl_format {
            Format::Xrgb8888 => false,
            Format::Argb8888 => true,
            _ => true,
        }
    }

    pub fn size(&self) -> Vector2<usize> {
        [self.size.x as usize, self.size.y as usize].into()
    }
}

#[derive(Debug, Error)]
pub enum ShmBackingCreationError {
    #[error("Format not supported")]
    UnsupportedFormat,
    #[error("Dmatex format enumeration failed: {0}")]
    DmatexFormatEnumerationFailed(NodeError),
    #[error("Format not supported by Dmatex")]
    FormatNotSupportedByDmatex,
}

struct DmatexUpload {
    tex: Arc<Dmatex>,
    staging: Subbuffer<[u8]>,
    acquire: u64,
}
static UPLOAD_QUEUE: LazyLock<mpsc::UnboundedSender<DmatexUpload>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::unbounded_channel();
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let _guard = runtime.enter();
        dmatex_upload_task(rx)
    });
    tx
});
fn dmatex_upload_task(mut receiver: mpsc::UnboundedReceiver<DmatexUpload>) {
    let mut uploads = Vec::new();
    let vk = VK.wait();
    loop {
        let n = receiver.blocking_recv_many(&mut uploads, 16);
        if n == 0 {
            tracing::error!("dmatex upload channel somehow closed");
            break;
        }
        let mut cmd_buf = AutoCommandBufferBuilder::primary(
            vk.cballoc.clone(),
            vk.queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();
        let mut semaphores = Vec::with_capacity(uploads.len());
        for upload in uploads.iter() {
            cmd_buf
                .copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(
                    upload.staging.clone(),
                    upload.tex.image.clone(),
                ))
                .unwrap();
            semaphores.push(Arc::new(
                Semaphore::new(
                    vk.dev.clone(),
                    SemaphoreCreateInfo {
                        export_handle_types: ExternalSemaphoreHandleTypes::SYNC_FD,
                        ..Default::default()
                    },
                )
                .unwrap(),
            ));
        }
        let buf = cmd_buf.build().unwrap();
        vk.queue.with(|mut queue| unsafe {
            queue
                .submit(
                    &[SubmitInfo {
                        command_buffers: vec![CommandBufferSubmitInfo::new(buf)],
                        signal_semaphores: semaphores
                            .iter()
                            .cloned()
                            .map(SemaphoreSubmitInfo::new)
                            .collect(),
                        ..Default::default()
                    }],
                    None,
                )
                .unwrap()
        });
        for (upload, semaphore) in uploads.drain(..).zip(semaphores.into_iter()) {
            tokio::spawn(async move {
                let fd =
                    unsafe { semaphore.export_fd(ExternalSemaphoreHandleType::SyncFd) }.unwrap();
                upload
                    .tex
                    .timeline
                    .import_sync_file_point(fd.as_fd(), upload.acquire)
                    .unwrap();
                upload
                    .tex
                    .timeline
                    .wait_async(upload.acquire)
                    .unwrap()
                    .await;
            });
        }
    }
}
