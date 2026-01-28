use std::time::Duration;

use stardust_xr_fusion::Client;

use crate::{socket::Wayland, vulkan_ctx::VkContext};

// pub mod client;
// pub mod display;
// pub mod protocols;
// pub mod registry;
pub mod socket;
// pub mod util;
pub mod error;
pub mod vulkan_ctx;

#[tokio::main]
async fn main() {
    let wayland = Wayland::new().unwrap();
    println!(
        "WAYLAND_DISPLAY={}",
        wayland.socket_path().file_name().unwrap().to_str().unwrap()
    );
    // tracing_subscriber::fmt()
    //     .with_thread_names(true)
    //     .with_ansi(true)
    //     .with_line_number(true)
    //     .init();

    // TODO: maybe allow reconnecting to different server? or multi server support?
    let async_loop = Client::connect().await.unwrap().async_event_loop();
    let client = async_loop.client_handle.clone();
    VkContext::init(&client).await;

    tokio::time::sleep(Duration::from_secs(10)).await;
}
