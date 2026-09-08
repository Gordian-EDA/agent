//! The one place colours are defined.
//!
//! Every styled span in the TUI names a *role* — `theme::PROSE`,
//! `theme::TOOL_NAME` — never a hue. Hues live only in the palette below, so the
//! scheme can be re-tuned in one file and [`super::shot`] captures exactly what
//! ships.
//!
//! Two accent tiers, deliberately: [`ACC`] (copper) is rare and high-salience —
//! the caret, focus, the brand — while [`INFO`] (teal) carries the frequent
//! structural work: tool names, headings, links.

use ratatui::style::{Color, Modifier, Style};

/// Page background. The app owns its rectangle rather than inheriting whatever
/// the terminal profile sets, so the warm surface ramp reads as intended.
pub const BG0: Color = Color::Rgb(0x10, 0x0e, 0x0b);
/// Seated surface: the composer band, one step up from the page.
pub const BG1: Color = Color::Rgb(0x17, 0x14, 0x0f);
/// Raised surface: the help overlay.
pub const BG2: Color = Color::Rgb(0x21, 0x1d, 0x16);
/// Rules, separators, unfocused borders.
pub const BORDER: Color = Color::Rgb(0x2b, 0x26, 0x1f);

/// Prose.
pub const FG: Color = Color::Rgb(0xe6, 0xde, 0xd1);
/// Secondary text that still needs to be read: tool detail.
pub const DIM: Color = Color::Rgb(0x9a, 0x90, 0x82);
/// Metadata that should recede until looked for: gutters, footer, placeholders.
pub const FAINT: Color = Color::Rgb(0x6b, 0x62, 0x59);

/// The brand accent. Rare by design.
pub const ACC: Color = Color::Rgb(0xd9, 0x8b, 0x4a);
pub const WARN: Color = Color::Rgb(0xe3, 0xbb, 0x52);
pub const ERR: Color = Color::Rgb(0xe0, 0x65, 0x5a);
/// The structural accent: frequent, calmer than [`ACC`].
pub const INFO: Color = Color::Rgb(0x5f, 0xb3, 0xb8);

const fn fg(c: Color) -> Style {
    Style::new().fg(c)
}
const fn bold(s: Style) -> Style {
    s.add_modifier(Modifier::BOLD)
}

/// The whole-frame wash laid down before anything else draws.
pub const PAGE: Style = Style::new().bg(BG0).fg(FG);
/// The composer band, seated above the page.
pub const BAND: Style = Style::new().bg(BG1);
/// The help overlay, raised above both.
pub const OVERLAY: Style = Style::new().bg(BG2).fg(FG);

pub const PROSE: Style = fg(FG);
/// The user's own words — bold, so scanning back lands on the prompts.
pub const USER: Style = bold(fg(FG));
pub const CARET: Style = bold(fg(ACC));

/// Text that still carries meaning but shouldn't compete.
pub const SUBTLE: Style = fg(DIM);
/// Chrome and metadata: gutters, footer, hints, placeholders.
pub const META: Style = fg(FAINT);

pub const TOOL_NAME: Style = bold(fg(INFO));
pub const HEADING: Style = bold(fg(INFO));

/// A horizontal rule at rest, and the same rule when a turn is running.
pub const RULE: Style = fg(BORDER);
pub const RULE_FOCUS: Style = fg(ACC);

/// The caption under an inline render — underlined teal, because a click on it
/// opens the PNG in the system viewer.
pub const LINK: Style = fg(INFO).add_modifier(Modifier::UNDERLINED);

pub const DANGER: Style = fg(ERR);
pub const WARNING: Style = fg(WARN);
pub const SPINNER: Style = bold(fg(ACC));
pub const LOGO: Style = bold(fg(ACC));
