use std::ops::{Mul, MulAssign};

/// RGB color in [0, 1] with unpremultiplied alpha.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn new_unpremul(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn from_rgba8_unpremul(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::from_array_unpremul([r, g, b, a].map(|x| x as f32 / 255.))
    }

    pub fn from_array_premul([r, g, b, a]: [f32; 4]) -> Self {
        let a = a.clamp(0., 1.);

        if a == 0. {
            Self::new_unpremul(0., 0., 0., 0.)
        } else {
            Self {
                r: (r / a).clamp(0., 1.),
                g: (g / a).clamp(0., 1.),
                b: (b / a).clamp(0., 1.),
                a,
            }
        }
    }

    pub const fn from_array_unpremul([r, g, b, a]: [f32; 4]) -> Self {
        Self { r, g, b, a }
    }

    pub fn to_array_unpremul(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    pub fn to_array_premul(self) -> [f32; 4] {
        let [r, g, b, a] = [self.r, self.g, self.b, self.a];
        [r * a, g * a, b * a, a]
    }
}

impl Mul<f32> for Color {
    type Output = Self;

    fn mul(mut self, rhs: f32) -> Self::Output {
        self.a *= rhs;
        self
    }
}

impl MulAssign<f32> for Color {
    fn mul_assign(&mut self, rhs: f32) {
        self.a *= rhs;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CornerRadius {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl From<CornerRadius> for [f32; 4] {
    fn from(value: CornerRadius) -> Self {
        [
            value.top_left,
            value.top_right,
            value.bottom_right,
            value.bottom_left,
        ]
    }
}

impl From<f32> for CornerRadius {
    fn from(value: f32) -> Self {
        Self {
            top_left: value,
            top_right: value,
            bottom_right: value,
            bottom_left: value,
        }
    }
}

impl CornerRadius {
    pub fn fit_to(self, width: f32, height: f32) -> Self {
        // Like in CSS: https://drafts.csswg.org/css-backgrounds/#corner-overlap
        let reduction = f32::min(
            f32::min(
                width / (self.top_left + self.top_right),
                width / (self.bottom_left + self.bottom_right),
            ),
            f32::min(
                height / (self.top_left + self.bottom_left),
                height / (self.top_right + self.bottom_right),
            ),
        );
        let reduction = f32::min(1., reduction);

        Self {
            top_left: self.top_left * reduction,
            top_right: self.top_right * reduction,
            bottom_right: self.bottom_right * reduction,
            bottom_left: self.bottom_left * reduction,
        }
    }

    pub fn expanded_by(mut self, width: f32) -> Self {
        // Radius = 0 is preserved, so that square corners remain square.
        if self.top_left > 0. {
            self.top_left += width;
        }
        if self.top_right > 0. {
            self.top_right += width;
        }
        if self.bottom_right > 0. {
            self.bottom_right += width;
        }
        if self.bottom_left > 0. {
            self.bottom_left += width;
        }

        if width < 0. {
            self.top_left = self.top_left.max(0.);
            self.top_right = self.top_right.max(0.);
            self.bottom_left = self.bottom_left.max(0.);
            self.bottom_right = self.bottom_right.max(0.);
        }

        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub on: bool,
    pub offset: ShadowOffset,
    pub softness: f64,
    pub spread: f64,
    pub draw_behind_window: bool,
    pub color: Color,
    pub inactive_color: Option<Color>,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            on: false,
            offset: ShadowOffset {
                x: FloatOrInt(0.),
                y: FloatOrInt(5.),
            },
            softness: 30.,
            spread: 5.,
            draw_behind_window: false,
            color: Color::from_rgba8_unpremul(0, 0, 0, 0x77),
            inactive_color: None,
        }
    }
}

impl Shadow {
    pub fn floating_default() -> Self {
        Self {
            on: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowOffset {
    pub x: FloatOrInt,
    pub y: FloatOrInt,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatOrInt(pub f64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blur {
    pub off: bool,
    pub passes: u8,
    pub offset: f64,
    pub noise: f64,
    pub saturation: f64,
}

impl Default for Blur {
    fn default() -> Self {
        Self {
            off: false,
            passes: 3,
            offset: 3.,
            noise: 0.02,
            saturation: 1.5,
        }
    }
}

/// Resolved background effect rule.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct BackgroundEffect {
    /// Whether to blur the background.
    ///
    /// - `None`: blur when the window/layer requests it (e.g. through ext-background-effect
    ///   protocol)
    /// - `Some(false)`: never blur
    /// - `Some(true)`: always blur
    pub blur: Option<bool>,

    pub noise: Option<f64>,
    pub saturation: Option<f64>,
}
