//! org.gnome.Mutter.DisplayConfig D-Bus interface implementation
//!
//! Based on niri's `dbus/mutter_display_config.rs`. This interface is used
//! by xdg-desktop-portal-gnome to enumerate monitors.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tracing::info;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Type, Value};
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

        for (idx, output) in outputs.iter().enumerate() {
            let connector = output.name.clone();
            let vendor = "Unknown".to_string();
            let product = "Unknown".to_string();
            let serial = format!("{}", idx);

            // Create mode from output info
            let refresh_rate = output.refresh as f64 / 1000.0;
            let mode_id = format!("{}x{}@{:.3}", output.width, output.height, refresh_rate);

            let mode = Mode {
                id: mode_id,
                width: output.width,
                height: output.height,
                refresh_rate,
                preferred_scale: 1.0,
                supported_scales: vec![1.0, 1.25, 1.5, 2.0],
                properties: HashMap::from([
                    (
                        "is-current".to_string(),
                        OwnedValue::try_from(Value::Bool(true))
                            .expect("bool conversion is infallible"),
                    ),
                    (
                        "is-preferred".to_string(),
                        OwnedValue::try_from(Value::Bool(true))
                            .expect("bool conversion is infallible"),
                    ),
                ]),
            };

            let names = (connector.clone(), vendor, product, serial);

            // Display name property
            let mut properties = HashMap::new();
            properties.insert(
                "display-name".to_string(),
                OwnedValue::try_from(Value::Str(connector.clone().into()))
                    .expect("string conversion is infallible"),
            );
            properties.insert(
                "is-builtin".to_string(),
                OwnedValue::try_from(Value::Bool(false)).expect("bool conversion is infallible"),
            );

            monitors.push(Monitor {
                names: names.clone(),
                modes: vec![mode],
                properties,
            });

            // Create logical monitor with actual position from compositor
            logical_monitors.push(LogicalMonitor {
                x: output.x,
                y: output.y,
                scale: 1.0,
                transform: 0, // Normal
                is_primary: idx == 0,
                monitors: vec![names],
                properties: HashMap::new(),
            });
        }

        // Sort by connector name
        monitors.sort_by(|a, b| a.names.0.cmp(&b.names.0));
        logical_monitors.sort_by(|a, b| a.monitors[0].0.cmp(&b.monitors[0].0));

        let properties = HashMap::from([(
            "layout-mode".to_string(),
            OwnedValue::try_from(Value::U32(1)).expect("u32 conversion is infallible"),
        )]);

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
        true
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
