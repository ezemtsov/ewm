//! Winit backend for running EWM nested inside another compositor
//!
//! This backend is used when WAYLAND_DISPLAY or DISPLAY environment
//! variables are set, indicating we're running inside an existing
//! desktop environment.

use smithay::{
    backend::{
        allocator::Fourcc,
        input::{
            AbsolutePositionEvent, Axis, ButtonState, Event, InputEvent, KeyboardKeyEvent,
            PointerAxisEvent, PointerButtonEvent,
        },
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer, ExportMem},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display},
    utils::{Transform, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};
use tracing::{error, info};

use crate::{
    input::{handle_keyboard_event, release_all_keys, restore_focus, KeyboardAction},
    ipc::setup_ipc_listener,
    render::collect_render_elements,
    spawn_client, Ewm, LoopData,
};

/// Winit backend state (minimal, most state is in Ewm)
#[allow(dead_code)]
pub struct WinitBackend {
    // Winit backend doesn't need much state since rendering is synchronous
}

#[allow(dead_code)]
impl WinitBackend {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for WinitBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Run EWM with Winit backend (nested mode)
pub fn run_winit(program: String, program_args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let state = Ewm::new(display_handle.clone());
    let mut data = LoopData {
        state,
        display,
        emacs: None,
    };

    // Set up IPC listener (shared code)
    setup_ipc_listener(&event_loop.handle())?;

    // Winit backend
    let (mut backend, mut winit_evt): (WinitGraphicsBackend<GlesRenderer>, _) =
        winit::init().map_err(|e| format!("Failed to init winit: {:?}", e))?;

    // Output
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "EWM".into(),
            model: "Winit".into(),
        },
    );
    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    // Transform::Flipped180 is required for winit backend because OpenGL has Y=0 at
    // bottom while window systems have Y=0 at top. This flip corrects the rendering.
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
    output.set_preferred(mode);
    output.create_global::<Ewm>(&display_handle);
    data.state.space.map_output(&output, (0, 0));
    data.state.output_size = (mode.size.w, mode.size.h);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Spawn client inside the compositor
    let mut client_process = spawn_client(&program, &program_args, &socket_name_str)?;
    let emacs_pid = client_process.id();
    info!("Client spawned with PID {}", emacs_pid);
    data.state.set_emacs_pid(emacs_pid);

    info!("EWM compositor started");

    // Main loop
    let mut screenshot_path: Option<String> = None;

    while data.state.running {
        // Check if client has exited
        match client_process.try_wait() {
            Ok(Some(status)) => {
                info!("Client exited with status: {}", status);
                data.state.running = false;
                break;
            }
            Ok(None) => {} // Still running
            Err(e) => {
                error!("Error checking client process: {}", e);
            }
        }

        // Collect input events
        let mut input_events = Vec::new();

        // Winit events
        let _ = winit_evt.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                data.state.output_size = (size.w, size.h);

                // Notify all surfaces of new size so they can resize
                for window in data.state.space.elements() {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some((size.w, size.h).into());
                        });
                        toplevel.send_configure();
                    }
                }
                info!(
                    "Output resized to {}x{}, notified {} surfaces",
                    size.w,
                    size.h,
                    data.state.space.elements().count()
                );
            }
            WinitEvent::CloseRequested => {
                data.state.running = false;
            }
            WinitEvent::Focus(focused) => {
                let keyboard = data.state.seat.get_keyboard().unwrap();

                if !focused {
                    // Window lost focus - release all currently pressed keys
                    info!("Window lost focus, releasing pressed keys");
                    release_all_keys(&mut data.state, &keyboard);
                } else {
                    // Restore focus to the previously focused surface
                    let target_id = data.state.focused_surface_id;
                    restore_focus(&mut data.state, &keyboard, target_id);
                }
            }
            WinitEvent::Input(event) => {
                input_events.push(event);
            }
            _ => {}
        });

        // Process input events
        for event in input_events {
            match event {
                InputEvent::Keyboard { event } => {
                    let keyboard = data.state.seat.get_keyboard().unwrap();
                    let action = handle_keyboard_event(
                        &mut data.state,
                        &keyboard,
                        event.key_code().into(),
                        event.state(),
                        Event::time_msec(&event),
                    );

                    if action == KeyboardAction::Shutdown {
                        info!("Kill combo pressed (Super+Ctrl+Backspace), shutting down");
                        data.state.running = false;
                    }
                }
                InputEvent::PointerMotionAbsolute { event } => {
                    let output_geo = data.state.space.output_geometry(&output).unwrap();
                    let pos = event.position_transformed(output_geo.size);
                    data.state.pointer_location = (pos.x, pos.y);

                    let pointer = data.state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Find surface under pointer
                    let under = data
                        .state
                        .space
                        .element_under((pos.x, pos.y))
                        .and_then(|(window, loc)| {
                            window
                                .wl_surface()
                                .map(|s| (s.into_owned(), (loc.x as f64, loc.y as f64).into()))
                        });

                    pointer.motion(
                        &mut data.state,
                        under,
                        &MotionEvent {
                            location: pos.into(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut data.state);
                }
                InputEvent::PointerButton { event } => {
                    let pointer = data.state.seat.get_pointer().unwrap();
                    let keyboard = data.state.seat.get_keyboard().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    let button_state = match event.state() {
                        ButtonState::Pressed => smithay::backend::input::ButtonState::Pressed,
                        ButtonState::Released => smithay::backend::input::ButtonState::Released,
                    };

                    // Click-to-focus: on button press, focus the surface under pointer
                    if button_state == smithay::backend::input::ButtonState::Pressed {
                        let (px, py) = data.state.pointer_location;
                        let focus_info = data
                            .state
                            .space
                            .element_under((px, py))
                            .and_then(|(window, _)| {
                                let id = data.state.window_ids.get(&window).copied()?;
                                let surface = window.wl_surface()?.into_owned();
                                Some((id, surface))
                            });

                        if let Some((id, surface)) = focus_info {
                            data.state.set_focus(id);
                            data.state.keyboard_focus = Some(surface.clone());
                            keyboard.set_focus(&mut data.state, Some(surface), serial);
                        }
                    }

                    pointer.button(
                        &mut data.state,
                        &ButtonEvent {
                            button: event.button_code(),
                            state: button_state,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut data.state);
                }
                InputEvent::PointerAxis { event } => {
                    let pointer = data.state.seat.get_pointer().unwrap();

                    let source = event.source();
                    let horizontal = event
                        .amount(Axis::Horizontal)
                        .or_else(|| event.amount_v120(Axis::Horizontal).map(|v| v / 120.0 * 15.0))
                        .unwrap_or(0.0);
                    let vertical = event
                        .amount(Axis::Vertical)
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v / 120.0 * 15.0))
                        .unwrap_or(0.0);

                    let mut frame = AxisFrame::new(event.time_msec()).source(source);
                    if horizontal != 0.0 {
                        frame = frame.value(Axis::Horizontal, horizontal);
                    }
                    if vertical != 0.0 {
                        frame = frame.value(Axis::Vertical, vertical);
                    }

                    pointer.axis(&mut data.state, frame);
                    pointer.frame(&mut data.state);
                }
                _ => {}
            }
        }

        // Render
        let taking_screenshot = screenshot_path.is_some();
        {
            backend.bind()?;
            let age = backend.buffer_age().unwrap_or(0);
            let renderer = backend.renderer();

            let elements = collect_render_elements(&data.state, renderer, 1.0.into());
            let result = damage_tracker.render_output(renderer, age, &elements, [0.1, 0.1, 0.1, 1.0]);

            match result {
                Ok(render_output_result) => {
                    // Screenshot capture
                    if let Some(ref path) = screenshot_path {
                        capture_screenshot(renderer, &data.state, path);
                        screenshot_path = None;
                    }

                    // Submit if there was damage (and not taking screenshot)
                    if !taking_screenshot {
                        if let Some(ref damage) = render_output_result.damage {
                            backend.submit(Some(damage.as_slice()))?;
                        }
                    }
                }
                Err(e) => {
                    error!("Render error: {:?}", e);
                }
            }
        }

        // Frame callbacks
        data.state.space.elements().for_each(|window| {
            window.send_frame(&output, std::time::Duration::ZERO, None, |_, _| {
                Some(output.clone())
            });
        });

        data.state.space.refresh();
        data.display.flush_clients().unwrap();

        // Flush pending events to Emacs (shared IPC handling)
        data.flush_events();

        // Check for pending screenshot request
        if let Some(path) = data.state.pending_screenshot.take() {
            screenshot_path = Some(path);
        }

        event_loop.dispatch(None, &mut data)?;
    }

    Ok(())
}

/// Capture a screenshot from the current framebuffer
fn capture_screenshot(renderer: &mut GlesRenderer, state: &Ewm, path: &str) {
    let size = state.output_size;
    let mapping = renderer.copy_framebuffer(
        smithay::utils::Rectangle::from_size((size.0, size.1).into()),
        Fourcc::Xrgb8888,
    );

    if let Ok(mapping) = mapping {
        if let Ok(pixel_data) = renderer.map_texture(&mapping) {
            let width = size.0 as usize;
            let height = size.1 as usize;
            let stride = width * 4;

            // Convert and flip vertically
            let mut rgb_data: Vec<u8> = Vec::with_capacity(width * height * 3);
            for y in (0..height).rev() {
                let row_start = y * stride;
                for x in 0..width {
                    let pixel_start = row_start + x * 4;
                    if pixel_start + 4 <= pixel_data.len() {
                        // BGRX layout in memory
                        let b = pixel_data[pixel_start];
                        let g = pixel_data[pixel_start + 1];
                        let r = pixel_data[pixel_start + 2];
                        rgb_data.extend_from_slice(&[r, g, b]);
                    }
                }
            }

            if let Some(img) = image::RgbImage::from_raw(width as u32, height as u32, rgb_data) {
                if let Err(e) = img.save(path) {
                    error!("Failed to save screenshot: {}", e);
                } else {
                    info!("Screenshot saved to {}", path);
                }
            } else {
                error!("Failed to create image buffer");
            }
        }
    }
}
