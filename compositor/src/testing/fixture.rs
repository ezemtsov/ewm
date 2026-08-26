//! Test fixture for integration testing
//!
//! The Fixture provides a complete compositor environment for testing,
//! including a headless backend, event loop, and Wayland display.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::input::ButtonState;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tracing::info;

use crate::backend::{Backend, HeadlessBackend};
use crate::cursor::CursorConfig;
use crate::strip::Frame;
use crate::testing::TestClient;
use crate::{register_display_source, ClientState, Ewm, OutputInfo, State};

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
        let ewm = Ewm::new(display_handle, event_loop.handle(), false, cursor_config);
        let backend = Backend::Headless(HeadlessBackend::new());

        let state = State { backend, ewm };

        register_display_source(&event_loop.handle(), display);

        info!("Test fixture initialized with headless backend");

        Ok(Self { event_loop, state })
    }

    /// Pair a `UnixStream` with the server `Display`, returning a `TestClient`
    /// on the other end. Roundtrip after to bind globals.
    pub fn new_client(&mut self) -> Result<TestClient, Box<dyn std::error::Error>> {
        let (server_end, client_end) = UnixStream::pair()?;
        let server_client = self
            .state
            .ewm
            .display_handle
            .insert_client(server_end, Arc::new(ClientState::default()))?;
        TestClient::new(client_end, server_client.id())
    }

    /// Attach an SHM buffer + commit; populates server-side surface_view and
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

    /// Drive a full xdg toplevel lifecycle.
    pub fn open_toplevel(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = client.create_toplevel(title);
        self.roundtrip(client);
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
        let toplevel = client.create_toplevel(title);
        toplevel
            .xdg_toplevel
            .set_parent(Some(&parent.xdg_toplevel));
        toplevel.surface.commit();
        self.roundtrip(client);
        self.finish_xdg(client, &toplevel.surface, &toplevel.xdg_surface, size);
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
            self.refresh_and_flush_clients();
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
        self.state.ewm.id_windows.iter().find_map(|(&id, window)| {
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
        self.refresh_and_flush_clients();
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

    /// Get immutable access to the compositor state
    pub fn ewm_ref(&self) -> &Ewm {
        &self.state.ewm
    }

    pub fn output_info_list(&self) -> Vec<OutputInfo> {
        self.state
            .backend
            .output_info_list(&self.state.ewm.sorted_outputs)
    }

    pub fn debug_state(&self) -> serde_json::Value {
        self.state.ewm.debug_state(&self.output_info_list())
    }

    /// Get the focused surface ID
    pub fn focused_surface_id(&self) -> u64 {
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

    /// Apply stored output config for the named output (headless backend)
    pub fn apply_output_config(&mut self, output_name: &str) {
        self.state
            .backend
            .apply_output_config(&mut self.state.ewm, output_name);
    }

    /// Check if a surface with the given ID exists
    pub fn has_surface(&self, id: u64) -> bool {
        self.state.ewm.id_windows.contains_key(&id)
    }

    /// Warp the pointer to an absolute position.
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        self.state.warp_pointer(x, y);
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
                time: 0,
            },
        );
        pointer.frame(&mut self.state);
        self.state.ewm.queue_redraw_for_pointer();
    }

    /// Apply a layout through the same command path used by Emacs.
    pub fn apply_output_layout_command(&mut self, output: &str, frames: Vec<Frame>) {
        self.state
            .handle_module_command(crate::module::ModuleCommand::OutputLayout {
                output: output.to_string(),
                frames,
            });
        self.state.sync_keyboard_focus();
    }

    pub fn handle_module_command(&mut self, command: crate::module::ModuleCommand) {
        self.state.handle_module_command(command);
    }

    pub fn set_intercepted_keys(&mut self, keys: Vec<crate::InterceptedKey>) {
        crate::module::set_intercepted_keys_for_test(keys);
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
        crate::input::handle_keyboard_event(&mut self.state, keycode, state, 0)
    }

    /// Send a pointer button event at the seat's current pointer focus.
    /// Mirrors the production `button` path.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        crate::input::handle_pointer_button_state(&mut self.state, state, button, 0);
    }

    /// Start mirroring `queue_event` into a per-Ewm buffer.
    pub fn enable_event_capture(&mut self) {
        self.state.ewm.captured_events = Some(Vec::new());
    }

    /// Take and clear captured events.  Empty if capture is disabled.
    pub fn drain_events(&mut self) -> Vec<crate::event::Event> {
        self.state
            .ewm
            .captured_events
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Per-frame processing callback.
    fn refresh_and_flush_clients(&mut self) {
        self.state.refresh_and_flush_clients();
    }
}
