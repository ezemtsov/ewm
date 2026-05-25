//! org.gnome.Mutter.DisplayConfig D-Bus interface implementation
//!
//! Based on niri's `dbus/mutter_display_config.rs`. This interface is used
//! by xdg-desktop-portal-gnome to enumerate monitors.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use smithay::utils::{Physical, Size};
use tracing::info;
use zbus::blocking::Connection;
use zbus::zvariant::{self, OwnedValue, Type};
use zbus::{fdo, interface};

use super::{OutputInfo, Start};

/// DisplayConfig D-Bus interface
#[derive(Clone)]
pub struct DisplayConfig {
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

impl DisplayConfig {
    pub fn new(outputs: Arc<Mutex<Vec<OutputInfo>>>) -> Self {
        Self { outputs }
    }
}

/// Monitor information for D-Bus
#[derive(Serialize, Type)]
pub struct Monitor {
    /// (connector, vendor, product, serial)
    names: (String, String, String, String),
    modes: Vec<Mode>,
    properties: HashMap<String, OwnedValue>,
}

/// Mode information
#[derive(Serialize, Type)]
pub struct Mode {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: HashMap<String, OwnedValue>,
}

/// Logical monitor (an enabled output with position/scale)
#[derive(Serialize, Type)]
pub struct LogicalMonitor {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    is_primary: bool,
    /// List of (connector, vendor, product, serial) tuples
    monitors: Vec<(String, String, String, String)>,
    properties: HashMap<String, OwnedValue>,
}

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfig {
    /// Get the current display configuration state
    async fn get_current_state(
        &self,
    ) -> fdo::Result<(
        u32,                         // serial
        Vec<Monitor>,                // monitors
        Vec<LogicalMonitor>,         // logical_monitors
        HashMap<String, OwnedValue>, // properties
    )> {
        info!("DisplayConfig::get_current_state() called");
        let outputs = self.outputs.lock().unwrap();
        info!(
            "DisplayConfig::get_current_state() - found {} outputs",
            outputs.len()
        );

        let mut monitors = Vec::new();
        let mut logical_monitors = Vec::new();

        for output in outputs.iter() {
            let connector = output.name.clone();
            let vendor = output.make.clone();
            let product = output.model.clone();
            let serial = if output.serial.is_empty() {
                connector.clone()
            } else {
                output.serial.clone()
            };
            let is_builtin = crate::is_laptop_panel(&connector);
            let display_name = make_display_name(output, is_builtin);

            let modes = output
                .modes
                .iter()
                .map(|mode| {
                    let refresh_rate = mode.refresh as f64 / 1000.0;
                    let mut properties = HashMap::new();
                    properties.insert("is-preferred".to_string(), OwnedValue::from(mode.preferred));
                    if mode.current {
                        properties.insert("is-current".to_string(), OwnedValue::from(true));
                    }

                    Mode {
                        id: format!("{}x{}@{:.3}", mode.width, mode.height, refresh_rate),
                        width: mode.width,
                        height: mode.height,
                        refresh_rate,
                        preferred_scale: output.scale,
                        supported_scales: supported_scales(Size::from((mode.width, mode.height)))
                            .collect(),
                        properties,
                    }
                })
                .collect();

            let names = (connector.clone(), vendor, product, serial);

            // Display name property
            let mut properties = HashMap::new();
            properties.insert(
                "display-name".to_string(),
                OwnedValue::from(zvariant::Str::from(display_name)),
            );
            properties.insert("is-builtin".to_string(), OwnedValue::from(is_builtin));

            monitors.push(Monitor {
                names: names.clone(),
                modes,
                properties,
            });

            // Create logical monitor with actual position from compositor
            logical_monitors.push(LogicalMonitor {
                x: output.x,
                y: output.y,
                scale: output.scale,
                transform: output.transform as u32,
                is_primary: false,
                monitors: vec![names],
                properties: HashMap::new(),
            });
        }

        // Sort by connector name
        monitors.sort_unstable_by(|a, b| a.names.0.cmp(&b.names.0));
        logical_monitors.sort_unstable_by(|a, b| a.monitors[0].0.cmp(&b.monitors[0].0));

        let properties = HashMap::from([("layout-mode".to_string(), OwnedValue::from(1u32))]);

        Ok((0, monitors, logical_monitors, properties))
    }

    #[zbus(property)]
    fn power_save_mode(&self) -> i32 {
        -1
    }

    #[zbus(property)]
    fn set_power_save_mode(&self, _mode: i32) -> zbus::Result<()> {
        Err(zbus::Error::Unsupported)
    }

    #[zbus(property)]
    fn panel_orientation_managed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn night_light_supported(&self) -> bool {
        false
    }
}

impl Start for DisplayConfig {
    fn start(self) -> anyhow::Result<Connection> {
        use zbus::fdo::RequestNameFlags;

        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/DisplayConfig", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.DisplayConfig", flags)?;

        Ok(conn)
    }
}

fn make_display_name(output: &OutputInfo, is_builtin: bool) -> String {
    if is_builtin {
        return "Built-in display".to_owned();
    }

    let make = output.make.trim();
    let model = output.model.trim();
    if !make.is_empty() && !model.is_empty() && model != "Unknown" {
        format!("{make} {model}")
    } else if !make.is_empty() && make != "Unknown" {
        make.to_owned()
    } else if !model.is_empty() && model != "Unknown" {
        model.to_owned()
    } else {
        output.name.clone()
    }
}

fn supported_scales(resolution: Size<i32, Physical>) -> impl Iterator<Item = f64> {
    const MIN_SCALE: i32 = 1;
    const MAX_SCALE: i32 = 4;
    const STEPS: i32 = 4;

    (MIN_SCALE * STEPS..=MAX_SCALE * STEPS)
        .map(|x| f64::from(x) / f64::from(STEPS))
        .filter(move |scale| is_valid_scale_for_resolution(resolution, *scale))
}

fn is_valid_scale_for_resolution(resolution: Size<i32, Physical>, scale: f64) -> bool {
    const MIN_LOGICAL_AREA: i32 = 800 * 480;

    let logical = resolution.to_f64().to_logical(scale).to_i32_round::<i32>();
    logical.w * logical.h >= MIN_LOGICAL_AREA
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str) -> OutputInfo {
        OutputInfo {
            name: name.to_owned(),
            make: "Acme".to_owned(),
            model: "Panel".to_owned(),
            serial: String::new(),
            width_mm: 600,
            height_mm: 340,
            x: 0,
            y: 0,
            scale: 1.5,
            transform: 0,
            modes: vec![
                crate::OutputMode {
                    width: 1280,
                    height: 720,
                    refresh: 60_000,
                    preferred: false,
                    current: false,
                },
                crate::OutputMode {
                    width: 1920,
                    height: 1080,
                    refresh: 144_000,
                    preferred: true,
                    current: true,
                },
            ],
        }
    }

    #[test]
    fn apply_monitors_config_is_not_advertised_without_handler() {
        let config = DisplayConfig::new(Arc::new(Mutex::new(Vec::new())));

        assert!(!config.apply_monitors_config_allowed());
    }

    #[test]
    fn monitor_serial_falls_back_to_connector_name() {
        let outputs = Arc::new(Mutex::new(vec![output("HDMI-A-1")]));
        let config = DisplayConfig::new(outputs);

        let (_, monitors, _, _) = async_io::block_on(config.get_current_state()).unwrap();

        assert_eq!(monitors[0].names.3, "HDMI-A-1");
    }

    #[test]
    fn builtin_connector_is_reported_as_builtin_display() {
        let outputs = Arc::new(Mutex::new(vec![output("eDP-1")]));
        let config = DisplayConfig::new(outputs);

        let (_, monitors, _, _) = async_io::block_on(config.get_current_state()).unwrap();

        assert_eq!(
            monitors[0].properties["display-name"],
            OwnedValue::from(zvariant::Str::from("Built-in display"))
        );
        assert_eq!(monitors[0].properties["is-builtin"], OwnedValue::from(true));
    }

    #[test]
    fn logical_monitors_do_not_invent_a_primary_output() {
        let mut second = output("DP-1");
        second.x = 1920;
        let outputs = Arc::new(Mutex::new(vec![output("HDMI-A-1"), second]));
        let config = DisplayConfig::new(outputs);

        let (_, _, logical_monitors, _) = async_io::block_on(config.get_current_state()).unwrap();

        assert!(logical_monitors.iter().all(|monitor| !monitor.is_primary));
    }

    #[test]
    fn monitor_state_uses_compositor_output_metadata() {
        let mut out = output("HDMI-A-1");
        out.make = "Dell".to_owned();
        out.model = "U2720Q".to_owned();
        out.serial = "ABC123".to_owned();
        out.x = 20;
        out.y = 30;
        out.scale = 1.25;
        out.transform = 2;
        let config = DisplayConfig::new(Arc::new(Mutex::new(vec![out])));

        let (_, monitors, logical_monitors, _) =
            async_io::block_on(config.get_current_state()).unwrap();

        assert_eq!(
            monitors[0].names,
            (
                "HDMI-A-1".to_owned(),
                "Dell".to_owned(),
                "U2720Q".to_owned(),
                "ABC123".to_owned()
            )
        );
        assert_eq!(monitors[0].modes.len(), 2);
        assert_eq!(monitors[0].modes[1].width, 1920);
        assert_eq!(monitors[0].modes[1].height, 1080);
        assert_eq!(monitors[0].modes[1].refresh_rate, 144.0);
        assert_eq!(monitors[0].modes[1].preferred_scale, 1.25);
        assert_eq!(
            monitors[0].properties["display-name"],
            OwnedValue::from(zvariant::Str::from("Dell U2720Q"))
        );
        assert_eq!(
            monitors[0].modes[1].properties["is-current"],
            OwnedValue::from(true)
        );
        assert_eq!((logical_monitors[0].x, logical_monitors[0].y), (20, 30));
        assert_eq!(logical_monitors[0].scale, 1.25);
        assert_eq!(logical_monitors[0].transform, 2);
    }

    #[test]
    fn display_name_falls_back_to_connector_for_unknown_metadata() {
        let mut out = output("HDMI-A-1");
        out.make = "Unknown".to_owned();
        out.model = "Unknown".to_owned();

        assert_eq!(make_display_name(&out, false), "HDMI-A-1");
    }

    #[test]
    fn supported_scales_follow_resolution_limits() {
        let scales: Vec<_> = supported_scales(Size::from((1920, 1080))).collect();

        assert_eq!(scales, vec![1.0, 1.25, 1.5, 1.75, 2.0, 2.25]);
    }
}
