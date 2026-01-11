mod core;
mod display;
mod dmabuf;
mod mesa_drm;
mod presentation;
mod registry;
mod relative_pointer;
mod util;
mod viewporter;
mod xdg;

use crate::core::error::ServerError;
use crate::core::registry::OwnedRegistry;
use crate::get_time;
use crate::nodes::drawable::model::ModelNodeSystemSet;
use crate::wayland::core::seat::SeatMessage;
use crate::wayland::core::surface::Surface;
use crate::wayland::presentation::MonotonicTimestamp;
use crate::wayland::util::ClientExt;
use crate::{BevyMaterial, core::task};
use bevy::app::{App, Plugin, Update};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Local, Res, ResMut};
use bevy::prelude::{Deref, DerefMut};
use bevy::render::{Render, RenderApp};
use bevy::{asset::Assets, ecs::resource::Resource, image::Image};
use bevy_dmabuf::import::ImportedDmatexs;
use bevy_mod_openxr::render::end_frame;
use bevy_mod_openxr::resources::{OxrFrameState, OxrInstance, Pipelined};
use bevy_mod_xr::session::XrRenderSet;
use core::buffer::BufferUsage;
use core::{buffer::Buffer, callback::Callback, surface::WL_SURFACE_REGISTRY};
use display::Display;
use mint::Vector2;
use pin_project_lite::pin_project;
use std::fs::File;
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{
	io,
	path::PathBuf,
	sync::{Arc, OnceLock},
};
use tokio::{net::UnixStream, sync::mpsc, task::AbortHandle};
use tokio_stream::{Stream, StreamExt};
use tracing::{debug_span, instrument};
use waynest::{Connection, Socket};
use waynest::{ObjectId, ProtocolError};
use waynest_protocols::server::core::wayland::wl_display::WlDisplay;
use waynest_server::{Client as _, Listener, Store, StoreError};
use xdg::toplevel::Toplevel;


pub struct WaylandPlugin;
impl Plugin for WaylandPlugin {
	fn build(&self, app: &mut App) {
		app.add_systems(Update, update_graphics.before(ModelNodeSystemSet));
		app.init_resource::<UsedBuffers>();
		app.sub_app_mut(RenderApp)
			.init_resource::<UsedBuffers>();
	}
	fn finish(&self, app: &mut App) {
		app.sub_app_mut(RenderApp)
			.add_systems(Render, before_render.in_set(XrRenderSet::PreRender))
			.add_systems(Render, after_render.in_set(XrRenderSet::PostRender))
			.add_systems(
				Render,
				submit_frame_timings
					.in_set(XrRenderSet::PostRender)
					.after(end_frame),
			);
	}
}

#[derive(Resource, Deref, DerefMut)]
struct UsedBuffers(OwnedRegistry<BufferUsage>);
impl Default for UsedBuffers {
	fn default() -> Self {
		Self(OwnedRegistry::new())
	}
}

fn before_render(buffers: Res<UsedBuffers>) {
	for buf in WL_SURFACE_REGISTRY
		.get_valid_contents()
		.into_iter()
		.filter_map(|surface| surface.current_buffer_usage())
	{
		buffers.add_raw(buf);
	}
	for surface in WL_SURFACE_REGISTRY.get_valid_contents() {
		surface.frame_event();
	}
}

fn after_render(buffers: Res<UsedBuffers>) {
	buffers.clear();
}

#[instrument(level = "debug", name = "Wayland frame", skip_all)]
fn update_graphics(
	dmatexes: Res<ImportedDmatexs>,
	mut materials: ResMut<Assets<BevyMaterial>>,
	mut images: ResMut<Assets<Image>>,
) {
	for surface in WL_SURFACE_REGISTRY.get_valid_contents() {
		surface.update_graphics(&dmatexes, &mut materials, &mut images);
	}
}

#[instrument(level = "debug", name = "Wayland frame", skip_all)]
fn submit_frame_timings(
	mut frame_count: Local<u64>,
	instance: Option<Res<OxrInstance>>,
	frame_state: Option<Res<OxrFrameState>>,
	pipelined: Option<Res<Pipelined>>,
) {
	*frame_count += 1;
	let display_timestamp = frame_state
		.and_then(|state| Some((state, instance?)))
		.and_then(|(state, instance)| {
			instance
				.exts()
				.khr_convert_timespec_time
				.and_then(|v| unsafe {
					let mut out = MaybeUninit::uninit();
					let result = (v.convert_time_to_timespec_time)(
						instance.as_raw(),
						get_time(pipelined.is_some(), &state),
						out.as_mut_ptr(),
					);
					if result != openxr::sys::Result::SUCCESS {
						return None;
					}
					let v = out.assume_init();
					Some(rustix::time::Timespec {
						tv_sec: v.tv_sec,
						tv_nsec: v.tv_nsec,
					})
				})
		})
		.unwrap_or_else(|| rustix::time::clock_gettime(rustix::time::ClockId::Monotonic))
		.into();
	for surface in WL_SURFACE_REGISTRY.get_valid_contents() {
		surface.submit_presentation_feedback(display_timestamp, *frame_count);
	}
}
