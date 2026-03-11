use std::sync::LazyLock;

use stardust_xr_fusion::{
    AsyncEventHandle,
    root::{FrameInfo, Root, RootAspect, RootEvent},
};
use tokio::sync::broadcast;

pub static FRAME_EVENT_PROVIDER: FrameEventProvider = FrameEventProvider::new();
pub struct FrameEventProvider {
    sender: LazyLock<broadcast::Sender<FrameInfo>>,
}
impl FrameEventProvider {
    const fn new() -> Self {
        Self {
            sender: LazyLock::new(|| broadcast::Sender::new(8)),
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<FrameInfo> {
        self.sender.subscribe()
    }
}

pub fn setup_frame_dispatcher(event_handle: AsyncEventHandle, root: Root) {
    tokio::spawn(async move {
        loop {
            event_handle.wait().await;
            let frame_info = match root.recv_root_event() {
                Some(RootEvent::Frame { info }) => info,
                Some(RootEvent::Ping { response }) => {
                    response.send_ok(());
                    continue;
                }
                Some(RootEvent::SaveState { response: _ }) => {
                    // TODO: is there any state we can safe?
                    continue;
                }
                None => continue,
            };
            _ = FRAME_EVENT_PROVIDER.sender.send(frame_info);
        }
    });
}
