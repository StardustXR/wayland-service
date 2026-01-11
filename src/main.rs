use std::sync::OnceLock;

use stardust_xr_fusion::Client;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt;

use crate::{socket::Wayland, vulkan_ctx::VkContext};

pub mod client;
pub mod display;
pub mod protocols;
pub mod registry;
pub mod socket;
pub mod util;
pub mod vulkan_ctx;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_thread_names(true)
        .with_ansi(true)
        .with_line_number(true)
        .init();

    // TODO: maybe allow reconnecting to different server? or multi server support?
    let async_loop = Client::connect().await.unwrap().async_event_loop();
    let client = async_loop.client_handle.clone();
    VkContext::init(&client);

    let wayland = Wayland::new();
}
