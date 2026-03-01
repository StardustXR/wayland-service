use std::{env::args_os, path::PathBuf, time::Duration};

use stardust_xr_fusion::Client;

use crate::{socket::Wayland, vulkan_ctx::VkContext};

pub mod client;
pub mod display;
pub mod error;
pub mod protocols;
pub mod registry;
pub mod socket;
pub mod util;
pub mod vulkan_ctx;

#[tokio::main]
async fn main() {
    let wayland_socket_path = PathBuf::from(args_os().skip(1).next().unwrap());
    let wayland = Wayland::new(&wayland_socket_path).unwrap();
    tracing_subscriber::fmt()
        .with_thread_names(true)
        .with_ansi(true)
        .with_line_number(true)
        .init();

    // TODO: maybe allow reconnecting to different server? or multi server support?
    let async_loop = Client::connect().await.unwrap().async_event_loop();
    let client = async_loop.client_handle.clone();
    VkContext::init(&client).await;

    tokio::time::sleep(Duration::from_secs(10)).await;
}
