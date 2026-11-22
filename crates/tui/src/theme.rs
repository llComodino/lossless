use ratatui::style::{Color, Style};

pub const BG: Color = Color::Rgb(0x18, 0x18, 0x18);
pub const FG: Color = Color::Rgb(0xe4, 0xe4, 0xef);
pub const YELLOW: Color = Color::Rgb(0xff, 0xdd, 0x33);
pub const GREEN: Color = Color::Rgb(0x73, 0xc9, 0x36);
pub const GREY: Color = Color::Rgb(0x52, 0x49, 0x4e);
pub const QUARTZ: Color = Color::Rgb(0x95, 0xa9, 0x9f);
pub const NIAGARA: Color = Color::Rgb(0x96, 0xa6, 0xc8);
pub const RED: Color = Color::Rgb(0xf4, 0x38, 0x41);
pub const SELECTED_BG: Color = Color::Rgb(0x45, 0x3d, 0x41);

pub fn base() -> Style {
    Style::new().fg(FG).bg(BG)
}

pub fn selected() -> Style {
    Style::new().fg(YELLOW).bg(SELECTED_BG)
}

pub fn playing() -> Style {
    Style::new().fg(GREEN)
}

pub fn dim() -> Style {
    Style::new().fg(QUARTZ)
}

pub fn border() -> Style {
    Style::new().fg(GREY)
}

pub fn accent() -> Style {
    Style::new().fg(NIAGARA)
}
