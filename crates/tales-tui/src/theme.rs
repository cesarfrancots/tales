//! Shared palette + name/color helpers, used by every screen so the connect,
//! prompt, and live-chat views look like one tool.

use ratatui::style::Color;
use tales_core::agent::tool_info;

pub const TEXT: Color = Color::Rgb(0xd2, 0xd8, 0xe2);
pub const DIM: Color = Color::Rgb(0x6b, 0x74, 0x83);
pub const FAINT: Color = Color::Rgb(0x44, 0x4d, 0x5a);
pub const ACCENT: Color = Color::Rgb(0x2d, 0xd4, 0xbf);
pub const CLAUDE: Color = Color::Rgb(0x5c, 0xb0, 0xff);
pub const CODEX: Color = Color::Rgb(0xc0, 0x8c, 0xff);
pub const OPENCODE: Color = Color::Rgb(0xff, 0xc7, 0x77);
pub const YOU: Color = Color::Rgb(0x7e, 0xe0, 0xa3);
pub const OK: Color = YOU;
pub const ERRC: Color = Color::Rgb(0xff, 0x7a, 0x85);

/// Human-facing name for a tool key (`claude` → `Claude Code`, `opencode` →
/// `Open Code`). Falls back to capitalizing anything not in the registry.
pub fn pretty(label: &str) -> String {
    if let Some(t) = tool_info(label) {
        return t.pretty.to_string();
    }
    let mut c = label.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => label.to_string(),
    }
}

/// The accent color a tool's blocks/badges render in.
pub fn color_for(label: &str) -> Color {
    match label.to_lowercase().as_str() {
        "claude" => CLAUDE,
        "codex" => CODEX,
        "opencode" => OPENCODE,
        _ => OPENCODE,
    }
}
