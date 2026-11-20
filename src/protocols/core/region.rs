use crate::error::{WaylandError, WaylandResult};
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_panels::panel_item::Rect;
use waynest::ObjectId;
pub use waynest_protocols::server::core::wayland::wl_region::*;
use waynest_server::{Client as _, RequestDispatcher};

/// surface pixels, max is exclusive
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
	pub min: Vector2<u32>,
	pub max: Vector2<u32>,
}
impl Patch {
	/// nothing above or left of the surface ever gets input, so clipping there early is lossless
	fn clipped(x: i32, y: i32, w: i32, h: i32) -> Option<Self> {
		let edge = |v: i32, len: i32| {
			let lo = v.max(0) as i64;
			let hi = (v as i64 + len as i64).clamp(0, u32::MAX as i64);
			(lo < hi).then_some((lo as u32, hi as u32))
		};
		let (x0, x1) = edge(x, w)?;
		let (y0, y1) = edge(y, h)?;
		Some(Self {
			min: [x0, y0].into(),
			max: [x1, y1].into(),
		})
	}
	pub fn rect(&self) -> Rect {
		Rect {
			origin: [self.min.x as f32, self.min.y as f32].into(),
			size: [
				(self.max.x - self.min.x) as f32,
				(self.max.y - self.min.y) as f32,
			]
			.into(),
		}
	}
	fn overlaps(&self, o: &Self) -> bool {
		self.min.x < o.max.x && o.min.x < self.max.x && self.min.y < o.max.y && o.min.y < self.max.y
	}
	/// up to 4 patches, full width bands above and below then the sides
	fn minus(self, o: &Self) -> Vec<Self> {
		if !self.overlaps(o) {
			return vec![self];
		}
		let (y0, y1) = (o.min.y.max(self.min.y), o.max.y.min(self.max.y));
		let candidates = [
			(self.min.x, self.min.y, self.max.x, y0),
			(self.min.x, y1, self.max.x, self.max.y),
			(self.min.x, y0, o.min.x.max(self.min.x), y1),
			(o.max.x.min(self.max.x), y0, self.max.x, y1),
		];
		candidates
			.into_iter()
			.filter(|(x0, y0, x1, y1)| x0 < x1 && y0 < y1)
			.map(|(x0, y0, x1, y1)| Self {
				min: [x0, y0].into(),
				max: [x1, y1].into(),
			})
			.collect()
	}
}

/// kept as disjoint patches so a later subtract only ever has to split
#[derive(Debug, RequestDispatcher)]
#[waynest(error = WaylandError, connection = crate::client::Client)]
pub struct Region {
	id: ObjectId,
	patches: Mutex<Vec<Patch>>,
}
impl Region {
	pub fn new(id: ObjectId) -> Self {
		Self {
			id,
			patches: Mutex::new(Vec::new()),
		}
	}
	pub fn patches(&self) -> Vec<Patch> {
		self.patches.lock().clone()
	}
	fn cut(&self, p: &Patch) {
		let mut patches = self.patches.lock();
		*patches = patches.drain(..).flat_map(|q| q.minus(p)).collect();
	}
}
impl WlRegion for Region {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_region:request:add
	async fn add(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		x: i32,
		y: i32,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		if let Some(p) = Patch::clipped(x, y, width, height) {
			self.cut(&p);
			self.patches.lock().push(p);
		}
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_region:request:subtract
	async fn subtract(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		x: i32,
		y: i32,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		if let Some(p) = Patch::clipped(x, y, width, height) {
			self.cut(&p);
		}
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_region:request:destroy
	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}
