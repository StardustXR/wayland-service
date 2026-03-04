use std::{env::args_os, path::PathBuf, sync::{Arc, OnceLock}, time::Duration};

use pion_binder::PionBinderDevice;
use stardust_xr_fusion::{Client, ClientHandle};

use crate::{socket::Wayland, vulkan_ctx::VkContext};

pub mod client;
pub mod display;
pub mod error;
pub mod protocols;
pub mod registry;
pub mod socket;
pub mod util;
pub mod vulkan_ctx;

pub static CLIENT: OnceLock<Arc<ClientHandle>> = OnceLock::new();
pub static BINDER_DEV: OnceLock<PionBinderDevice> = OnceLock::new();

#[tokio::main]
async fn main() {
    let wayland_socket_path = PathBuf::from(args_os().skip(1).next().unwrap());
    let wayland = Wayland::new(&wayland_socket_path).unwrap();
    tracing_subscriber::fmt()
        .with_thread_names(true)
        .with_ansi(true)
        .with_line_number(true)
        .init();

    let binder_dev = PionBinderDevice::default();
    // TODO: maybe allow reconnecting to different server? or multi server support?
    let async_loop = Client::connect().await.unwrap().async_event_loop();
    let client = async_loop.client_handle.clone();
    VkContext::init(&client).await;
    CLIENT.set(client.clone());
    BINDER_DEV.set(binder_dev);

    tokio::time::sleep(Duration::from_secs(10)).await;
}
