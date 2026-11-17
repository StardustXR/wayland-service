use crate::error::{WaylandError, WaylandResult};
use mint::Vector2;
use parking_lot::Mutex;
use stardust_xr_panels::panel_item::Geometry;
use waynest::ObjectId;
use waynest_protocols::server::stable::xdg_shell::{
	xdg_positioner::*, xdg_wm_base::Error as WmBaseError,
};
use waynest_server::Client as _;

#[derive(Debug, Clone, Copy)]
pub struct PositionerData {
	pub size: Vector2<u32>,
	pub anchor_rect: Geometry,
	pub offset: Vector2<i32>,
	pub anchor: Anchor,
	pub gravity: Gravity,
	pub constraint_adjustment: ConstraintAdjustment,
	pub reactive: bool,
	pub parent_size: Vector2<u32>,
}
impl PositionerData {
	fn gravity_has_edge(&self, edge: Gravity) -> bool {
		match edge {
			Gravity::Top => {
				self.gravity == Gravity::Top
					|| self.gravity == Gravity::TopLeft
					|| self.gravity == Gravity::TopRight
			}
			Gravity::Bottom => {
				self.gravity == Gravity::Bottom
					|| self.gravity == Gravity::BottomLeft
					|| self.gravity == Gravity::BottomRight
			}
			Gravity::Left => {
				self.gravity == Gravity::Left
					|| self.gravity == Gravity::TopLeft
					|| self.gravity == Gravity::BottomLeft
			}
			Gravity::Right => {
				self.gravity == Gravity::Right
					|| self.gravity == Gravity::TopRight
					|| self.gravity == Gravity::BottomRight
			}
			_ => {
				tracing::error!("gravity_has_edge called with a non-edge gravity");
				false
			}
		}
	}
	pub fn infinite_geometry(&self) -> Geometry {
		let anchor_point: Vector2<i32> = match self.anchor {
			Anchor::TopLeft => self.anchor_rect.origin,
			Anchor::Top => [
				self.anchor_rect.origin.x + (self.anchor_rect.size.x / 2) as i32,
				self.anchor_rect.origin.y,
			]
			.into(),
			Anchor::TopRight => [
				self.anchor_rect.origin.x + self.anchor_rect.size.x as i32,
				self.anchor_rect.origin.y,
			]
			.into(),
			Anchor::Left => [
				self.anchor_rect.origin.x,
				self.anchor_rect.origin.y + (self.anchor_rect.size.y / 2) as i32,
			]
			.into(),
			Anchor::Right => [
				self.anchor_rect.origin.x + self.anchor_rect.size.x as i32,
				self.anchor_rect.origin.y + (self.anchor_rect.size.y / 2) as i32,
			]
			.into(),
			Anchor::BottomLeft => [
				self.anchor_rect.origin.x,
				self.anchor_rect.origin.y + self.anchor_rect.size.y as i32,
			]
			.into(),
			Anchor::Bottom => [
				self.anchor_rect.origin.x + (self.anchor_rect.size.x / 2) as i32,
				self.anchor_rect.origin.y + self.anchor_rect.size.y as i32,
			]
			.into(),
			Anchor::BottomRight => [
				self.anchor_rect.origin.x + self.anchor_rect.size.x as i32,
				self.anchor_rect.origin.y + self.anchor_rect.size.y as i32,
			]
			.into(),
			_ => [
				self.anchor_rect.origin.x + (self.anchor_rect.size.x / 2) as i32,
				self.anchor_rect.origin.y + (self.anchor_rect.size.y / 2) as i32,
			]
			.into(),
		};

		let mut geometry = Geometry {
			origin: Vector2 {
				x: anchor_point.x + self.offset.x,
				y: anchor_point.y + self.offset.y,
			},
			size: self.size,
		};

		// apply gravity
		if self.gravity_has_edge(Gravity::Top) {
			geometry.origin.y -= geometry.size.y as i32;
		} else if !self.gravity_has_edge(Gravity::Bottom) {
			geometry.origin.y -= (geometry.size.y / 2) as i32;
		}

		if self.gravity_has_edge(Gravity::Left) {
			geometry.origin.x -= geometry.size.x as i32;
		} else if !self.gravity_has_edge(Gravity::Right) {
			geometry.origin.x -= (geometry.size.x / 2) as i32;
		}

		geometry
	}
}

/// what the client has set so far, there's no sensible default for size or anchor rect
/// so a positioner only becomes usable once both are in
#[derive(Debug, Clone, Copy)]
struct UncheckedPositioner {
	size: Option<Vector2<u32>>,
	anchor_rect: Option<Geometry>,
	offset: Vector2<i32>,
	anchor: Anchor,
	gravity: Gravity,
	constraint_adjustment: ConstraintAdjustment,
	reactive: bool,
	parent_size: Vector2<u32>,
}
impl Default for UncheckedPositioner {
	fn default() -> Self {
		Self {
			size: None,
			anchor_rect: None,
			offset: [0, 0].into(),
			anchor: Anchor::TopLeft,
			gravity: Gravity::TopLeft,
			constraint_adjustment: ConstraintAdjustment::empty(),
			reactive: false,
			parent_size: [0; 2].into(),
		}
	}
}
impl UncheckedPositioner {
	fn check(&self) -> Option<PositionerData> {
		Some(PositionerData {
			size: self.size?,
			anchor_rect: self.anchor_rect?,
			offset: self.offset,
			anchor: self.anchor,
			gravity: self.gravity,
			constraint_adjustment: self.constraint_adjustment,
			reactive: self.reactive,
			parent_size: self.parent_size,
		})
	}
}

#[derive(Debug, waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Positioner {
	draft: Mutex<UncheckedPositioner>,
	id: ObjectId,
}
impl Positioner {
	pub fn new(id: ObjectId) -> Self {
		Self {
			draft: Mutex::new(UncheckedPositioner::default()),
			id,
		}
	}

	// the spec puts invalid_positioner on xdg_wm_base, not on the positioner itself
	pub fn complete(&self, wm_base: ObjectId) -> WaylandResult<PositionerData> {
		self.draft.lock().check().ok_or(WaylandError::Fatal {
			object_id: wm_base,
			code: WmBaseError::InvalidPositioner as u32,
			message: "positioner needs both a size and an anchor rect",
		})
	}

	fn invalid_input(&self, message: &'static str) -> WaylandError {
		WaylandError::Fatal {
			object_id: self.id,
			code: Error::InvalidInput as u32,
			message,
		}
	}
}
impl XdgPositioner for Positioner {
	type Connection = crate::client::Client;

	async fn set_size(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		if width <= 0 || height <= 0 {
			return Err(self.invalid_input("positioner size must be positive"));
		}
		self.draft.lock().size = Some([width as u32, height as u32].into());
		Ok(())
	}

	async fn set_anchor_rect(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		x: i32,
		y: i32,
		width: i32,
		height: i32,
	) -> WaylandResult<()> {
		if width < 0 || height < 0 {
			return Err(self.invalid_input("anchor rect size can't be negative"));
		}
		self.draft.lock().anchor_rect = Some(Geometry {
			origin: [x, y].into(),
			size: [width as u32, height as u32].into(),
		});
		Ok(())
	}

	async fn set_anchor(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		anchor: Anchor,
	) -> WaylandResult<()> {
		self.draft.lock().anchor = anchor;
		Ok(())
	}

	async fn set_gravity(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		gravity: Gravity,
	) -> WaylandResult<()> {
		self.draft.lock().gravity = gravity;
		Ok(())
	}

	async fn set_constraint_adjustment(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		constraint_adjustment: ConstraintAdjustment,
	) -> WaylandResult<()> {
		self.draft.lock().constraint_adjustment = constraint_adjustment;
		Ok(())
	}

	async fn set_offset(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		x: i32,
		y: i32,
	) -> WaylandResult<()> {
		self.draft.lock().offset = [x, y].into();
		Ok(())
	}

	async fn set_reactive(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		self.draft.lock().reactive = true;
		Ok(())
	}

	async fn set_parent_size(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		parent_width: i32,
		parent_height: i32,
	) -> WaylandResult<()> {
		self.draft.lock().parent_size =
			[parent_width.max(0) as u32, parent_height.max(0) as u32].into();
		Ok(())
	}

	async fn set_parent_configure(
		&self,
		_client: &mut Self::Connection,
		_sender_id: ObjectId,
		_serial: u32,
	) -> WaylandResult<()> {
		Ok(())
	}

	async fn destroy(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(self.id);
		Ok(())
	}
}
