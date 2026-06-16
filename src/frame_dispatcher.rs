use stardust_xr_fusion::client::FrameInfo;
use tokio::sync::broadcast;

use crate::CLIENT;

pub static FRAME_EVENT_PROVIDER: FrameEventProvider = FrameEventProvider::new();
pub struct FrameEventProvider {}
impl FrameEventProvider {
    const fn new() -> Self {
        Self {}
    }
    pub fn subscribe(&self) -> broadcast::Receiver<FrameInfo> {
        tracing::info!("subscribing to frame event");
        CLIENT.wait().frame_receiver()
    }
}
