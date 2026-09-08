//! The one place colours are defined.
//!
//! Every styled span in the TUI names a *role* from this module — `theme::PROSE`,
//! `theme::TOOL_NAME` — never a hue. Hues live only in the palette below, so the
//! scheme can be re-tuned in one file and the screenshot harness renders exactly
//! what ships (see [`crate::tui::screenshot`], which reads these same constants).
//!
//! Two accent tiers, deliberately: [`ACC`] (copper) is rare and high-salience —
//! the user's caret, focus, selection, the brand — while [`INFO`] (teal) carries
//! the frequent structural work: tool names, headings, inline code. One accent
//! doing both jobs is what left the old scheme flat.

use ratatui::style::{Color, Modifier, Style};

/// Page background. The app owns its rectangle rather than inheriting whatever
/// the terminal profile sets, so the warm surface ramp reads as intended.
pub const BG0: Color = Color::Rgb(0x10, 0x0e, 0x0b);
/// Seated surface: the composer band, one step up from the page.
pub const BG1: Color = Color::Rgb(0x17, 0x14, 0x0f);
/// Raised surface: fenced code blocks.
pub const BG2: Color = Color::Rgb(0x21, 0x1d, 0x16);
/// Selected-row wash in the popups.
pub const SEL: Color = Color::Rgb(0x2c, 0x26, 0x1c);
/// Rules, separators, unfocused borders.
pub const BORDER: Color = Color::Rgb(0x2b, 0x26, 0x1f);

/// Prose.
pub const FG: Color = Color::Rgb(0xe6, 0xde, 0xd1);
/// Secondary text that still needs to be read: tool detail, unselected rows.
pub const DIM: Color = Color::Rgb(0x9a, 0x90, 0x82);
/// Metadata that should recede until looked for: gutters, footer, placeholders.
pub const FAINT: Color = Color::Rgb(0x6b, 0x62, 0x59);

/// The brand accent. Rare by design.
pub const ACC: Color = Color::Rgb(0xd9, 0x8b, 0x4a);
pub const WARN: Color = Color::Rgb(0xe3, 0xbb, 0x52);
pub const ERR: Color = Color::Rgb(0xe0, 0x65, 0x5a);
/// The structural accent: frequent, calmer than [`ACC`].
pub const INFO: Color = Color::Rgb(0x5f, 0xb3, 0xb8);

/// Error-callout wash — [`ERR`] pulled down into the surface ramp so a failure
/// reads as a band, not just a coloured glyph.
pub const ERR_BG: Color = Color::Rgb(0x2e, 0x1a, 0x17);

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

pub const PROSE: Style = fg(FG);
/// The user's own words — bold, so scanning back through a transcript lands on
/// the prompts.
pub const USER: Style = bold(fg(FG));
pub const USER_CARET: Style = bold(fg(ACC));

/// Text that still carries meaning but shouldn't compete.
pub const SUBTLE: Style = fg(DIM);
/// Chrome and metadata: gutters, footer, hints, placeholders.
pub const META: Style = fg(FAINT);

pub const TOOL_GROUP: Style = bold(fg(DIM));
pub const TOOL_NAME: Style = bold(fg(INFO));
pub const TOOL_GUTTER: Style = fg(FAINT);

pub const HEADING: Style = bold(fg(INFO));
pub const INLINE_CODE: Style = fg(INFO);
pub const CODE: Style = Style::new().bg(BG2).fg(FG);
pub const CODE_GUTTER: Style = Style::new().bg(BG2).fg(FAINT);
pub const CODE_LANG: Style = Style::new().bg(BG2).fg(FAINT);
pub const QUOTE_GUTTER: Style = fg(FAINT);

/// A horizontal rule at rest, and the same rule when the composer has focus.
pub const RULE: Style = fg(BORDER);
pub const RULE_FOCUS: Style = fg(ACC);

/// An inline render-preview link in a tool row — underlined teal so it reads
/// as clickable (a click opens the PNG in the system viewer).
pub const LINK: Style = fg(INFO).add_modifier(Modifier::UNDERLINED);

pub const POPUP_TITLE: Style = bold(fg(ACC));

/// The floating menus (`/command` completion, the unwind picker) are borderless
/// lists seated on [`SEL`], one surface step above the composer band they cover.
/// The selected row is a solid accent bar — legible from anywhere along the row,
/// where a caret is only legible at its start.
pub const MENU: Style = Style::new().bg(SEL).fg(FG);
pub const MENU_LABEL: Style = bold(Style::new().bg(SEL).fg(FG));
pub const MENU_DETAIL: Style = Style::new().bg(SEL).fg(DIM);
pub const MENU_SEL: Style = Style::new().bg(ACC).fg(BG0);
pub const MENU_SEL_LABEL: Style = bold(Style::new().bg(ACC).fg(BG0));

pub const DANGER: Style = fg(ERR);
pub const WARNING: Style = fg(WARN);

pub const SPINNER: Style = bold(fg(ACC));
pub const LOGO: Style = bold(fg(ACC));
