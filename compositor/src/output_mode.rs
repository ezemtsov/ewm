//! Output mode configuration types, ported from niri's `niri-config`/`niri-ipc`.
//!
//! `Mode` selects an advertised or CVT-generated custom mode; `Modeline` is an
//! explicit DRM modeline. The timing calculation lives in `backend/drm.rs`.

use std::str::FromStr;

use serde::{Deserialize, Deserializer, de};

/// A requested video mode: width/height with optional refresh in Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfiguredMode {
    pub width: u16,
    pub height: u16,
    pub refresh: Option<f64>,
}

/// A configured mode. When `custom` is set, the mode is CVT-generated rather than
/// matched against the connector's advertised modes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    pub custom: bool,
    pub mode: ConfiguredMode,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HSyncPolarity {
    PHSync,
    NHSync,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VSyncPolarity {
    PVSync,
    NVSync,
}

/// An explicit DRM modeline, mirroring the X11 modeline fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Modeline {
    /// The rate at which pixels are drawn in MHz.
    pub clock: f64,
    /// Horizontal active pixels.
    pub hdisplay: u16,
    /// Horizontal sync pulse start position in pixels.
    pub hsync_start: u16,
    /// Horizontal sync pulse end position in pixels.
    pub hsync_end: u16,
    /// Total horizontal pixels per line.
    pub htotal: u16,
    /// Vertical active pixels.
    pub vdisplay: u16,
    /// Vertical sync pulse start position in pixels.
    pub vsync_start: u16,
    /// Vertical sync pulse end position in pixels.
    pub vsync_end: u16,
    /// Total vertical pixels per frame.
    pub vtotal: u16,
    /// Horizontal sync polarity: "+hsync" or "-hsync".
    pub hsync_polarity: HSyncPolarity,
    /// Vertical sync polarity: "+vsync" or "-vsync".
    pub vsync_polarity: VSyncPolarity,
}

impl FromStr for HSyncPolarity {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "+hsync" => Ok(Self::PHSync),
            "-hsync" => Ok(Self::NHSync),
            _ => Err(r#"invalid horizontal sync polarity, can be "+hsync" or "-hsync""#),
        }
    }
}

impl FromStr for VSyncPolarity {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "+vsync" => Ok(Self::PVSync),
            "-vsync" => Ok(Self::NVSync),
            _ => Err(r#"invalid vertical sync polarity, can be "+vsync" or "-vsync""#),
        }
    }
}

/// Reads the X11 modeline string.
impl<'de> Deserialize<'de> for Modeline {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(de::Error::custom)
    }
}

impl FromStr for Modeline {
    type Err = String;

    /// Parse the X11 modeline form:
    /// `clock hdisplay hsync_start hsync_end htotal vdisplay vsync_start vsync_end vtotal +hsync +vsync`
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        fn u16_field(v: Option<&str>, what: &str) -> Result<u16, String> {
            v.ok_or_else(|| format!("modeline: missing {what}"))?
                .parse()
                .map_err(|_| format!("modeline: invalid {what}"))
        }

        let mut f = s.split_whitespace();
        let clock = f
            .next()
            .ok_or_else(|| "modeline: missing clock".to_string())?
            .parse::<f64>()
            .map_err(|_| "modeline: invalid clock".to_string())?;
        let hdisplay = u16_field(f.next(), "hdisplay")?;
        let hsync_start = u16_field(f.next(), "hsync_start")?;
        let hsync_end = u16_field(f.next(), "hsync_end")?;
        let htotal = u16_field(f.next(), "htotal")?;
        let vdisplay = u16_field(f.next(), "vdisplay")?;
        let vsync_start = u16_field(f.next(), "vsync_start")?;
        let vsync_end = u16_field(f.next(), "vsync_end")?;
        let vtotal = u16_field(f.next(), "vtotal")?;
        let hsync_polarity = f
            .next()
            .ok_or_else(|| "modeline: missing hsync polarity".to_string())?
            .parse::<HSyncPolarity>()
            .map_err(|e| e.to_string())?;
        let vsync_polarity = f
            .next()
            .ok_or_else(|| "modeline: missing vsync polarity".to_string())?
            .parse::<VSyncPolarity>()
            .map_err(|e| e.to_string())?;
        if f.next().is_some() {
            return Err("modeline: too many fields".to_string());
        }

        Ok(Modeline {
            clock,
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            hsync_polarity,
            vsync_polarity,
        })
    }
}
