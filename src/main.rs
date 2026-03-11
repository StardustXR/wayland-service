use std::{
    env::args_os,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use pion_binder::PionBinderDevice;
use stardust_xr_fusion::{Client, ClientHandle, project_local_resources, zbus::Connection};
use tracing_subscriber::EnvFilter;

use crate::{frame_dispatcher::setup_frame_dispatcher, socket::Wayland, vulkan_ctx::VkContext};

pub mod client;
pub mod display;
pub mod error;
pub mod frame_dispatcher;
pub mod panel_item_ui;
pub mod protocols;
pub mod registry;
pub mod socket;
pub mod util;
pub mod vulkan_ctx;

pub static CLIENT: OnceLock<Arc<ClientHandle>> = OnceLock::new();
pub static BINDER_DEV: OnceLock<PionBinderDevice> = OnceLock::new();
pub static DBUS: OnceLock<Connection> = OnceLock::new();

#[tokio::main]
async fn main() {
    let wayland_socket_path = PathBuf::from(args_os().skip(1).next().unwrap());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .without_time()
        .with_thread_names(true)
        .with_ansi(true)
        .with_line_number(true)
        .init();

    let binder_dev = PionBinderDevice::default();
    let conn = stardust_xr_gluon::connect_client().await.unwrap();
    // TODO: maybe allow reconnecting to different server? or multi server support?
    let client = Client::connect().await.unwrap();
    client
        .setup_resources(&[&project_local_resources!("res")])
        .unwrap();
    let async_loop = client.async_event_loop();
    let client = async_loop.client_handle.clone();
    VkContext::init(&client).await;
    _ = CLIENT.set(client.clone());
    _ = BINDER_DEV.set(binder_dev);
    _ = DBUS.set(conn);
    setup_frame_dispatcher(async_loop.get_event_handle(), client.get_root().clone());

    let _wayland = Wayland::new(&wayland_socket_path).unwrap();

    tokio::signal::ctrl_c().await.unwrap();
}
