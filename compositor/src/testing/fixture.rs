//! Test fixture for integration testing
//!
//! The Fixture provides a complete compositor environment for testing,
//! including a headless backend, event loop, and Wayland display.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use smithay::backend::input::{ButtonState, InputTime};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tracing::info;

use crate::backend::{Backend, HeadlessBackend};
use crate::cursor::CursorConfig;
use crate::layout::strip::Frame;
use crate::testing::TestClient;
use crate::testing::input::{TestAbsoluteEvent, TestDevice, TestGestureEvent, TestInputBackend};
use crate::{ClientState, Ewm, OutputInfo, State, register_display_source};

/// Test fixture for integration testing
///
/// Provides a complete compositor environment with:
/// - Event loop for async operations
/// - Headless backend for virtual outputs
/// - Wayland display for protocol testing
///
/// Uses the same `State` struct as production, but with `Backend::Headless`.
pub struct Fixture {
    event_loop: EventLoop<'static, State>,
    state: State,
    /// Server ends of the client sockets, hung up on drop.
    client_ends: Vec<UnixStream>,
    owns_event_channel: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Hang up the clients so their objects are destroyed, not leaked with the display.
        for end in &self.client_ends {
            let _ = end.shutdown(std::net::Shutdown::Both);
        }
        self.event_loop
            .dispatch(Some(Duration::ZERO), &mut self.state)
            .ok();
        if self.owns_event_channel {
            crate::module::clear_event_channel();
        }
    }
}

impl Fixture {
    /// Create a new test fixture
    ///
    /// Initializes the event loop, display, and headless backend.
    /// The fixture starts with no outputs - use `add_output` to create virtual displays.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_cursor_config(CursorConfig::default())
    }

    pub fn new_with_cursor_config(
        cursor_config: CursorConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize event loop
        let event_loop: EventLoop<State> = EventLoop::try_new()?;

        // Create Wayland display (typed for State, which has handlers)
        let display: Display<State> = Display::new()?;
        let display_handle = display.handle();

        // Create compositor state with headless backend
        let mut ewm = Ewm::new(display_handle, event_loop.handle(), false, cursor_config);
        // Headless stands in for a touchscreen too
        ewm.seat.add_touch();
        let backend = Backend::Headless(HeadlessBackend::new());

        let state = State { backend, ewm };

        register_display_source(&event_loop.handle(), display);

        info!("Test fixture initialized with headless backend");

        Ok(Self {
            event_loop,
            state,
            client_ends: Vec::new(),
            owns_event_channel: false,
        })
    }

    /// Pair a `UnixStream` with the server `Display` as the listener would,
    /// returning a `TestClient` on the other end. Roundtrip after to bind globals.
    pub fn new_client(&mut self) -> Result<TestClient, Box<dyn std::error::Error>> {
        let (server_end, client_end) = UnixStream::pair()?;
        self.client_ends.push(server_end.try_clone()?);
        let server_client = ClientState::insert(&mut self.state.ewm.display_handle, server_end)?;
        TestClient::new(client_end, server_client.id())
    }

    /// Like `new_client`, but tagged as the portal backend from the service channel.
    pub fn new_portal_client(&mut self) -> Result<TestClient, Box<dyn std::error::Error>> {
        let (server_end, client_end) = UnixStream::pair()?;
        self.client_ends.push(server_end.try_clone()?);
        let server_client = self
            .state
            .ewm
            .display_handle
            .insert_client(server_end, ClientState::portal())?;
        TestClient::new(client_end, server_client.id())
    }

    /// Attach an SHM buffer + commit. Populates server-side surface_view and
    /// flips the xdg-shell committed state so hit-tests find the surface.
    fn finish_xdg(
        &mut self,
        client: &mut TestClient,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        xdg_surface: &wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface,
        size: Size<i32, Logical>,
    ) {
        let buffer = client.create_shm_buffer(size);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size.w, size.h);
        xdg_surface.set_window_geometry(0, 0, size.w, size.h);
        surface.commit();
        self.roundtrip(client);
    }

    #[cfg(feature = "testing-layer-shell")]
    fn finish_layer_surface(
        &mut self,
        client: &mut TestClient,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        size: Size<i32, Logical>,
    ) {
        let buffer = client.create_shm_buffer(size);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size.w, size.h);
        surface.commit();
        self.roundtrip(client);
    }

    /// Create a toplevel, a dialog of `parent` if given, and leave it unmapped.
    pub fn open_toplevel_unmapped(
        &mut self,
        client: &mut TestClient,
        title: &str,
        parent: Option<&crate::testing::Toplevel>,
    ) -> crate::testing::Toplevel {
        let toplevel = client.create_toplevel(title);
        if let Some(parent) = parent {
            toplevel.xdg_toplevel.set_parent(Some(&parent.xdg_toplevel));
            toplevel.surface.commit();
        }
        self.roundtrip(client);
        toplevel
    }

    /// Map `toplevel` by committing its first buffer.
    pub fn map_toplevel(
        &mut self,
        client: &mut TestClient,
        toplevel: &crate::testing::Toplevel,
        size: Size<i32, Logical>,
    ) {
        self.finish_xdg(client, &toplevel.surface, &toplevel.xdg_surface, size);
    }

    /// Deliver what is pending, returning the sizes configured since the last drain.
    pub fn configured(&mut self, client: &mut TestClient) -> Vec<(i32, i32)> {
        self.roundtrip(client);
        client.configured_sizes()
    }

    /// Deliver the configures and commit a buffer of the last size asked, returning the sizes.
    pub fn answer_configure(
        &mut self,
        client: &mut TestClient,
        toplevel: &crate::testing::Toplevel,
    ) -> Vec<(i32, i32)> {
        let sizes = self.configured(client);
        if let Some(&(w, h)) = sizes.iter().rev().find(|&&(w, h)| w > 0 && h > 0) {
            self.map_toplevel(client, toplevel, Size::from((w, h)));
        }
        sizes
    }

    /// Drive a full xdg toplevel lifecycle.
    pub fn open_toplevel(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = self.open_toplevel_unmapped(client, title, None);
        self.map_toplevel(client, &toplevel, size);
        toplevel
    }

    /// Open a toplevel and return its surface id.
    pub fn open_toplevel_id(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> u64 {
        self.open_toplevel_with_id(client, title, size).1
    }

    /// Open a toplevel and return it with its surface id.
    pub fn open_toplevel_with_id(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> (crate::testing::Toplevel, u64) {
        let toplevel = self.open_toplevel(client, title, size);
        let id = self
            .find_surface_id(client, &toplevel.surface)
            .expect("toplevel gets an id");
        (toplevel, id)
    }

    /// Open a toplevel titled FIRST before map and retitled SECOND on the mapping commit.
    pub fn open_toplevel_retitled(
        &mut self,
        client: &mut TestClient,
        first: &str,
        second: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = client.create_toplevel(first);
        self.roundtrip(client);
        toplevel.xdg_toplevel.set_title(second.to_string());
        self.finish_xdg(client, &toplevel.surface, &toplevel.xdg_surface, size);
        toplevel
    }

    /// Open a fixed-size toplevel (min == max), exercising the auto-float path.
    pub fn open_toplevel_fixed_size(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = client.create_toplevel(title);
        toplevel.xdg_toplevel.set_min_size(size.w, size.h);
        toplevel.xdg_toplevel.set_max_size(size.w, size.h);
        toplevel.surface.commit();
        self.roundtrip(client);
        self.finish_xdg(client, &toplevel.surface, &toplevel.xdg_surface, size);
        toplevel
    }

    /// Open a toplevel parented to PARENT (a dialog), exercising auto-float.
    pub fn open_toplevel_with_parent(
        &mut self,
        client: &mut TestClient,
        title: &str,
        parent: &crate::testing::Toplevel,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = self.open_toplevel_unmapped(client, title, Some(parent));
        self.map_toplevel(client, &toplevel, size);
        toplevel
    }

    /// Drive a full xdg popup lifecycle.
    pub fn open_popup(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> crate::testing::Popup {
        let popup = client.create_popup(parent, anchor, size);
        self.roundtrip(client);
        self.finish_xdg(client, &popup.surface, &popup.xdg_surface, size);
        popup
    }

    /// Open a popup on `parent` that asks to be slid back on screen.
    pub fn open_sliding_popup(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> crate::testing::Popup {
        let popup = client.create_sliding_popup(parent, anchor, size);
        self.roundtrip(client);
        self.finish_xdg(client, &popup.surface, &popup.xdg_surface, size);
        popup
    }

    /// Open a popup on `parent` (a toplevel), grabbed before the mapping commit.
    pub fn open_popup_grabbed(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
        serial: u32,
    ) -> crate::testing::Popup {
        self.open_grabbed_popup_on(client, &parent.xdg_surface, anchor, size, serial)
    }

    /// Open a submenu: a popup parented to another popup, grabbed before mapping.
    pub fn open_nested_popup_grabbed(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::Popup,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
        serial: u32,
    ) -> crate::testing::Popup {
        self.open_grabbed_popup_on(client, &parent.xdg_surface, anchor, size, serial)
    }

    /// Request the grab before the mapping commit. A grab after mapping is rejected.
    fn open_grabbed_popup_on(
        &mut self,
        client: &mut TestClient,
        parent_xdg: &wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
        serial: u32,
    ) -> crate::testing::Popup {
        let popup = client.create_popup_on(parent_xdg, anchor, size);
        self.roundtrip(client);
        client.grab_popup(&popup, serial);
        self.finish_xdg(client, &popup.surface, &popup.xdg_surface, size);
        popup
    }

    /// Drive a full layer-shell surface lifecycle.
    #[cfg(feature = "testing-layer-shell")]
    pub fn open_layer_surface(
        &mut self,
        client: &mut TestClient,
        namespace: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::LayerSurface {
        let layer = client.create_layer_surface(namespace, size);
        self.roundtrip(client);
        self.finish_layer_surface(client, &layer.surface, size);
        layer
    }

    /// Drive a full xdg popup lifecycle parented to a layer-shell surface.
    #[cfg(feature = "testing-layer-shell")]
    pub fn open_layer_popup(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::LayerSurface,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> crate::testing::Popup {
        let popup = client.create_layer_popup(parent, anchor, size);
        self.roundtrip(client);
        self.finish_xdg(client, &popup.surface, &popup.xdg_surface, size);
        popup
    }

    /// Pump client and server until both sides go idle for two consecutive
    /// passes (one quiet pass can still race a delayed server flush).
    pub fn roundtrip(&mut self, client: &mut TestClient) {
        let mut idle_passes = 0;
        for _ in 0..32 {
            let _ = client.eq.flush();
            self.event_loop
                .dispatch(Some(Duration::from_millis(5)), &mut self.state)
                .ok();
            self.state.refresh_and_flush_clients();
            if let Some(guard) = client.conn.prepare_read() {
                let _ = guard.read();
            }
            let dispatched = client.eq.dispatch_pending(&mut client.state).unwrap_or(0);
            if dispatched == 0 {
                idle_passes += 1;
                if idle_passes >= 2 {
                    return;
                }
            } else {
                idle_passes = 0;
            }
        }
    }

    /// Find the server-side surface_id for a client `WlSurface`. The protocol
    /// id is only unique within one Wayland client, so multi-client tests must
    /// also match the owning server-side client.
    pub fn find_surface_id(
        &self,
        client: &TestClient,
        client_surface: &wayland_client::protocol::wl_surface::WlSurface,
    ) -> Option<u64> {
        use smithay::reexports::wayland_server::Resource as _;
        use smithay::wayland::seat::WaylandFocus;
        use wayland_client::Proxy as _;
        let target = client_surface.id().protocol_id();
        self.state.ewm.windows().find_map(|(id, window)| {
            let server_surface = window.wl_surface()?;
            (server_surface.id().protocol_id() == target).then_some(())?;
            let server_client_id = self
                .state
                .ewm
                .display_handle
                .get_client(server_surface.id())
                .ok()?;
            (server_client_id.id() == client.server_client_id).then_some(id)
        })
    }

    /// Server-side resource of a client's bare (non-toplevel) surface.
    pub fn server_surface(
        &self,
        client: &TestClient,
        client_surface: &wayland_client::protocol::wl_surface::WlSurface,
    ) -> WlSurface {
        use smithay::reexports::wayland_server::Resource as _;
        use wayland_client::Proxy as _;
        let dh = &self.state.ewm.display_handle;
        let id = dh
            .backend_handle()
            .object_for_protocol_id(
                client.server_client_id.clone(),
                WlSurface::interface(),
                client_surface.id().protocol_id(),
            )
            .expect("surface not on the server; roundtrip first");
        WlSurface::from_id(dh, id).unwrap()
    }

    /// Start a pointer drag from surface SID with ICON. A button must be held.
    pub fn start_pointer_dnd(&mut self, sid: u64, icon: Option<WlSurface>) {
        use smithay::input::dnd::GrabType;
        use smithay::utils::SERIAL_COUNTER;
        use smithay::wayland::seat::WaylandFocus as _;
        use smithay::wayland::selection::data_device::WaylandDndGrabHandler as _;

        assert!(
            self.state.ewm.pointer.is_grabbed(),
            "start_drag needs a held button"
        );
        let source = self
            .state
            .ewm
            .window(sid)
            .unwrap()
            .wl_surface()
            .unwrap()
            .into_owned();
        let seat = self.state.ewm.seat.clone();
        self.state.dnd_requested(
            source,
            icon,
            seat,
            SERIAL_COUNTER.next_serial(),
            GrabType::Pointer,
        );
    }

    /// Add a virtual output with the given name and size
    pub fn add_output(&mut self, name: &str, width: i32, height: i32) {
        let Backend::Headless(headless) = &mut self.state.backend else {
            unreachable!("test fixture always uses the headless backend");
        };
        headless.add_output(name, width, height, &mut self.state.ewm);
    }

    /// Add an output with an explicit EDID identity. Reuse the same
    /// make/model/serial under a different `name` to model one physical display
    /// re-enumerating on a different connector after a replug.
    #[allow(clippy::too_many_arguments)]
    pub fn add_output_with_identity(
        &mut self,
        name: &str,
        width: i32,
        height: i32,
        make: &str,
        model: &str,
        serial: &str,
    ) {
        let Backend::Headless(headless) = &mut self.state.backend else {
            unreachable!("test fixture always uses the headless backend");
        };
        headless.add_output_with_identity(
            name,
            width,
            height,
            make,
            model,
            serial,
            &mut self.state.ewm,
        );
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str) {
        let Backend::Headless(headless) = &mut self.state.backend else {
            unreachable!("test fixture always uses the headless backend");
        };
        headless.remove_output(name, &mut self.state.ewm);
    }

    pub fn has_output(&self, name: &str) -> bool {
        self.state.ewm.connected_output(name).is_some()
    }

    /// Dispatch the event loop once with a short timeout
    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut self.state)
            .ok();
        self.state.refresh_and_flush_clients();
    }

    /// Dispatch the event loop multiple times to allow async operations to complete
    pub fn dispatch_roundtrip(&mut self, iterations: usize) {
        for _ in 0..iterations {
            self.dispatch();
        }
    }

    /// Get mutable access to the compositor state
    pub fn ewm(&mut self) -> &mut Ewm {
        &mut self.state.ewm
    }

    /// The mapped output called `name`.
    pub fn output(&self, name: &str) -> smithay::output::Output {
        self.ewm_ref()
            .layout
            .output(name)
            .cloned()
            .unwrap_or_else(|| panic!("output {name} is not mapped"))
    }

    pub fn strip(&self, name: &str) -> &crate::layout::strip::Strip {
        self.ewm_ref().layout.strip(&self.output(name)).unwrap()
    }

    pub fn strip_mut(&mut self, name: &str) -> &mut crate::layout::strip::Strip {
        let output = self.output(name);
        self.ewm().layout.strip_mut(&output).unwrap()
    }

    pub fn install_strip(&mut self, name: &str, strip: crate::layout::strip::Strip) {
        let output = self.output(name);
        self.ewm().layout.install_strip(&output, strip);
    }

    pub fn ewm_ref(&self) -> &Ewm {
        &self.state.ewm
    }

    pub fn output_info_list(&self) -> Vec<OutputInfo> {
        self.state
            .backend
            .output_info_list(&self.state.ewm.sorted_outputs)
    }

    /// Headless renders of OUTPUT so far.
    pub fn render_count(&self, output: &str) -> u32 {
        let Backend::Headless(headless) = &self.state.backend else {
            unreachable!("test fixture always uses the headless backend");
        };
        headless.render_count(output)
    }

    pub fn debug_state(&self) -> serde_json::Value {
        self.state.ewm.debug_state(&self.output_info_list())
    }

    /// Get the focused surface ID
    pub fn focused_surface_id(&self) -> Option<u64> {
        self.state.ewm.focused_surface_id()
    }

    pub fn active_text_input_surface_id(&self) -> Option<u64> {
        self.state.ewm.active_text_input_surface_id()
    }

    pub fn replace_active_text_input(&self, before: u32, after: u32, text: &str) {
        self.state
            .ewm
            .replace_active_text_input(before, after, text.to_string());
    }

    pub fn keyboard_focus_surface_id(&self) -> Option<u64> {
        self.state
            .ewm
            .keyboard_focus
            .as_ref()
            .and_then(|surface| self.state.ewm.surface_id(surface))
    }

    pub fn pointer_focus_surface_id(&self) -> Option<u64> {
        self.state
            .ewm
            .pointer_focus
            .as_ref()
            .and_then(|(surface, _)| self.state.ewm.surface_id(surface))
    }

    /// Apply stored output config for the named output (headless backend)
    pub fn apply_output_config(&mut self, output_name: &str) {
        self.state
            .backend
            .apply_output_config(&mut self.state.ewm, output_name);
    }

    /// Check if a surface with the given ID exists
    pub fn has_surface(&self, id: u64) -> bool {
        self.state.ewm.surfaces.contains_key(&id)
    }

    /// Warp the pointer to an absolute position.
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        self.state.warp_pointer(x, y);
    }

    /// Touch down at fractions (`x`, `y`) of the touchscreen.
    pub fn touch_down(&mut self, x: f64, y: f64) {
        let event = TestAbsoluteEvent {
            x,
            y,
            ..Default::default()
        };
        self.state.on_touch_down::<TestInputBackend>(event);
    }

    /// Lift the finger.
    pub fn touch_up(&mut self) {
        let event = TestAbsoluteEvent::default();
        self.state.on_touch_up::<TestInputBackend>(event);
    }

    /// Absolute pointer motion to fractions (`x`, `y`) of a device bound to `output`.
    pub fn pointer_motion_absolute(&mut self, output: &str, x: f64, y: f64) {
        let device = TestDevice {
            output: self.state.ewm.find_mapped_output(output),
        };
        let event = TestAbsoluteEvent { device, x, y };
        self.state
            .on_pointer_motion_absolute::<TestInputBackend>(event);
    }

    /// Send pointer motion to an absolute position through the compositor's
    /// normal pointer-focus and focus-follows-mouse bookkeeping.
    pub fn pointer_motion_to(&mut self, x: f64, y: f64) {
        use smithay::input::pointer::MotionEvent;
        use smithay::utils::SERIAL_COUNTER;

        let pos = Point::from((x, y));
        let (under, _) = self.state.ewm.update_pointer_focus_for_motion(pos);
        let pointer = self.state.ewm.pointer.clone();
        pointer.motion(
            &mut self.state,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_millis(0),
            },
        );
        pointer.frame(&mut self.state);
        self.state.ewm.queue_redraw_for_pointer();
    }

    /// Put `frames` on `output`'s strip the way `new_toplevel` places Emacs
    /// frames, then declare their contents. Frames placed already stay put.
    pub fn tile(&mut self, output: &str, frames: Vec<Frame>) {
        let layout = self.place(output, frames, false);
        self.state.apply_layout(layout);
    }

    /// `tile` declaring the contents through the command path Emacs uses.
    pub fn tile_command(&mut self, output: &str, frames: Vec<Frame>) {
        let frames = self.place(output, frames, false);
        self.state
            .handle_module_command(crate::module::ModuleCommand::Layout { frames });
        self.state.sync_keyboard_focus();
    }

    /// `tile` for `output`'s floating space.
    pub fn float(&mut self, output: &str, frames: Vec<Frame>) {
        let layout = self.place(output, frames, true);
        self.state.apply_layout(layout);
    }

    /// Place the unplaced frames among `frames` on `output` as `new_toplevel` would.
    pub fn open_frames(&mut self, output: &str, frames: &[Frame], floating: bool) {
        let ewm = &mut self.state.ewm;
        let Some(output) = ewm.layout.output(output).cloned() else {
            return;
        };
        for frame in frames {
            let id = frame.surface_id;
            if id == 0 || ewm.layout.has_frame(id) {
                continue;
            }
            let placement = if floating {
                let pos = frame.floating_pos.and_then(|pos| {
                    let area = ewm.layout.floating(&output)?.working_area();
                    Some(crate::layout::floating::Data::scale_by_working_area(
                        area, pos,
                    ))
                });
                crate::layout::Placement::Floating { pos }
            } else {
                crate::layout::Placement::Tiled { after: None }
            };
            ewm.layout.add_frame(id, &output, placement);
        }
    }

    /// `open_frames`, then the layout Emacs declares for them.
    fn place(&mut self, output: &str, frames: Vec<Frame>, floating: bool) -> Vec<Frame> {
        self.open_frames(output, &frames, floating);
        self.declare(frames)
    }

    /// The layout Emacs declares: every placed frame as it is, `frames` as given.
    fn declare(&self, frames: Vec<Frame>) -> Vec<Frame> {
        let mut layout: Vec<Frame> = self
            .state
            .ewm
            .layout
            .placed_frames()
            .filter(|placed| !frames.iter().any(|f| f.surface_id == placed.surface_id))
            .cloned()
            .collect();
        layout.extend(frames);
        layout
    }

    /// One layout for `tiled` and `floats` on `output`, the frames opened first as Emacs does.
    pub fn layout(&mut self, output: &str, tiled: Vec<Frame>, mut floats: Vec<Frame>) {
        self.open_frames(output, &tiled, false);
        self.open_frames(output, &floats, true);
        let mut frames = tiled;
        frames.append(&mut floats);
        let layout = self.declare(frames);
        self.state.apply_layout(layout);
    }

    pub fn handle_module_command(&mut self, command: crate::module::ModuleCommand) {
        self.state.handle_module_command(command);
    }

    /// Run `action` as a key or `ewm-action-module` would.
    pub fn do_action(&mut self, action: crate::Action) {
        self.handle_module_command(crate::module::ModuleCommand::Action { action });
    }

    /// A workspace client's activate request for frame `id`.
    pub fn activate_workspace(&mut self, id: u64) {
        crate::protocols::workspace::WorkspaceHandler::activate_workspace(&mut self.state, id);
    }

    pub fn set_text_input_provider(&mut self, provider: crate::im::TextInputProvider) {
        self.handle_module_command(crate::module::ModuleCommand::ConfigureTextInput { provider });
    }

    pub fn set_intercepted_keys(&mut self, keys: Vec<crate::InterceptedKey>) {
        *crate::module::intercepted_keys().write().unwrap() = keys;
    }

    pub fn set_intercepted_keys_from_lisp(&mut self, payload: &str) {
        self.set_intercepted_keys(crate::module::intercepted_keys_from_lisp(payload));
    }

    pub fn send_key_from_lisp(&mut self, payload: &str) {
        self.handle_module_command(crate::module::send_key_from_lisp(payload));
    }

    pub fn set_fullscreen(&mut self, entry_id: crate::LayoutEntryId, on: bool) {
        self.handle_module_command(crate::module::ModuleCommand::SetFullscreen { entry_id, on });
    }

    pub fn configure_idle(&mut self, timeout_secs: Option<u64>, action: &str) {
        self.handle_module_command(crate::module::ModuleCommand::ConfigureIdle {
            timeout_secs,
            action: action.into(),
        });
    }

    pub fn set_idle_inhibited(&mut self, inhibited: bool) {
        self.handle_module_command(crate::module::ModuleCommand::SetIdleInhibited { inhibited });
    }

    pub fn clear_keyboard_capture(&mut self) {
        crate::module::clear_keyboard_capture_holders();
        self.state.sync_keyboard_focus();
    }

    pub fn keyboard_capture_active(&self) -> bool {
        crate::module::get_keyboard_capture()
    }

    pub fn sync_keyboard_focus(&mut self) {
        self.state.sync_keyboard_focus();
    }

    pub fn keyboard_key(&mut self, keycode: u32, pressed: bool) -> crate::input::KeyboardAction {
        use smithay::backend::input::KeyState;

        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.state
            .on_keyboard(keycode, state, InputTime::from_millis(0))
    }

    /// Send a pointer button event at the seat's current pointer focus.
    /// Mirrors the production `button` path.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        self.state
            .pointer_button(state, button, InputTime::from_millis(0));
    }

    /// Put `fingers` down for a swipe.
    pub fn swipe_begin(&mut self, fingers: u32) {
        let event = TestGestureEvent {
            fingers,
            ..TestGestureEvent::default()
        };
        self.state.on_gesture_swipe_begin::<TestInputBackend>(event);
    }

    /// Move the swiping fingers by (`dx`, `dy`).
    pub fn swipe_update(&mut self, dx: f64, dy: f64) {
        let event = TestGestureEvent {
            dx,
            dy,
            ..TestGestureEvent::default()
        };
        self.state
            .on_gesture_swipe_update::<TestInputBackend>(event);
    }

    /// Lift the swiping fingers.
    pub fn swipe_end(&mut self) {
        self.state
            .on_gesture_swipe_end::<TestInputBackend>(TestGestureEvent::default());
    }

    /// End the swipe as libinput does when the fingers change, with nothing to run.
    pub fn swipe_cancel(&mut self) {
        self.state
            .on_gesture_swipe_end::<TestInputBackend>(TestGestureEvent {
                cancelled: true,
                ..TestGestureEvent::default()
            });
    }

    /// Put `fingers` down for a pinch.
    pub fn pinch_begin(&mut self, fingers: u32) {
        let event = TestGestureEvent {
            fingers,
            ..TestGestureEvent::default()
        };
        self.state.on_gesture_pinch_begin::<TestInputBackend>(event);
    }

    /// Move the pinching fingers by (`dx`, `dy`), spread by `scale` and turned by `rotation`.
    pub fn pinch_update(&mut self, dx: f64, dy: f64, scale: f64, rotation: f64) {
        let event = TestGestureEvent {
            dx,
            dy,
            scale,
            rotation,
            ..TestGestureEvent::default()
        };
        self.state
            .on_gesture_pinch_update::<TestInputBackend>(event);
    }

    /// Lift the pinching fingers.
    pub fn pinch_end(&mut self) {
        self.state
            .on_gesture_pinch_end::<TestInputBackend>(TestGestureEvent::default());
    }

    /// Rest `fingers` on the touchpad.
    pub fn hold_begin(&mut self, fingers: u32) {
        let event = TestGestureEvent {
            fingers,
            ..TestGestureEvent::default()
        };
        self.state.on_gesture_hold_begin::<TestInputBackend>(event);
    }

    /// Lift the resting fingers.
    pub fn hold_end(&mut self) {
        self.state
            .on_gesture_hold_end::<TestInputBackend>(TestGestureEvent::default());
    }

    /// Start mirroring `queue_event` into a per-Ewm buffer.
    pub fn enable_event_capture(&mut self) {
        self.state.ewm.captured_events = Some(Vec::new());
    }

    /// Take and clear captured events. Empty if capture is disabled.
    pub fn drain_events(&mut self) -> Vec<crate::event::Event> {
        self.state
            .ewm
            .captured_events
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Give the compositor the event channel Emacs would have registered.
    pub fn install_event_channel(&mut self) {
        let _ = crate::module::drain_commands();
        crate::module::install_event_channel(Box::new(std::io::sink()));
        self.owns_event_channel = true;
    }

    /// Read the channel as `ewm-drain-events` does, then run what the read queued.
    pub fn drain_channel(&mut self) -> Vec<crate::event::Event> {
        let events = crate::module::take_events();
        for command in crate::module::drain_commands() {
            self.state.handle_module_command(command);
        }
        events
    }

    pub fn deferred_key_count(&self) -> usize {
        self.state.ewm.deferred_keys.len()
    }

    /// Hold the animation clock where it is, so a spring or slide started now stays at its start.
    pub fn freeze_clock(&mut self) {
        let clock = &self.state.ewm.animations_clock;
        clock.freeze(clock.now());
    }
}
