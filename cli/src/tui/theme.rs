use ratatui::style::Color;

pub const INK: Color = Color::Rgb(0x17, 0x19, 0x1F); // canvas
pub const TERMINAL: Color = Color::Rgb(0x1B, 0x1E, 0x27); // panel
pub const RECESSED_WELL: Color = Color::Rgb(0x14, 0x16, 0x1C); // welcome/input bg
pub const ROW_SELECT: Color = Color::Rgb(0x26, 0x2A, 0x36); // selected row fill
pub const TEXT: Color = Color::Rgb(0xEC, 0xEF, 0xF3); // primary
pub const BODY: Color = Color::Rgb(0xC2, 0xC7, 0xD2); // secondary
pub const MUTED: Color = Color::Rgb(0x86, 0x8C, 0x9B); // meta
pub const FAINT: Color = Color::Rgb(0x4A, 0x4F, 0x5C); // read dot
pub const SIGNAL: Color = Color::Rgb(0x1C, 0x5F, 0xD8); // brand blue
pub const SIGNAL_LIGHT: Color = Color::Rgb(0x5B, 0x8D, 0xEF); // unread/prompt/active on dark
pub const CYAN: Color = Color::Rgb(0x4F, 0xE8, 0xF5);
pub const TEAL: Color = Color::Rgb(0x49, 0xC7, 0xD6); // @ glyph
pub const GREEN: Color = Color::Rgb(0x5F, 0xCB, 0x87); // ✓
pub const AMBER: Color = Color::Rgb(0xE0, 0xA8, 0x4E); // ★ ⚠
pub const RED: Color = Color::Rgb(0xF2, 0x76, 0x6B); // ! ✕
pub const VIOLET: Color = Color::Rgb(0x9A, 0x8B, 0xF5); // ✦ ◆
pub const HAIRLINE: Color = Color::Rgb(0x2A, 0x2D, 0x34); // borders

// Glyphs
pub const G_UNREAD: char = '●';
pub const G_READ: char = '○';
pub const G_STARRED: char = '★';
pub const G_URGENT: char = '!';
pub const G_ATTACHMENT: char = '@';
pub const G_LABEL: char = '◆';
pub const G_SUCCESS: char = '✓';
pub const G_WARNING: char = '⚠';
pub const G_ERROR: char = '✕';
pub const G_PROMPT: char = '›';
pub const G_AI: char = '✦';
pub const G_SELECTED: char = '❯';
pub const G_TREE: char = '└';
pub const G_TREE_CONT: char = '⎿';
