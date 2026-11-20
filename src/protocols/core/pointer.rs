use super::surface::SurfaceRole;
use crate::protocols::{core::surface::Surface, relative_pointer::RelativePointer};
use crate::{
	client::Client,
	error::{WaylandError, WaylandResult},
	util::{WaylandTime as _, now},
};
use mint::Vector2;
use stardust_xr_fusion::types::Timestamp;
use stardust_xr_panels::panel_item::{Geometry, ScrollSource};
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::{Mutex, RwLock};
use waynest::ObjectId;
use waynest_server::Client as _;

pub use waynest_protocols::server::core::wayland::wl_pointer::*;

/// a press keeps focus where it happened until every button is up again,
/// so a drag that wanders off still gets its release
#[derive(Debug)]
enum Focus {
	None,
	Hovering(Weak<Surface>),
	Grabbed {
		surface: Weak<Surface>,
		buttons: Vec<u32>,
	},
}
impl Focus {
	fn surface(&self) -> Option<Arc<Surface>> {
		match self {
			Self::None => None,
			Self::Hovering(s) | Self::Grabbed { surface: s, .. } => s.upgrade(),
		}
	}
	fn is_on(&self, surface: &Arc<Surface>) -> bool {
		matches!(self, Self::Hovering(s) | Self::Grabbed { surface: s, .. } if s.as_ptr() == Arc::as_ptr(surface))
	}
	/// the client already forgot about a surface it destroyed, so no leave
	fn drop_dead(&mut self, client: &Client) {
		if self.surface().is_none_or(|s| !client.holds(s.id, &*s)) {
			*self = Self::None;
		}
	}
}

#[derive(waynest_server::RequestDispatcher)]
#[waynest(error = crate::error::WaylandError, connection = crate::client::Client)]
pub struct Pointer {
	pub id: ObjectId,
	version: u32,
	focus: Mutex<Focus>,
	cursor_surface: Mutex<Option<Arc<Surface>>>,
	pub relative_pointer: RwLock<Weak<RelativePointer>>,
}
impl Pointer {
	pub fn new(id: ObjectId, version: u32) -> Self {
		Self {
			id,
			version,
			focus: Mutex::new(Focus::None),
			cursor_surface: Mutex::new(None),
			relative_pointer: RwLock::new(Weak::new()),
		}
	}

	pub async fn handle_absolute_pointer_motion(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		position: Vector2<f32>,
		time: Timestamp,
	) -> WaylandResult<()> {
		tracing::debug!(
			"Handling absolute pointer motion at ({}, {})",
			position.x,
			position.y
		);
		if !client.holds(surface.id, &*surface) {
			return Ok(());
		}
		let mut focus = self.focus.lock().await;
		focus.drop_dead(client);

		if !focus.is_on(&surface) {
			// stardust gives positions relative to whatever is under the pointer, there's no
			// turning them into the grabbed surface's coordinates
			if matches!(*focus, Focus::Grabbed { .. }) {
				return Ok(());
			}
			if let Some(old) = focus.surface() {
				let serial = client.next_event_serial();
				self.leave(client, self.id, serial, old.id).await?;
			}
			let serial = client.next_event_serial();
			self.enter(
				client,
				self.id,
				serial,
				surface.id,
				(position.x as f64).into(),
				(position.y as f64).into(),
			)
			.await?;
			*focus = Focus::Hovering(Arc::downgrade(&surface));
		}

		self.motion(
			client,
			self.id,
			time.ms(),
			(position.x as f64).into(),
			(position.y as f64).into(),
		)
		.await?;
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}

		Ok(())
	}

	pub async fn handle_relative_pointer_motion(
		&self,
		client: &mut Client,
		delta: Vector2<f32>,
		time: Timestamp,
	) -> WaylandResult<()> {
		tracing::debug!(
			"Handling relative pointer motion of ({}, {})",
			delta.x,
			delta.y
		);

		let Some(relative_pointer) = self.relative_pointer.read().await.upgrade() else {
			return Ok(());
		};

		relative_pointer
			.send_relative_motion(client, delta, time)
			.await
	}

	pub async fn handle_pointer_button(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		button: u32,
		pressed: bool,
		time: Timestamp,
	) -> WaylandResult<()> {
		tracing::debug!(
			"Handling pointer button {} {} on surface {:?}",
			button,
			if pressed { "pressed" } else { "released" },
			surface.id
		);
		let mut focus = self.focus.lock().await;
		focus.drop_dead(client);
		if pressed {
			// without a position there's no entering this surface properly, so a press anywhere
			// but the focused surface is dropped rather than misdirected
			if !focus.is_on(&surface) {
				return Ok(());
			}
			match &mut *focus {
				Focus::Hovering(s) => {
					let s = s.clone();
					*focus = Focus::Grabbed {
						surface: s,
						buttons: vec![button],
					};
				}
				Focus::Grabbed { buttons, .. } if !buttons.contains(&button) => {
					buttons.push(button)
				}
				_ => return Ok(()),
			}
		} else {
			// releases always go to the grab, wherever the pointer ended up
			let Focus::Grabbed {
				surface: s,
				buttons,
			} = &mut *focus
			else {
				return Ok(());
			};
			let Some(i) = buttons.iter().position(|b| *b == button) else {
				return Ok(());
			};
			buttons.swap_remove(i);
			if buttons.is_empty() {
				let s = s.clone();
				*focus = Focus::Hovering(s);
			}
		}

		let serial = client.next_event_serial();
		self.button(
			client,
			self.id,
			serial,
			time.ms(),
			button,
			if pressed {
				ButtonState::Pressed
			} else {
				ButtonState::Released
			},
		)
		.await?;
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}
		Ok(())
	}
	pub async fn handle_pointer_scroll_discrete(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
		time: Timestamp,
	) -> WaylandResult<()> {
		if !self.focus.lock().await.is_on(&surface) {
			return Ok(());
		}
		tracing::info!(
			"Handling discrete pointer scroll: steps={:?} on version {}",
			delta,
			self.version
		);
		let horizontal = delta.x != 0.0;
		let vertical = delta.y != 0.0;
		// cage doesn't forward scroll without this
		if self.version >= 5 {
			self.axis_source(
				client,
				self.id,
				match source {
					ScrollSource::Wheel => AxisSource::Wheel,
					ScrollSource::Touch => AxisSource::Finger,
					ScrollSource::Continuous => AxisSource::Continuous,
					ScrollSource::WheelTilt => AxisSource::WheelTilt,
				},
			)
			.await?;
		}
		if self.version >= 9 {
			if horizontal {
				self.axis_relative_direction(
					client,
					self.id,
					Axis::HorizontalScroll,
					// TODO: expose over panel item?
					AxisRelativeDirection::Identical,
				)
				.await?;
			}
			if vertical {
				self.axis_relative_direction(
					client,
					self.id,
					Axis::VerticalScroll,
					// TODO: expose over panel item?
					AxisRelativeDirection::Identical,
				)
				.await?;
			}
		}
		if vertical {
			self.axis(
				client,
				self.id,
				time.ms(),
				Axis::VerticalScroll,
				(delta.y as f64).into(),
			)
			.await?;
		}
		if horizontal {
			self.axis(
				client,
				self.id,
				time.ms(),
				Axis::HorizontalScroll,
				(delta.x as f64).into(),
			)
			.await?;
		}
		if self.version < 8 && self.version >= 5 {
			if horizontal {
				self.axis_discrete(client, self.id, Axis::HorizontalScroll, delta.x as i32)
					.await?;
			}
			if vertical {
				self.axis_discrete(client, self.id, Axis::VerticalScroll, delta.y as i32)
					.await?;
			}
		}
		if self.version >= 8 {
			if horizontal {
				self.axis_value120(
					client,
					self.id,
					Axis::HorizontalScroll,
					(delta.x * 120.) as i32,
				)
				.await?;
			}
			if vertical {
				self.axis_value120(
					client,
					self.id,
					Axis::VerticalScroll,
					(delta.y * 120.) as i32,
				)
				.await?;
			}
		}
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}
		Ok(())
	}
	pub async fn handle_pointer_scroll_pixels(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		delta: Vector2<f32>,
		source: ScrollSource,
		time: Timestamp,
	) -> WaylandResult<()> {
		if !self.focus.lock().await.is_on(&surface) {
			return Ok(());
		}
		tracing::debug!("Handling pointer smooth scroll: distance={:?}", delta,);
		if self.version >= 5 {
			self.axis_source(
				client,
				self.id,
				match source {
					ScrollSource::Wheel => AxisSource::Wheel,
					ScrollSource::Touch => AxisSource::Finger,
					ScrollSource::Continuous => AxisSource::Continuous,
					ScrollSource::WheelTilt => AxisSource::WheelTilt,
				},
			)
			.await?;
		}
		self.axis(
			client,
			self.id,
			time.ms(),
			Axis::HorizontalScroll,
			(delta.x as f64).into(),
		)
		.await?;
		self.axis(
			client,
			self.id,
			time.ms(),
			Axis::VerticalScroll,
			(delta.y as f64).into(),
		)
		.await?;
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}
		Ok(())
	}
	pub async fn handle_pointer_scroll_stop(
		&self,
		client: &mut Client,
		surface: Arc<Surface>,
		time: Timestamp,
	) -> WaylandResult<()> {
		if !self.focus.lock().await.is_on(&surface) {
			return Ok(());
		}
		if self.version >= 5 {
			self.axis_stop(client, self.id, time.ms(), Axis::HorizontalScroll)
				.await?;
			self.axis_stop(client, self.id, time.ms(), Axis::VerticalScroll)
				.await?;
		}
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}
		Ok(())
	}

	pub async fn reset(
		&self,
		client: &mut Client,
		toplevel_surface: &Surface,
	) -> WaylandResult<()> {
		let mut focus = self.focus.lock().await;
		focus.drop_dead(client);
		let Some(old) = focus.surface() else {
			return Ok(());
		};
		if !old.same_toplevel(toplevel_surface) {
			return Ok(());
		}
		// a leave alone doesn't tell the client its held buttons went up
		if let Focus::Grabbed { buttons, .. } = &*focus {
			for button in buttons.clone() {
				let serial = client.next_event_serial();
				self.button(
					client,
					self.id,
					serial,
					now().ms(),
					button,
					ButtonState::Released,
				)
				.await?;
			}
		}
		let serial = client.next_event_serial();
		self.leave(client, self.id, serial, old.id).await?;
		if self.version >= 5 {
			self.frame(client, self.id).await?;
		}
		*focus = Focus::None;
		Ok(())
	}

	pub async fn grabbed_within(&self, origin: &Surface) -> bool {
		matches!(&*self.focus.lock().await, Focus::Grabbed { surface, .. }
			if surface.upgrade().is_some_and(|s| s.same_toplevel(origin)))
	}
	/// the pointer belongs to the drag until it's released, and comes back on the next motion
	pub async fn yield_to_drag(&self, client: &mut Client) -> WaylandResult<()> {
		let mut focus = self.focus.lock().await;
		focus.drop_dead(client);
		if let Some(old) = focus.surface() {
			let serial = client.next_event_serial();
			self.leave(client, self.id, serial, old.id).await?;
			if self.version >= 5 {
				self.frame(client, self.id).await?;
			}
		}
		*focus = Focus::None;
		Ok(())
	}

	pub async fn cursor_surface(&self) -> Option<Arc<Surface>> {
		self.cursor_surface.lock().await.clone()
	}
}

impl WlPointer for Pointer {
	type Connection = crate::client::Client;

	/// https://wayland.app/protocols/wayland#wl_pointer:request:set_cursor
	async fn set_cursor(
		&self,
		client: &mut Self::Connection,
		_sender_id: ObjectId,
		_serial: u32,
		surface: Option<ObjectId>,
		hotspot_x: i32,
		hotspot_y: i32,
	) -> WaylandResult<()> {
		if let Some(focused_surface) = self.focus.lock().await.surface()
			&& let Some(panel_item) = focused_surface.panel_item()
		{
			let geometry = surface.and_then(|s| client.get::<Surface>(s)).map(|s| {
				let size = s.current_size().unwrap_or([16; 2].into());
				Geometry {
					origin: Vector2::from([hotspot_x, hotspot_y]),
					size: Vector2::from([size.x as u32, size.y as u32]),
				}
			});
			if let Some(shell) = panel_item.panel_shell()
				&& let Err(e) = shell.set_cursor_visuals(geometry)
			{
				tracing::error!("failed to send cursor visuals to panel shell: {e}");
			}
		}
		let Some(surface) = surface else {
			return Ok(());
		};
		let Some(surface) = client.get::<Surface>(surface) else {
			return Ok(());
		};

		surface
			.assign_role(SurfaceRole::Cursor)
			.map_err(|_| WaylandError::Fatal {
				object_id: self.id,
				code: Error::Role as u32,
				message: "cursor surface already has another role",
			})?;
		_ = surface
			.surface_id
			.set(stardust_xr_panels::panel_item::SurfaceUpdateTarget::Cursor);
		if let Some(focused_surface) = self.focus.lock().await.surface() {
			*surface.toplevel.write() = focused_surface.toplevel.read().clone();
		}
		// a kinda ugly hack to get the cursor to show on startup
		surface.resubmit_buffer();
		self.cursor_surface.lock().await.replace(surface);
		Ok(())
	}

	/// https://wayland.app/protocols/wayland#wl_pointer:request:release
	async fn release(
		&self,
		client: &mut Self::Connection,
		sender_id: ObjectId,
	) -> WaylandResult<()> {
		client.remove(sender_id);
		Ok(())
	}
}
