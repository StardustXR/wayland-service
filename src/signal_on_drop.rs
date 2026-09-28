use std::{
	future::ready,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use gluon_ipc::{Handler, RefExt};
use stardust_xr_fusion::dmatex::{
	DmatexSubmitRelease, DmatexSubmitReleaseHandler, DmatexSubmitReleaseLocal,
};
use timeline_syncobj::timeline_syncobj::TimelineSyncObj;
use tracing::{debug, error, warn};

#[derive(Handler, Debug)]
pub struct SignalOnDrop {
	timeline: Arc<TimelineSyncObj>,
	point: u64,
	consumed: AtomicBool,
}
impl SignalOnDrop {
	/// on failure the handler drops right away and signals the point, so the buffer still releases
	pub fn new(
		timeline: Arc<TimelineSyncObj>,
		point: u64,
	) -> Option<DmatexSubmitReleaseLocal<Self>> {
		DmatexSubmitRelease::new_service(Self {
			timeline,
			point,
			consumed: AtomicBool::new(false),
		})
		.inspect_err(|e| error!("failed to create buffer release service: {e}"))
		.ok()
	}
	pub fn timeline(&self) -> &Arc<TimelineSyncObj> {
		&self.timeline
	}
	pub fn consumed(&self) -> bool {
		self.consumed.load(Ordering::Relaxed)
	}
}

impl DmatexSubmitReleaseHandler for SignalOnDrop {
	fn consume(&self, _ctx: gluon_ipc::Context) -> impl Future<Output = u64> + Send + Sync {
		debug!("consuming signal on drop");
		self.consumed.store(true, Ordering::Relaxed);
		ready(self.point)
	}
}
impl Drop for SignalOnDrop {
	fn drop(&mut self) {
		if !self.consumed.load(Ordering::Relaxed) {
			warn!("SignalOnDrop dropped without being consumed");
			_ = unsafe { self.timeline.signal(self.point) };
		}
	}
}
