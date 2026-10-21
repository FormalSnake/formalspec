use ratatui::style::{Color, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub truecolor: bool,
    pub bg: Color,
    pub surface: Color,
    pub sel: Color,
    pub line: Color,
    pub ink: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub amber: Color,
    pub red: Color,
    pub green: Color,
    pub on_accent: Color,
    pub on_warm: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    pub fn truecolor() -> Self {
        Theme {
            truecolor: true,
            bg: rgb(0x161616),
            surface: rgb(0x1E1E1E),
            sel: rgb(0x23272B),
            line: rgb(0x2C2C2C),
            ink: rgb(0xE4E4E4),
            muted: rgb(0x8C8C8C),
            faint: rgb(0x5A5A5A),
            accent: rgb(0x4A9EDA),
            amber: rgb(0xD3A35F),
            red: rgb(0xE5534B),
            green: rgb(0x7FB685),
            on_accent: rgb(0x161616),
            on_warm: rgb(0x161616),
        }
    }

    pub fn basic() -> Self {
        Theme {
            truecolor: false,
            bg: Color::Reset,
            surface: Color::Reset,
            sel: Color::Reset,
            line: Color::DarkGray,
            ink: Color::White,
            muted: Color::Gray,
            faint: Color::DarkGray,
            accent: Color::Blue,
            amber: Color::Yellow,
            red: Color::Red,
            green: Color::Green,
            on_accent: Color::White,
            on_warm: Color::Black,
        }
    }

    pub fn detect() -> Self {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let linux = std::env::var("TERM").is_ok_and(|t| t == "linux");
        if (colorterm.contains("truecolor") || colorterm.contains("24bit")) && !linux {
            Self::truecolor()
        } else {
            Self::basic()
        }
    }

    pub fn base(&self) -> Style {
        Style::new().fg(self.ink).bg(self.bg)
    }
}

pub fn fg(c: Color) -> Style {
    Style::new().fg(c)
}
