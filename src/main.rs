use std::{
    env::args_os,
    fs::OpenOptions,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use pion_binder::PionBinderDevice;
use stardust_xr_fusion::{
    client::{Client, DefaultHandler},
    keymap::KeymapStore,
    project_local_resources,
};
use tracing_subscriber::EnvFilter;
use waynest::ProtocolError;

use crate::{socket::Wayland, vulkan_ctx::VkContext};

pub mod client;
pub mod display;
pub mod error;
pub mod frame_dispatcher;
pub mod panel_item_ui;
pub mod protocols;
pub mod registry;
pub mod signal_on_drop;
pub mod socket;
pub mod util;
pub mod vulkan_ctx;

pub static CLIENT: OnceLock<Arc<Client<DefaultHandler>>> = OnceLock::new();
pub static BINDER_DEV: OnceLock<PionBinderDevice> = OnceLock::new();
pub static KEYMAP_STORE: OnceLock<KeymapStore> = OnceLock::new();

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
    let v = ProtocolError::from(std::io::Error::last_os_error());
    let v = <ProtocolError as From<std::io::Error>>::from(std::io::Error::last_os_error());
    let binder_dev = PionBinderDevice::default();
    // TODO: maybe allow reconnecting to different server? or multi server support?
    let (client, _) = Client::manual_connect(&binder_dev, &[&project_local_resources!("res")])
        .await
        .unwrap();
    let client = Arc::new(client);
    VkContext::init(&client).await;
    _ = CLIENT.set(client.clone());
    _ = BINDER_DEV.set(binder_dev);

    let wayland = Wayland::new(&wayland_socket_path).unwrap();

    let path = stardust_xr_protocol::dir::find_pion_file("stardust-keymap-store").unwrap();
    let fd = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(path)
        .unwrap();
    let obj = client
        .pion_device()
        .get_binder_ref_from_file(fd)
        .await
        .unwrap();
    _ = KEYMAP_STORE.set(KeymapStore::from_object_or_ref(obj));

    tokio::signal::ctrl_c().await.unwrap();
}
