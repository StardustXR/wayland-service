use std::sync::{Arc, OnceLock};

use stardust_xr_cme::{dmatex::Dmatex, render_device::RenderDevice};
use stardust_xr_fusion::ClientHandle;
use tracing::{debug, error, info, warn};
use vulkano::{
    VulkanLibrary,
    command_buffer::allocator::StandardCommandBufferAllocator,
    device::{
        Device, DeviceCreateInfo, Queue, QueueCreateInfo, QueueFlags, physical::PhysicalDevice,
    },
    instance::{
        Instance, InstanceExtensions,
        debug::{
            DebugUtilsMessageSeverity, DebugUtilsMessageType, DebugUtilsMessengerCallback,
            DebugUtilsMessengerCallbackData, DebugUtilsMessengerCreateInfo,
        },
    },
    memory::allocator::StandardMemoryAllocator,
};

pub struct VkContext {
    pub render_dev: RenderDevice,
    pub instance: Arc<Instance>,
    pub phys_dev: Arc<PhysicalDevice>,
    pub dev: Arc<Device>,
    pub queue: Arc<Queue>,
    pub cballoc: Arc<StandardCommandBufferAllocator>,
    pub mem_alloc: Arc<StandardMemoryAllocator>,
}
pub static VK: OnceLock<VkContext> = OnceLock::new();
impl VkContext {
    // TODO: proper error handling?
    pub async fn init(client: &Arc<ClientHandle>) {
        let render_dev = RenderDevice::primary_server_device(client).await.unwrap();
        let entry = VulkanLibrary::new().unwrap();
        let debug_callback = unsafe { DebugUtilsMessengerCallback::new(debug_callback) };
        let instance = Instance::new(
            entry,
            vulkano::instance::InstanceCreateInfo {
                application_name: Some("Stardust XR wayland compositor service".to_string()),
                enabled_extensions: InstanceExtensions {
                    ext_debug_utils: true,
                    ..InstanceExtensions::empty()
                } | Dmatex::required_instance_exts(),
                debug_utils_messengers: vec![DebugUtilsMessengerCreateInfo::user_callback(
                    debug_callback,
                )],
                ..Default::default()
            },
        )
        .unwrap();
        let phys_dev = render_dev.get_physical_device(&instance).unwrap();

        let queue_family_index = phys_dev
            .queue_family_properties()
            .iter()
            .enumerate()
            .find(|(_, p)| {
                p.queue_flags.contains(QueueFlags::TRANSFER)
                    && !p.queue_flags.contains(QueueFlags::PROTECTED)
            })
            .unwrap()
            .0 as u32;
        let (dev, mut queues) = Device::new(
            phys_dev.clone(),
            DeviceCreateInfo {
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index,
                    ..Default::default()
                }],
                enabled_extensions: Dmatex::required_device_exts(),
                enabled_features: Dmatex::required_device_features(),
                ..Default::default()
            },
        )
        .unwrap();
        let queue = queues.next().unwrap();
        let cballoc = Arc::new(StandardCommandBufferAllocator::new(
            dev.clone(),
            Default::default(),
        ));
        let mem_alloc = Arc::new(StandardMemoryAllocator::new_default(dev.clone()));
        _ = VK.set(Self {
            render_dev,
            instance,
            phys_dev,
            dev,
            queue,
            cballoc,
            mem_alloc,
        });
    }
    pub fn get() -> &'static Self {
        VK.wait()
    }
}

fn debug_callback(
    level: DebugUtilsMessageSeverity,
    msg_type: DebugUtilsMessageType,
    data: DebugUtilsMessengerCallbackData,
) {
    let msg_type = match msg_type {
        DebugUtilsMessageType::VALIDATION => "Validation",
        DebugUtilsMessageType::PERFORMANCE => "Performance",
        DebugUtilsMessageType::GENERAL => "Misc",
        _ => "Unknown",
    };
    match level {
        DebugUtilsMessageSeverity::ERROR => {
            error!("VK-{msg_type}: {}", data.message)
        }
        DebugUtilsMessageSeverity::WARNING => {
            warn!("VK-{msg_type}: {}", data.message)
        }
        DebugUtilsMessageSeverity::INFO => {
            info!("VK-{msg_type}: {}", data.message)
        }
        DebugUtilsMessageSeverity::VERBOSE => {
            debug!("VK-{msg_type}: {}", data.message)
        }
        _ => {}
    }
}
