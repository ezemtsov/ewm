//! Libinput device configuration from `ewm-input-config` and the touchpad gesture knobs.

use serde::Deserialize;
use smithay::reexports::input as libinput;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccelProfile {
    Flat,
    Adaptive,
}

impl From<AccelProfile> for libinput::AccelProfile {
    fn from(p: AccelProfile) -> Self {
        match p {
            AccelProfile::Flat => libinput::AccelProfile::Flat,
            AccelProfile::Adaptive => libinput::AccelProfile::Adaptive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClickMethod {
    ButtonAreas,
    Clickfinger,
}

impl From<ClickMethod> for libinput::ClickMethod {
    fn from(m: ClickMethod) -> Self {
        match m {
            ClickMethod::ButtonAreas => libinput::ClickMethod::ButtonAreas,
            ClickMethod::Clickfinger => libinput::ClickMethod::Clickfinger,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollMethod {
    NoScroll,
    TwoFinger,
    Edge,
    OnButtonDown,
}

impl From<ScrollMethod> for libinput::ScrollMethod {
    fn from(m: ScrollMethod) -> Self {
        match m {
            ScrollMethod::NoScroll => libinput::ScrollMethod::NoScroll,
            ScrollMethod::TwoFinger => libinput::ScrollMethod::TwoFinger,
            ScrollMethod::Edge => libinput::ScrollMethod::Edge,
            ScrollMethod::OnButtonDown => libinput::ScrollMethod::OnButtonDown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapButtonMap {
    LeftRightMiddle,
    LeftMiddleRight,
}

impl From<TapButtonMap> for libinput::TapButtonMap {
    fn from(m: TapButtonMap) -> Self {
        match m {
            TapButtonMap::LeftRightMiddle => libinput::TapButtonMap::LeftRightMiddle,
            TapButtonMap::LeftMiddleRight => libinput::TapButtonMap::LeftMiddleRight,
        }
    }
}

/// Input device type for configuration matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceType {
    Touchpad,
    Mouse,
    Trackball,
    Trackpoint,
    Keyboard,
    Tablet,
    Touch,
}

pub const DEFAULT_KEYBOARD_REPEAT_DELAY: i32 = 200;

pub const DEFAULT_KEYBOARD_REPEAT_RATE: i32 = 25;

/// A single input configuration entry, either a type default or device-specific.
///
/// Type defaults have `device: None` and `device_type: Some(...)`.
/// Device-specific overrides have `device: Some("name")` and optional `device_type`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct InputConfigEntry {
    /// None = type default, Some("device name") = device-specific override
    pub device: Option<String>,
    /// Required for type defaults, optional for device overrides (auto-detected)
    #[serde(rename = "type")]
    pub device_type: Option<DeviceType>,
    // All possible settings (superset of touchpad + mouse)
    #[serde(deserialize_with = "elisp::serde::present")]
    pub natural_scroll: Option<bool>,
    #[serde(deserialize_with = "elisp::serde::present")]
    pub tap: Option<bool>,
    #[serde(deserialize_with = "elisp::serde::present")]
    pub dwt: Option<bool>,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfile>,
    pub click_method: Option<ClickMethod>,
    pub scroll_method: Option<ScrollMethod>,
    #[serde(deserialize_with = "elisp::serde::present")]
    pub left_handed: Option<bool>,
    #[serde(deserialize_with = "elisp::serde::present")]
    pub middle_emulation: Option<bool>,
    pub tap_button_map: Option<TapButtonMap>,
    // Keyboard-specific settings
    pub repeat_delay: Option<i32>,
    pub repeat_rate: Option<i32>,
    pub xkb_layouts: Option<String>,
    pub xkb_variants: Option<String>,
    pub xkb_options: Option<String>,
    // Tablet and touch settings
    pub calibration_matrix: Option<[f32; 6]>,
    pub map_to_output: Option<String>,
    #[serde(deserialize_with = "elisp::serde::present")]
    pub map_to_focused_output: Option<bool>,
}

pub fn effective_keyboard_repeat(configs: &[InputConfigEntry]) -> (i32, i32) {
    let keyboard_config = configs
        .iter()
        .find(|c| c.device.is_none() && c.device_type == Some(DeviceType::Keyboard));
    let rate = keyboard_config
        .and_then(|config| config.repeat_rate)
        .unwrap_or(DEFAULT_KEYBOARD_REPEAT_RATE);
    let delay = keyboard_config
        .and_then(|config| config.repeat_delay)
        .unwrap_or(DEFAULT_KEYBOARD_REPEAT_DELAY);
    (rate, delay)
}

/// The gesture knobs, a plist from Lisp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Gestures {
    /// Fingers of the swipe that scrolls the strip, `None` for no such swipe.
    pub strip_swipe_fingers: Option<u32>,
    /// Fingers of the swipe that drives the overview, `None` for no such swipe.
    pub overview_swipe_fingers: Option<u32>,
}

impl Default for Gestures {
    fn default() -> Self {
        Self {
            strip_swipe_fingers: Some(3),
            overview_swipe_fingers: Some(3),
        }
    }
}

#[cfg(test)]
mod tests {
    use elisp::Value;

    use super::*;

    fn plist(pairs: &[(&str, Value)]) -> Value {
        let items = pairs
            .iter()
            .flat_map(|(key, value)| [Value::symbol(format!(":{key}")), value.clone()]);
        Value::List(items.collect())
    }

    fn text(s: &str) -> Value {
        Value::Str(s.into())
    }

    #[test]
    fn input_config_reads_lisp_plist() {
        let matrix = [1, 0, 0, 0, 1, 0].map(Value::Int).to_vec();
        let entry: InputConfigEntry = elisp::serde::from_value(&plist(&[
            ("type", text("touchpad")),
            ("tap", Value::NIL),
            ("accel-speed", Value::Float(-0.2)),
            ("accel-profile", text("flat")),
            ("click-method", Value::symbol("clickfinger")),
            ("scroll-method", text("two-finger")),
            ("tap-button-map", text("left-middle-right")),
            ("calibration-matrix", Value::List(matrix)),
        ]))
        .unwrap();
        assert_eq!(entry.device_type, Some(DeviceType::Touchpad));
        assert_eq!(entry.tap, Some(false));
        assert_eq!(entry.dwt, None);
        assert_eq!(entry.accel_speed, Some(-0.2));
        assert_eq!(entry.accel_profile, Some(AccelProfile::Flat));
        assert_eq!(entry.click_method, Some(ClickMethod::Clickfinger));
        assert_eq!(entry.scroll_method, Some(ScrollMethod::TwoFinger));
        assert_eq!(entry.tap_button_map, Some(TapButtonMap::LeftMiddleRight));
        assert_eq!(
            entry.calibration_matrix,
            Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0])
        );
    }

    #[test]
    fn input_config_rejects_unknown_values_and_keys() {
        let err = |pairs: &[(&str, Value)]| {
            elisp::serde::from_value::<InputConfigEntry>(&plist(pairs))
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            err(&[("accel-profile", text("linear"))]),
            "unknown variant `linear`, expected `flat` or `adaptive`"
        );
        assert!(
            err(&[("natrual-scroll", Value::symbol("t"))])
                .starts_with("unknown field `natrual-scroll`")
        );
        assert!(
            err(&[("calibration-matrix", Value::List(vec![Value::Int(1)]))])
                .contains("invalid length 1")
        );
    }

    #[test]
    fn keyboard_repeat_config_uses_one_effective_source() {
        assert_eq!(
            effective_keyboard_repeat(&[]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let xkb_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            xkb_layouts: Some("us,ru".to_string()),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[xkb_only]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let rate_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            repeat_rate: Some(17),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[rate_only]),
            (17, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let delay_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            repeat_delay: Some(450),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[delay_only]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, 450),
        );
    }

    #[test]
    fn gestures_read_the_lisp_plist() {
        let gestures: Gestures = elisp::serde::from_value(&plist(&[
            ("strip-swipe-fingers", Value::Int(4)),
            ("overview-swipe-fingers", Value::NIL),
        ]))
        .unwrap();
        assert_eq!(gestures.strip_swipe_fingers, Some(4));
        assert_eq!(gestures.overview_swipe_fingers, None);
        let defaults: Gestures = elisp::serde::from_value(&Value::NIL).unwrap();
        assert_eq!(defaults, Gestures::default());
    }
}
