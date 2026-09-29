//! In-process Wayland test client.
//!
//! Pairs with `Fixture` via `UnixStream::pair`: one half goes into the server
//! `Display`, the other half lives here so tests drive real xdg-shell protocol
//! and observe actual `WlSurface` / `PopupManager` state.

use std::collections::HashMap;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use smithay::reexports::wayland_server::backend::ClientId;
use smithay::utils::{Logical, Rectangle, Size};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat,
    wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, ExtBackgroundEffectManagerV1,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1;
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1;
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration;
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration_manager::{
    self, OrgKdeKwinServerDecorationManager,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2, zwp_input_method_v2,
};
#[cfg(feature = "testing-layer-shell")]
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// Notable events the client received from the server.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    PointerEnter {
        surface: wl_surface::WlSurface,
        x: f64,
        y: f64,
    },
    PointerLeave {
        surface: wl_surface::WlSurface,
    },
    PointerMotion {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    KeyboardKey {
        key: u32,
        pressed: bool,
    },
    KeyboardModifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    ToplevelConfigure {
        width: i32,
        height: i32,
    },
    SurfaceEnter {
        surface: wl_surface::WlSurface,
        output: wl_output::WlOutput,
    },
    SurfaceLeave {
        surface: wl_surface::WlSurface,
        output: wl_output::WlOutput,
    },
    TextInputCommitString {
        text: Option<String>,
    },
    TextInputDeleteSurroundingText {
        before: u32,
        after: u32,
    },
    TextInputDone {
        serial: u32,
    },
    KdeDecorationDefaultMode {
        mode: WEnum<org_kde_kwin_server_decoration_manager::Mode>,
    },
    KdeDecorationMode {
        mode: WEnum<org_kde_kwin_server_decoration::Mode>,
    },
    BackgroundEffectCapabilities {
        flags: WEnum<ext_background_effect_manager_v1::Capability>,
    },
}

pub struct TestClient {
    pub(crate) server_client_id: ClientId,
    pub(crate) conn: Connection,
    pub(crate) eq: EventQueue<ClientState>,
    pub(crate) state: ClientState,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    pub shm: Option<wl_shm::WlShm>,
    pub seat: Option<wl_seat::WlSeat>,
    pub pointer: Option<wl_pointer::WlPointer>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub input_method_manager: Option<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    pub system_bell: Option<xdg_system_bell_v1::XdgSystemBellV1>,
    pub kde_decoration_manager: Option<OrgKdeKwinServerDecorationManager>,
    pub background_effect_manager: Option<ExtBackgroundEffectManagerV1>,
    #[cfg(feature = "testing-layer-shell")]
    pub layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    /// `wl_output` protocol id -> server-advertised name.
    pub output_names: HashMap<u32, String>,
    pub events: Vec<ClientEvent>,
}

#[derive(Clone)]
pub struct Toplevel {
    pub surface: wl_surface::WlSurface,
    pub xdg_surface: xdg_surface::XdgSurface,
    pub xdg_toplevel: xdg_toplevel::XdgToplevel,
}

#[derive(Clone)]
pub struct Popup {
    pub surface: wl_surface::WlSurface,
    pub xdg_surface: xdg_surface::XdgSurface,
    pub xdg_popup: xdg_popup::XdgPopup,
}

#[cfg(feature = "testing-layer-shell")]
#[derive(Clone)]
pub struct LayerSurface {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
}

impl TestClient {
    pub fn new(
        stream: UnixStream,
        server_client_id: ClientId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::from_socket(stream)?;
        let eq = conn.new_event_queue::<ClientState>();
        let qh = eq.handle();
        let _registry = conn.display().get_registry(&qh, ());
        Ok(Self {
            server_client_id,
            conn,
            eq,
            state: ClientState::default(),
        })
    }

    pub fn qh(&self) -> QueueHandle<ClientState> {
        self.eq.handle()
    }

    pub fn server_client_id(&self) -> ClientId {
        self.server_client_id.clone()
    }

    /// Take and clear the recorded client-side events.
    pub fn drain_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.state.events)
    }

    /// Server-advertised name for the `wl_output` proxy with this protocol id,
    /// or `None` if the client hasn't received a `Name` event for it yet.
    pub fn output_name(&self, proxy_id: u32) -> Option<&str> {
        self.state.output_names.get(&proxy_id).map(|s| s.as_str())
    }

    pub fn has_keyboard(&self) -> bool {
        self.state.keyboard.is_some()
    }

    fn compositor(&self) -> &wl_compositor::WlCompositor {
        self.state
            .compositor
            .as_ref()
            .expect("wl_compositor not bound; roundtrip the registry first")
    }

    fn wm_base(&self) -> &xdg_wm_base::XdgWmBase {
        self.state
            .xdg_wm_base
            .as_ref()
            .expect("xdg_wm_base not bound; roundtrip the registry first")
    }

    #[cfg(feature = "testing-layer-shell")]
    fn layer_shell(&self) -> &zwlr_layer_shell_v1::ZwlrLayerShellV1 {
        self.state
            .layer_shell
            .as_ref()
            .expect("zwlr_layer_shell_v1 not bound; roundtrip the registry first")
    }

    fn shm(&self) -> &wl_shm::WlShm {
        self.state
            .shm
            .as_ref()
            .expect("wl_shm not bound; roundtrip the registry first")
    }

    /// Zero-filled ARGB8888 SHM buffer of the given size.
    pub fn create_shm_buffer(&self, size: Size<i32, Logical>) -> wl_buffer::WlBuffer {
        let qh = self.eq.handle();
        let stride = size.w * 4;
        let total = stride * size.h;
        let fd = create_memfd(total as usize);
        let pool = self.shm().create_pool(fd.as_fd(), total, &qh, ());
        let buffer =
            pool.create_buffer(0, size.w, size.h, stride, wl_shm::Format::Argb8888, &qh, ());
        pool.destroy();
        buffer
    }

    /// Caller must roundtrip before reading server-side state.
    pub fn create_toplevel(&mut self, title: &str) -> Toplevel {
        let qh = self.eq.handle();
        let surface = self.compositor().create_surface(&qh, ());
        let xdg_surface = self.wm_base().get_xdg_surface(&surface, &qh, ());
        let xdg_toplevel = xdg_surface.get_toplevel(&qh, ());
        xdg_toplevel.set_title(title.to_string());
        xdg_toplevel.set_app_id("ewm-test".into());
        surface.commit();
        Toplevel {
            surface,
            xdg_surface,
            xdg_toplevel,
        }
    }

    pub fn create_text_input(&mut self) -> zwp_text_input_v3::ZwpTextInputV3 {
        let qh = self.eq.handle();
        let seat = self.state.seat.clone().expect("wl_seat not bound");
        self.state
            .text_input_manager
            .as_ref()
            .expect("zwp_text_input_manager_v3 not bound")
            .get_text_input(&seat, &qh, ())
    }

    /// A running IME; smithay discards text-input requests without one.
    pub fn create_input_method(&mut self) -> zwp_input_method_v2::ZwpInputMethodV2 {
        let qh = self.eq.handle();
        let seat = self.state.seat.clone().expect("wl_seat not bound");
        self.state
            .input_method_manager
            .as_ref()
            .expect("zwp_input_method_manager_v2 not bound")
            .get_input_method(&seat, &qh, ())
    }

    pub fn create_surface(&self) -> wl_surface::WlSurface {
        self.compositor().create_surface(&self.qh(), ())
    }

    pub fn create_kde_decoration(
        &self,
        surface: &wl_surface::WlSurface,
    ) -> org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration {
        self.state
            .kde_decoration_manager
            .as_ref()
            .expect("kde decoration manager not bound; roundtrip the registry first")
            .create(surface, &self.qh(), ())
    }

    pub fn create_region(&self) -> wl_region::WlRegion {
        self.compositor().create_region(&self.qh(), ())
    }

    pub fn create_background_effect(
        &self,
        surface: &wl_surface::WlSurface,
    ) -> ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1 {
        self.state
            .background_effect_manager
            .as_ref()
            .expect("background effect manager not bound; roundtrip the registry first")
            .get_background_effect(surface, &self.qh(), ())
    }

    pub fn ring_bell(&self, surface: Option<&wl_surface::WlSurface>) {
        self.state
            .system_bell
            .as_ref()
            .expect("xdg_system_bell_v1 not bound; roundtrip the registry first")
            .ring(surface);
    }

    /// Caller must roundtrip before attaching the first buffer.
    #[cfg(feature = "testing-layer-shell")]
    pub fn create_layer_surface(
        &mut self,
        namespace: &str,
        size: Size<i32, Logical>,
    ) -> LayerSurface {
        let qh = self.eq.handle();
        let surface = self.compositor().create_surface(&qh, ());
        let layer_surface = self.layer_shell().get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            namespace.to_string(),
            &qh,
            (),
        );
        layer_surface.set_size(size.w as u32, size.h as u32);
        layer_surface
            .set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        surface.commit();
        LayerSurface {
            surface,
            layer_surface,
        }
    }

    /// Caller must roundtrip before reading server-side state.
    pub fn create_popup(
        &mut self,
        parent: &Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Popup {
        self.create_popup_on(&parent.xdg_surface, anchor, size)
    }

    /// Create a popup whose parent is an arbitrary xdg_surface (a toplevel or,
    /// for a submenu, another popup).
    pub fn create_popup_on(
        &mut self,
        parent_xdg: &xdg_surface::XdgSurface,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Popup {
        self.create_popup_with(
            parent_xdg,
            anchor,
            size,
            xdg_positioner::ConstraintAdjustment::None,
        )
    }

    /// Like `create_popup`, asking the compositor to slide the popup on screen.
    pub fn create_sliding_popup(
        &mut self,
        parent: &Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Popup {
        self.create_popup_with(
            &parent.xdg_surface,
            anchor,
            size,
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::SlideY,
        )
    }

    fn create_popup_with(
        &mut self,
        parent_xdg: &xdg_surface::XdgSurface,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
        adjustment: xdg_positioner::ConstraintAdjustment,
    ) -> Popup {
        let qh = self.eq.handle();
        let positioner = self.wm_base().create_positioner(&qh, ());
        positioner.set_anchor_rect(anchor.loc.x, anchor.loc.y, anchor.size.w, anchor.size.h);
        positioner.set_size(size.w, size.h);
        positioner.set_anchor(xdg_positioner::Anchor::BottomLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(adjustment);
        let surface = self.compositor().create_surface(&qh, ());
        let xdg_surface = self.wm_base().get_xdg_surface(&surface, &qh, ());
        let xdg_popup = xdg_surface.get_popup(Some(parent_xdg), &positioner, &qh, ());
        positioner.destroy();
        surface.commit();
        Popup {
            surface,
            xdg_surface,
            xdg_popup,
        }
    }

    /// Request an `xdg_popup.grab` on the seat with `serial`.
    pub fn grab_popup(&self, popup: &Popup, serial: u32) {
        let seat = self.state.seat.as_ref().expect("wl_seat not bound");
        popup.xdg_popup.grab(seat, serial);
    }

    /// Caller must roundtrip before reading server-side state.
    #[cfg(feature = "testing-layer-shell")]
    pub fn create_layer_popup(
        &mut self,
        parent: &LayerSurface,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Popup {
        let qh = self.eq.handle();
        let positioner = self.wm_base().create_positioner(&qh, ());
        positioner.set_anchor_rect(anchor.loc.x, anchor.loc.y, anchor.size.w, anchor.size.h);
        positioner.set_size(size.w, size.h);
        positioner.set_anchor(xdg_positioner::Anchor::BottomLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let surface = self.compositor().create_surface(&qh, ());
        let xdg_surface = self.wm_base().get_xdg_surface(&surface, &qh, ());
        let xdg_popup = xdg_surface.get_popup(None, &positioner, &qh, ());
        parent.layer_surface.get_popup(&xdg_popup);
        positioner.destroy();
        surface.commit();
        Popup {
            surface,
            xdg_surface,
            xdg_popup,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        if interface == wl_compositor::WlCompositor::interface().name {
            state.compositor = Some(registry.bind(name, version, qh, ()));
        } else if interface == xdg_wm_base::XdgWmBase::interface().name {
            state.xdg_wm_base = Some(registry.bind(name, version, qh, ()));
        } else if interface == wl_shm::WlShm::interface().name {
            state.shm = Some(registry.bind(name, version, qh, ()));
        } else if interface == wl_seat::WlSeat::interface().name {
            state.seat = Some(registry.bind(name, version, qh, ()));
        } else if interface == wl_output::WlOutput::interface().name {
            let _: wl_output::WlOutput = registry.bind(name, version, qh, ());
        } else if interface == zwp_text_input_manager_v3::ZwpTextInputManagerV3::interface().name {
            state.text_input_manager = Some(registry.bind(name, version, qh, ()));
        } else if interface
            == zwp_input_method_manager_v2::ZwpInputMethodManagerV2::interface().name
        {
            state.input_method_manager = Some(registry.bind(name, version, qh, ()));
        } else if interface == xdg_system_bell_v1::XdgSystemBellV1::interface().name {
            state.system_bell = Some(registry.bind(name, version, qh, ()));
        } else if interface == OrgKdeKwinServerDecorationManager::interface().name {
            state.kde_decoration_manager = Some(registry.bind(name, version, qh, ()));
        } else if interface == ExtBackgroundEffectManagerV1::interface().name {
            state.background_effect_manager = Some(registry.bind(name, version, qh, ()));
        }
        #[cfg(feature = "testing-layer-shell")]
        if interface == zwlr_layer_shell_v1::ZwlrLayerShellV1::interface().name {
            state.layer_shell = Some(registry.bind(name, version, qh, ()));
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for ClientState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_names.insert(output.id().protocol_id(), name);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClientState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => state.events.push(ClientEvent::PointerEnter {
                surface,
                x: surface_x,
                y: surface_y,
            }),
            wl_pointer::Event::Leave { surface, .. } => {
                state.events.push(ClientEvent::PointerLeave { surface })
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.events.push(ClientEvent::PointerMotion {
                x: surface_x,
                y: surface_y,
            }),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                ..
            } => state.events.push(ClientEvent::PointerButton {
                button,
                pressed: button_state == wl_pointer::ButtonState::Pressed,
            }),
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => state.events.push(ClientEvent::KeyboardKey {
                key,
                pressed: key_state == wl_keyboard::KeyState::Pressed,
            }),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => state.events.push(ClientEvent::KeyboardModifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            }),
            _ => {}
        }
    }
}

#[cfg(feature = "testing-layer-shell")]
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for ClientState {
    fn event(
        _: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            layer_surface.ack_configure(serial);
        }
    }
}

/// Anonymous file-backed shared memory, zero-filled on `ftruncate`.
fn create_memfd(size: usize) -> OwnedFd {
    let name = c"ewm-test-shm";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        panic!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd, size as i64) } < 0 {
        panic!("ftruncate failed: {}", std::io::Error::last_os_error());
    }
    owned
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientState {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            state
                .events
                .push(ClientEvent::ToplevelConfigure { width, height });
        }
    }
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &OrgKdeKwinServerDecorationManager,
        event: org_kde_kwin_server_decoration_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let org_kde_kwin_server_decoration_manager::Event::DefaultMode { mode } = event {
            state
                .events
                .push(ClientEvent::KdeDecorationDefaultMode { mode });
        }
    }
}

impl Dispatch<org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        event: org_kde_kwin_server_decoration::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let org_kde_kwin_server_decoration::Event::Mode { mode } = event {
            state.events.push(ClientEvent::KdeDecorationMode { mode });
        }
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            state
                .events
                .push(ClientEvent::BackgroundEffectCapabilities { flags });
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { output } => state.events.push(ClientEvent::SurfaceEnter {
                surface: surface.clone(),
                output,
            }),
            wl_surface::Event::Leave { output } => state.events.push(ClientEvent::SurfaceLeave {
                surface: surface.clone(),
                output,
            }),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: ignore xdg_popup::XdgPopup);
wayland_client::delegate_noop!(ClientState: ignore xdg_positioner::XdgPositioner);
wayland_client::delegate_noop!(ClientState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(ClientState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ClientState: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(ClientState: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
wayland_client::delegate_noop!(ClientState: ignore xdg_system_bell_v1::XdgSystemBellV1);
wayland_client::delegate_noop!(ClientState: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(ClientState: ignore ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1);

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::CommitString { text } => state
                .events
                .push(ClientEvent::TextInputCommitString { text }),
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => state
                .events
                .push(ClientEvent::TextInputDeleteSurroundingText {
                    before: before_length,
                    after: after_length,
                }),
            zwp_text_input_v3::Event::Done { serial } => {
                state.events.push(ClientEvent::TextInputDone { serial })
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(ClientState: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);
wayland_client::delegate_noop!(ClientState: ignore zwp_input_method_v2::ZwpInputMethodV2);
#[cfg(feature = "testing-layer-shell")]
wayland_client::delegate_noop!(ClientState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
