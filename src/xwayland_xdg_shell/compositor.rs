// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use serde_derive::Deserialize;
use serde_derive::Serialize;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::pointer::CursorImageSurfaceData;
use smithay::input::Seat;
use smithay::input::SeatHandler;
use smithay::input::SeatState;
use smithay::output::Mode;
use smithay::output::Output;
use smithay::output::PhysicalProperties;
use smithay::output::Scale;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor;
use smithay::wayland::compositor::BufferAssignment;
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::compositor::SurfaceAttributes;
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::ClientDndGrabHandler;
use smithay::wayland::selection::data_device::DataDeviceHandler;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::data_device::ServerDndGrabHandler;
use smithay::wayland::selection::primary_selection::PrimarySelectionHandler;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::SelectionSource;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::shm::ShmHandler;
use smithay::wayland::shm::ShmState;
use smithay::xwayland::X11Surface;
use smithay::xwayland::X11Wm;
use smithay::xwayland::XWayland;
use smithay::xwayland::XWaylandClientData;
use smithay::xwayland::XWaylandEvent;
use smithay_client_toolkit::reexports::csd_frame::DecorationsFrame;
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_surface;
use smithay_client_toolkit::shell::xdg::window::Window;
use smithay_client_toolkit::shell::xdg::XdgSurface;
use smithay_client_toolkit::shell::WaylandSurface;

use crate::compositor_utils;
use crate::fallible_entry::FallibleEntryExt;
use crate::prelude::*;
use crate::serialization::geometry::Point;
use crate::serialization::wayland::OutputInfo;
use crate::utils::SerialMap;
use crate::xwayland_xdg_shell::client::Role;
use crate::xwayland_xdg_shell::wmname;
use crate::xwayland_xdg_shell::CalloopData;
use crate::xwayland_xdg_shell::WprsState;
use crate::xwayland_xdg_shell::XWaylandSurface;

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub enum DecorationBehavior {
    #[default]
    Auto,
    AlwaysEnabled,
    AlwaysDisabled,
}

#[derive(Debug)]
pub struct WprsCompositorState {
    pub dh: DisplayHandle,
    pub compositor_state: CompositorState,
    pub start_time: Instant,
    pub shm_state: ShmState,
    pub seat_state: SeatState<WprsState>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub decoration_behavior: DecorationBehavior,

    pub seat: Seat<WprsState>,

    pub outputs: HashMap<u32, (Output, GlobalId)>,
    pub(crate) serial_map: SerialMap,
    pub(crate) pressed_keys: HashSet<u32>,

    pub xwayland: XWayland,
    pub xwm: Option<X11Wm>,

    /// unpaired x11 surfaces
    pub x11_surfaces: Vec<X11Surface>,
}

impl WprsCompositorState {
    /// # Panics
    /// On failure launching xwayland.
    pub fn new(
        dh: DisplayHandle,
        event_loop_handle: LoopHandle<'static, CalloopData>,
        decoration_behavior: DecorationBehavior,
    ) -> Self {
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(&dh, "wprs");

        let xwayland = {
            let (xwayland, channel) = XWayland::new(&dh);
            let dh = dh.clone();
            let ret = event_loop_handle.insert_source(channel, move |event, _, data| match event {
                XWaylandEvent::Ready {
                    connection,
                    client,
                    client_fd: _,
                    display,
                } => {
                    let wm = X11Wm::start_wm(
                        data.state.event_loop_handle.clone(),
                        dh.clone(),
                        connection,
                        client,
                    )
                    .expect("Failed to attach X11 Window Manager.");

                    // Oh Java...
                    wmname::set_wmname(Some(&format!(":{}", display)), "LG3D")
                        .expect("Failed to set WM name.");

                    data.state.compositor_state.xwm = Some(wm);
                },
                XWaylandEvent::Exited => {
                    let _ = data.state.compositor_state.xwm.take();
                },
            });
            if let Err(e) = ret {
                error!(
                    "Failed to insert the XWaylandSource into the event loop: {}",
                    e
                );
            }
            xwayland
        };

        Self {
            dh: dh.clone(),
            compositor_state: CompositorState::new::<WprsState>(&dh),
            start_time: Instant::now(),
            shm_state: ShmState::new::<WprsState>(&dh, Vec::new()),
            seat_state,
            data_device_state: DataDeviceState::new::<WprsState>(&dh),
            primary_selection_state: PrimarySelectionState::new::<WprsState>(&dh),
            decoration_behavior,
            seat,
            outputs: HashMap::new(),
            serial_map: SerialMap::new(),
            pressed_keys: HashSet::new(),

            xwayland,
            xwm: None,

            x11_surfaces: Vec::new(),
        }
    }
}

impl BufferHandler for WprsState {
    #[instrument(skip(self), level = "debug")]
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {}
}

impl SelectionHandler for WprsState {
    type SelectionUserData = ();

    // We need to implement this trait for copying to clients, but all our
    // clients are xwayland clients and so the methods below should never be
    // called.

    #[instrument(skip(self, _seat), level = "debug")]
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        error!("new_selection called");
    }

    #[instrument(skip(self, _fd, _seat, _user_data), level = "debug")]
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        _fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
        error!("new_selection called");
    }
}

impl DataDeviceHandler for WprsState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.compositor_state.data_device_state
    }
}

impl PrimarySelectionHandler for WprsState {
    fn primary_selection_state(
        &self,
    ) -> &smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &self.compositor_state.primary_selection_state
    }
}

impl ClientDndGrabHandler for WprsState {}
impl ServerDndGrabHandler for WprsState {}

fn execute_or_defer_commit(state: &mut WprsState, surface: WlSurface) -> Result<()> {
    commit(&surface, state).location(loc!())?;

    let xwayland_surface = state.surfaces.get(&surface.id());
    let is_cursor = matches!(
        xwayland_surface,
        Some(XWaylandSurface {
            role: Some(Role::Cursor),
            ..
        })
    );

    if !(xwayland_surface
        .as_ref()
        .map_or(false, |s| s.x11_surface.is_some())
        || is_cursor)
    {
        debug!("deferring commit");
        X11Wm::commit_hook::<CalloopData>(&surface);
        state.event_loop_handle.insert_idle(|loop_data| {
            execute_or_defer_commit(&mut loop_data.state, surface).log_and_ignore(loc!());
        });
    }
    Ok(())
}

impl CompositorHandler for WprsState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<XWaylandClientData>()
            .unwrap()
            .compositor_state
    }

    #[instrument(skip(self), level = "debug")]
    fn commit(&mut self, surface: &WlSurface) {
        execute_or_defer_commit(self, surface.clone()).log_and_ignore(loc!());
    }
}

#[instrument(skip(state), level = "debug")]
pub fn commit(surface: &WlSurface, state: &mut WprsState) -> Result<()> {
    compositor::with_states(surface, |surface_data| -> Result<()> {
        commit_inner(surface, surface_data, state).location(loc!())?;
        Ok(())
    })
    .location(loc!())?;
    on_commit_buffer_handler::<WprsState>(surface);
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct X11ParentForPopup {
    pub(crate) surface_id: ObjectId,
    pub(crate) xdg_surface: xdg_surface::XdgSurface,
    pub(crate) offset: Point<i32>,
}

#[derive(Debug, Clone)]
pub(crate) struct X11Parent {
    pub(crate) surface_id: ObjectId,
    pub(crate) for_toplevel: Option<Window>,
    pub(crate) for_popup: X11ParentForPopup,
}

pub(crate) fn find_x11_parent(
    state: &WprsState,
    x11_surface: Option<X11Surface>,
) -> Option<X11Parent> {
    if let Some(x11_surface) = &x11_surface {
        if let Some(parent_id) = x11_surface.is_transient_for() {
            let (parent_id, parent) = state
                .surfaces
                .iter()
                .find(|(_, xwls)| {
                    xwls.x11_surface
                        .as_ref()
                        .map_or(false, |s| s.window_id() == parent_id)
                })
                .unwrap();

            let Ok(parent_x11_surface) = parent.get_x11_surface() else {
                error!("parent {parent:?} has no attached x11 surface");
                return None;
            };
            let geo = parent_x11_surface.geometry();
            match &parent.role {
                Some(Role::XdgToplevel(toplevel)) => Some(X11Parent {
                    surface_id: parent_id.clone(),
                    for_toplevel: Some(toplevel.local_window.clone()),
                    for_popup: X11ParentForPopup {
                        surface_id: parent_id.clone(),
                        xdg_surface: toplevel.xdg_surface().clone(),
                        offset: (
                            -geo.loc.x + toplevel.frame_offset.x,
                            -geo.loc.y + toplevel.frame_offset.y,
                        )
                            .into(),
                    },
                }),
                Some(Role::XdgPopup(popup)) => Some(X11Parent {
                    surface_id: parent_id.clone(),
                    for_toplevel: None,
                    for_popup: X11ParentForPopup {
                        surface_id: parent_id.clone(),
                        xdg_surface: popup.xdg_surface().clone(),
                        offset: (-geo.loc.x, -geo.loc.y).into(),
                    },
                }),
                Some(Role::Cursor) => unreachable!("Cursors cannot have child surfaces."),
                // TODO: fix this
                None => unreachable!(
                    "Parent doesn't yet have a role assigned. This is a race condition."
                ),
            }
        } else {
            None
        }
    } else {
        None
    }
}

#[instrument(skip(state), level = "debug")]
pub fn commit_inner(
    surface: &WlSurface,
    surface_data: &SurfaceData,
    state: &mut WprsState,
) -> Result<()> {
    let mut surface_attributes = surface_data.cached_state.current::<SurfaceAttributes>();
    let x11_surface = state
        .compositor_state
        .x11_surfaces
        .iter()
        .position(|x11s| x11s.wl_surface().map(|s| s == *surface).unwrap_or(false))
        .map(|pos| state.compositor_state.x11_surfaces.swap_remove(pos));
    debug!("matched x11 surface: {x11_surface:?}");

    let parent = find_x11_parent(state, x11_surface.clone());

    if let (Some(parent), Some(_)) = (&parent, &x11_surface) {
        debug!(
            "registering child {:?} with parent {:?}",
            surface.id(),
            &parent.surface_id
        );
        // We can still get cycles in the case of bugs in find_x11_parent, but
        // this is a start.
        assert!(
            surface.id() != parent.surface_id,
            "tried to register a surface as a child of itself"
        );
        let parent_xwayland_surface = state.surfaces.get_mut(&parent.surface_id).unwrap();
        parent_xwayland_surface.children.insert(surface.id());
    }

    let xwayland_surface = state
        .surfaces
        .entry(surface.id())
        .or_insert_with_result(|| {
            XWaylandSurface::new(
                surface,
                &state.client_state.compositor_state,
                &state.client_state.qh,
                &mut state.surface_bimap,
            )
        })
        .location(loc!())?;

    if let Some(x11_surface) = x11_surface {
        xwayland_surface
            .update_x11_surface(
                x11_surface,
                parent,
                &state.client_state.last_focused_window,
                &state.client_state.xdg_shell_state,
                &state.client_state.shm_state,
                state.client_state.subcompositor_state.clone(),
                &state.client_state.qh,
                state.compositor_state.decoration_behavior,
            )
            .location(loc!())?;
    }

    debug!("buffer assignment: {:?}", &surface_attributes.buffer);
    match &surface_attributes.buffer {
        Some(BufferAssignment::NewBuffer(buffer)) => {
            compositor_utils::with_buffer_contents(buffer, |data, spec| {
                xwayland_surface.update_buffer(
                    &spec,
                    data,
                    state.client_state.pool.as_mut().location(loc!())?,
                )
            })
            .location(loc!())?
            .location(loc!())?;
        },
        Some(BufferAssignment::Removed) => {
            xwayland_surface.buffer = None;
            xwayland_surface.wl_surface().attach(None, 0, 0);
        },
        None => {},
    }

    if let Some(Role::XdgToplevel(toplevel)) = &mut xwayland_surface.role {
        if toplevel.configured && toplevel.window_frame.is_dirty() {
            toplevel.window_frame.draw();
        }
    }

    xwayland_surface.frame(&state.client_state.qh);
    xwayland_surface.commit();

    if xwayland_surface.x11_surface.is_none() || matches!(xwayland_surface.role, Some(Role::Cursor))
    {
        compositor_utils::send_frames(
            surface,
            &surface_data.data_map,
            &mut surface_attributes,
            state.compositor_state.start_time.elapsed(),
            Duration::ZERO,
        )
        .location(loc!())?;
    }
    Ok(())
}

impl ShmHandler for WprsState {
    fn shm_state(&self) -> &ShmState {
        &self.compositor_state.shm_state
    }
}

impl SeatHandler for WprsState {
    type KeyboardFocus = X11Surface;
    type PointerFocus = X11Surface;
    type TouchFocus = X11Surface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.compositor_state.seat_state
    }

    #[instrument(skip(self, _seat), level = "debug")]
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // TODO: support multiple seats
        let themed_pointer = self
            .client_state
            .seat_objects
            .last()
            .unwrap()
            .pointer
            .as_ref()
            .unwrap();
        let pointer = themed_pointer.pointer();

        // TODO: move to a fn on serialization::CursorImaveStatus
        match image {
            CursorImageStatus::Hidden => {
                themed_pointer.hide_cursor().log_and_ignore(loc!());
            },
            CursorImageStatus::Surface(surface) => {
                let hotspot = compositor::with_states(&surface, |surface_data| {
                    surface_data
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                let xwayland_surface = log_and_return!(self
                    .surfaces
                    .entry(surface.id())
                    .or_insert_with_result(|| {
                        XWaylandSurface::new(
                            &surface,
                            &self.client_state.compositor_state,
                            &self.client_state.qh,
                            &mut self.surface_bimap,
                        )
                    }));

                xwayland_surface.role = Some(Role::Cursor);

                // TODO: expose serial to this function, then remove
                // last_enter_serial on client.
                pointer.set_cursor(
                    self.client_state.last_enter_serial,
                    Some(xwayland_surface.wl_surface()),
                    hotspot.x,
                    hotspot.y,
                );
            },
            CursorImageStatus::Named(name) => {
                themed_pointer
                    .set_cursor(&self.client_state.conn, name)
                    .log_and_ignore(loc!());
            },
        }
    }
}

impl OutputHandler for WprsState {}

// TODO: dedupe with the one in server
// TODO: should this be in a trait?
#[instrument(skip(state), level = "debug")]
pub(crate) fn handle_output(state: &mut WprsState, output: OutputInfo) {
    let (local_output, _) = state
        .compositor_state
        .outputs
        .entry(output.id)
        .or_insert_with_key(|id| {
            let new_output = Output::new(
                format!("{}_{}", id, output.name.unwrap_or("None".to_string())),
                PhysicalProperties {
                    size: output.physical_size.into(),
                    subpixel: output.subpixel.into(),
                    make: output.make,
                    model: output.model,
                },
            );
            let global_id = new_output.create_global::<WprsState>(&state.compositor_state.dh);
            (new_output, global_id)
        });

    let current_mode = local_output.current_mode().unwrap_or(Mode {
        size: (0, 0).into(),
        refresh: 0,
    });
    let received_mode = Mode {
        size: output.mode.dimensions.into(),
        refresh: output.mode.refresh_rate,
    };
    if current_mode != received_mode {
        local_output.delete_mode(current_mode);
    }

    local_output.change_current_state(
        Some(received_mode),
        Some(output.transform.into()),
        Some(Scale::Integer(output.scale_factor)),
        Some(output.location.into()),
    );

    if output.mode.preferred {
        local_output.set_preferred(received_mode);
    }
}

smithay::delegate_compositor!(WprsState);
smithay::delegate_shm!(WprsState);
smithay::delegate_seat!(WprsState);
smithay::delegate_data_device!(WprsState);
smithay::delegate_output!(WprsState);
smithay::delegate_primary_selection!(WprsState);
