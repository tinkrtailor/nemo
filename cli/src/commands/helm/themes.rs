use std::str::FromStr;

use ratatui::style::Color;

/// Built-in color theme (FR-8).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub teal: Color,
    pub amber: Color,
    pub green: Color,
    pub red: Color,
    pub blue: Color,
}

/// Theme selector (FR-8a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
    HighContrast,
}

impl ThemeName {
    /// Cycle to next theme (FR-8c).
    pub fn cycle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::HighContrast,
            Self::HighContrast => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::HighContrast => "high-contrast",
        }
    }

    pub fn theme(self) -> Theme {
        match self {
            Self::Dark => DARK_THEME,
            Self::Light => LIGHT_THEME,
            Self::HighContrast => HIGH_CONTRAST_THEME,
        }
    }
}

impl FromStr for ThemeName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            "high-contrast" | "high_contrast" => Ok(Self::HighContrast),
            _ => Err(format!("unknown theme: {s}")),
        }
    }
}

/// Dark theme (current default, matches DESIGN.md).
pub const DARK_THEME: Theme = Theme {
    bg: Color::Rgb(15, 15, 14),
    surface: Color::Rgb(26, 25, 24),
    border: Color::Rgb(46, 45, 43),
    text: Color::Rgb(232, 230, 227),
    muted: Color::Rgb(138, 135, 132),
    teal: Color::Rgb(27, 107, 90),
    amber: Color::Rgb(232, 168, 56),
    green: Color::Rgb(45, 122, 79),
    red: Color::Rgb(196, 57, 45),
    blue: Color::Rgb(59, 123, 192),
};

/// Light theme.
pub const LIGHT_THEME: Theme = Theme {
    bg: Color::Rgb(250, 250, 248),
    surface: Color::Rgb(240, 239, 237),
    border: Color::Rgb(210, 208, 205),
    text: Color::Rgb(30, 30, 28),
    muted: Color::Rgb(110, 108, 105),
    teal: Color::Rgb(20, 90, 75),
    amber: Color::Rgb(180, 120, 20),
    green: Color::Rgb(30, 100, 60),
    red: Color::Rgb(180, 40, 30),
    blue: Color::Rgb(40, 100, 170),
};

/// High-contrast theme for accessibility.
pub const HIGH_CONTRAST_THEME: Theme = Theme {
    bg: Color::Rgb(0, 0, 0),
    surface: Color::Rgb(15, 15, 15),
    border: Color::Rgb(80, 80, 80),
    text: Color::Rgb(255, 255, 255),
    muted: Color::Rgb(180, 180, 180),
    teal: Color::Rgb(0, 255, 200),
    amber: Color::Rgb(255, 200, 0),
    green: Color::Rgb(0, 255, 0),
    red: Color::Rgb(255, 50, 50),
    blue: Color::Rgb(80, 180, 255),
};
