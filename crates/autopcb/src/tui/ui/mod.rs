//! Rendering: draw an [`App`] onto a ratatui [`Frame`]. No I/O; the single
//! mutation is clamping `app.scroll` to the viewport (only the renderer knows
//! the wrapped line count).
//!
//! Layout (spec §11):
//!
//! ```text
//! ┌─ auto-pcb ── design.kicad_sch ──────── [KiCAD ●] [auto OFF] ─┐
//! │ chat transcript (scrollable; tool cards collapsed)           │
//! ├──────────────────────────────────────────────────────────────┤
//! │ ◆ PROPOSED CHANGES  +U1 +R7  ~C2   nets 3→12   [a]pprove [r]…  │  (only when a diff is pending)
//! ├──────────────────────────────────────────────────────────────┤
//! │ > input…                                                      │
//! ├──────────────────────────────────────────────────────────────┤
//! │ bedrock · opus · turns 2 · ctx 23.4k (12%) ····· /help /undo  │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! Split by pane: [`transcript`] draws the scrollable chat (and owns the styled
//! word-wrap that keeps the scroll math exact); [`composer`] draws the input box
//! plus the floating popups and the apply-gate card that sit just above it;
//! [`chrome`] draws the frame furniture (header, status footer, running
//! indicator, help overlay). The transcript is wrapped by
//! [`transcript::wrap_segments`] (not `Paragraph::wrap`) so the scroll arithmetic
//! — tail-following, clamping, the `↑n` indicator — is exact in visual rows.

mod chrome;
mod composer;
mod transcript;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui_image::picker::Picker;

use super::app::App;

/// Symmetric horizontal margin (in columns) applied to every pane via [`body`],
/// so the header, transcript, composer, diff card, and footer all share one left
/// edge and none of them hugs the terminal edge (the Codex layout discipline).
pub(super) const MARGIN: u16 = 2;

/// Everything the renderer needs that the testable [`App`] deliberately does not
/// hold: the image [`Picker`] (terminal graphics capability + cell font size).
///
/// When `picker` is `None` — under the screenshot harness or a dumb terminal —
/// inline images fall back to a stable text label, so SVG snapshots stay
/// byte-stable and a graphics-less terminal never emits escape garbage.
pub struct RenderCtx<'a> {
    pub picker: Option<&'a Picker>,
}

impl RenderCtx<'_> {
    /// A text-only context: images render as their label, never as graphics. Used
    /// by the screenshot harness and the unit tests for byte-stable output.
    #[cfg(test)]
    pub fn text_only() -> Self {
        Self { picker: None }
    }
}

/// Draw the whole cockpit with no image-rendering capability (text-label
/// previews). The screenshot harness and the unit tests use this.
#[cfg(test)]
pub fn draw(f: &mut Frame, app: &mut App) {
    draw_with(f, app, &mut RenderCtx::text_only());
}

/// Draw the whole cockpit, rendering inline image previews through `ctx`'s
/// [`Picker`] when one is present.
pub fn draw_with(f: &mut Frame, app: &mut App, ctx: &mut RenderCtx) {
    let area = f.area();

    // Size the diff pane to its content (0 when nothing is pending).
    let diff_h = app
        .pending
        .as_ref()
        .map(|d| composer::diff_height(d, area.width))
        .unwrap_or(0);
    // The running indicator takes a row while a turn is in flight, plus a second
    // detail row when a named tool is currently executing.
    let running_h = if app.running {
        1 + u16::from(app.active_tool.is_some())
    } else {
        0
    };
    // The composer grows with a multi-line draft (capped), so a pasted or
    // Shift-Enter'd prompt stays visible instead of scrolling under the border.
    let input_h = composer::composer_height(app, area.height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // header
            Constraint::Min(3),            // transcript
            Constraint::Length(diff_h),    // proposed-changes pane
            Constraint::Length(running_h), // running indicator
            Constraint::Length(input_h),   // input composer (rounded box)
            Constraint::Length(1),         // status bar
        ])
        .split(area);

    chrome::draw_header(f, chunks[0], app);
    transcript::draw_transcript(f, chunks[1], app, ctx);
    if app.pending.is_some() {
        composer::draw_diff(f, chunks[2], app);
    }
    if app.running {
        chrome::draw_running(f, chunks[3], app);
    }
    composer::draw_input(f, chunks[4], app);
    chrome::draw_status(f, chunks[5], app);
    composer::draw_completions(f, chunks[4], app);
    composer::draw_unwind(f, chunks[4], app);

    if app.help {
        chrome::draw_help(f, area);
    }
}

/// Inset a pane by [`MARGIN`] columns on *both* sides (full height), so content
/// shares one left edge and never touches either terminal edge.
pub(super) fn body(area: Rect) -> Rect {
    Rect {
        x: area.x + MARGIN,
        y: area.y,
        width: area.width.saturating_sub(2 * MARGIN),
        height: area.height,
    }
}

/// Compact token count: `950`, `23.4k`, `1.2M`.
pub(super) fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Msg, Status};
    use gordian_core::AgentEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    /// Render an app to a TestBackend and return the buffer's text as one string.
    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buffer_text(&buf)
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app() -> App {
        App::new(Status::new(
            "bedrock",
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "/tmp/proj/design.kicad_sch",
            true,
        ))
    }

    #[test]
    fn assistant_markdown_renders_styled_not_literal() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "I added **R1** with `10k`:\n- pull-up\n```yaml\nnets:\n```".into(),
        )));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("R1"), "bold text shows:\n{text}");
        assert!(!text.contains("**"), "bold markers stripped:\n{text}");
        assert!(!text.contains('`'), "code markers stripped:\n{text}");
        assert!(text.contains("• pull-up"), "bullet normalized:\n{text}");
        assert!(text.contains("nets:"), "fence content shows:\n{text}");
        // The ``` fence markers are gone; the language surfaces as a dim label on
        // the code block's opening row, not as a literal fence line.
        assert!(!text.contains("```"), "fence markers stripped:\n{text}");
        assert!(text.contains("yaml"), "fence language shows as a label:\n{text}");
    }

    #[test]
    fn user_text_is_never_markdown_rendered() {
        let mut a = app();
        for c in "literally **stars**".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let text = render_to_string(&mut a, 80, 24);
        assert!(
            text.contains("**stars**"),
            "user input stays literal:\n{text}"
        );
    }

    #[test]
    fn transcript_renders_user_message_and_tool_card() {
        let mut a = app();
        for c in "design a board".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "search_symbols".into(),
        }));
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "search_symbols".into(),
            summary: "\"STM32\" → 4 hits".into(),
            image_path: None,
        }));

        let text = render_to_string(&mut a, 80, 24);
        assert!(
            text.contains("design a board"),
            "user msg should render:\n{text}"
        );
        assert!(
            text.contains("search_symbols") && text.contains("4 hits"),
            "tool card should render:\n{text}"
        );
    }

    #[test]
    fn render_tool_posts_an_inline_image_cell_shown_as_a_label_in_text_mode() {
        let mut a = app();
        for c in "render the board".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "render_board".into(),
        }));
        // A render tool returns a PNG path: an inline image cell is posted.
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "render_board".into(),
            summary: "routed view → ok".into(),
            image_path: Some("/tmp/proj/.autopcb/renders/000.png".into()),
        }));
        assert_eq!(a.images.len(), 1, "an image cell was posted");
        // In text-label mode (no picker, like the screenshot harness) the cell
        // renders its stable label, never graphics.
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("board preview"), "image label shows:\n{text}");
        assert!(text.contains("000.png"), "label carries the path:\n{text}");
    }

    #[test]
    fn multiline_assistant_text_renders_every_line() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "first line\nsecond line".into(),
        )));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("first line"), "line 1:\n{text}");
        assert!(text.contains("second line"), "line 2:\n{text}");
    }

    #[test]
    fn long_entries_wrap_and_the_tail_stays_visible() {
        let mut a = app();
        for i in 0..30 {
            a.update(Msg::Agent(AgentEvent::AssistantText(format!(
                "message number {i} with some extra words so that it wraps across rows"
            ))));
        }
        let text = render_to_string(&mut a, 40, 12);
        assert!(
            text.contains("number 29"),
            "newest entry visible at the tail:\n{text}"
        );
    }

    #[test]
    fn overscroll_is_clamped_and_indicated() {
        let mut a = app();
        for i in 0..20 {
            a.update(Msg::Agent(AgentEvent::AssistantText(format!("m{i}"))));
        }
        a.scroll = u16::MAX;
        let text = render_to_string(&mut a, 40, 12);
        assert!(a.scroll < u16::MAX, "scroll clamps to the content height");
        assert!(text.contains("↑"), "scrolled-back indicator:\n{text}");
        assert!(text.contains("m0"), "clamped view shows the top:\n{text}");
    }

    #[test]
    fn running_turn_shows_the_status_line() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Tick);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("working"), "running verb:\n{text}");
        assert!(text.contains("esc to interrupt"), "interrupt hint:\n{text}");
    }

    #[test]
    fn running_row_shows_the_live_tool_detail_line() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted { name: "route_board".into() }));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("route_board"), "active tool named under spinner:\n{text}");
        // When the tool finishes, the detail row clears.
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "route_board".into(),
            summary: "ok".into(),
            image_path: None,
        }));
        assert!(a.active_tool.is_none(), "detail clears when the tool finishes");
    }

    #[test]
    fn running_line_shows_streamed_output_tokens() {
        let mut a = app();
        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 100,
            output_tokens: 1200,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
        }));
        a.update(Msg::Tick);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("↓1.2k tok"), "per-turn output tokens:\n{text}");
    }

    #[test]
    fn idle_app_has_no_running_line() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(!text.contains("esc to interrupt"), "no running line idle:\n{text}");
    }

    #[test]
    fn pending_diff_shows_approve_and_reject() {
        let mut a = app();
        a.update(Msg::PendingDiff(json!({
            "ok": true,
            "would_write": true,
            "diff": { "added": ["U1"], "removed": [], "changed": [], "nets_before": 0, "nets_after": 5 }
        })));
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("proposed changes"), "diff header:\n{text}");
        assert!(text.contains("+ U1"), "added refdes:\n{text}");
        assert!(text.contains("approve"), "approve hint:\n{text}");
        assert!(text.contains("reject"), "reject hint:\n{text}");
    }

    #[test]
    fn placeholder_shows_when_input_is_empty() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("type a prompt"), "placeholder:\n{text}");
    }

    #[test]
    fn completion_popup_lists_matches_and_highlights_selection() {
        let mut a = app();
        for c in "/c".chars() {
            a.update(Msg::Char(c));
        }
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("/clear"), "popup lists /clear:\n{text}");
        assert!(text.contains("/compact"), "popup lists /compact:\n{text}");
        assert!(text.contains("commands"), "popup title:\n{text}");

        a.update(Msg::Complete);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("/clear"), "first match filled:\n{text}");
    }

    #[test]
    fn no_completion_popup_for_plain_prompts() {
        let mut a = app();
        for c in "hello".chars() {
            a.update(Msg::Char(c));
        }
        let text = render_to_string(&mut a, 80, 24);
        assert!(!text.contains("Tab to complete"), "no popup:\n{text}");
    }

    #[test]
    fn status_bar_shows_the_cost_hud_after_usage() {
        let mut a = app(); // bedrock opus-4-5 → priced at $5/$25 per 1M
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 23_000,
            output_tokens: 400,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
        }));
        // Wide enough that every HUD field fits (narrow terminals drop fields;
        // here we want the full token/cost/context/elapsed run).
        let text = render_to_string(&mut a, 120, 24);
        assert!(text.contains("23.4k tok"), "total tokens:\n{text}");
        assert!(text.contains("(23.0k/400)"), "in/out split:\n{text}");
        // 23000 in / 1M * $5 + 400 out / 1M * $25 = $0.115 + $0.01 = $0.12 (2dp).
        assert!(text.contains("$0.12"), "session cost:\n{text}");
        // ctx 23.4k of a 200k window → ~88% left.
        assert!(text.contains("88% ctx left"), "context-left percentage:\n{text}");
    }

    #[test]
    fn status_bar_shows_a_cached_badge_and_dash_for_unpriced_models() {
        // An unknown model id has no price → cost renders "—", never wrong.
        let mut a = App::new(Status::new(
            "openai",
            "some/unknown-model-9",
            "/tmp/p/d.kicad_sch",
            true,
        ));
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 10_000,
            output_tokens: 500,
            cache_write_tokens: 0,
            cache_read_tokens: 8_000, // non-trivial cache read → "cached" badge
        }));
        let text = render_to_string(&mut a, 120, 24);
        assert!(text.contains("cached"), "cache indicator shows:\n{text}");
        assert!(text.contains('—'), "unpriced model renders a dash, not a number:\n{text}");
    }

    #[test]
    fn esc_armed_shows_the_unwind_hint() {
        let mut a = app();
        a.update(Msg::Cancel);
        assert!(a.esc_armed);
        let text = render_to_string(&mut a, 80, 24);
        assert!(
            text.contains("open the unwind picker"),
            "unwind hint:\n{text}"
        );
    }

    #[test]
    fn user_message_has_an_accent_caret_and_no_speaker_labels() {
        let mut a = app();
        for c in "hello there".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("›"), "user accent caret:\n{text}");
        assert!(text.contains("hello there"), "user text:\n{text}");
        assert!(!text.contains("you  "), "no `you` gutter label:\n{text}");
        assert!(!text.contains("ai   "), "no `ai` gutter label:\n{text}");
    }

    #[test]
    fn unwind_picker_overlay_lists_prompts_and_hint() {
        let mut a = app();
        a.open_unwind(vec!["swap the regulator".into(), "add usb-c".into()]);
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("unwind to"), "picker title:\n{text}");
        assert!(text.contains("swap the regulator"), "newest prompt:\n{text}");
        assert!(text.contains("add usb-c"), "older prompt:\n{text}");
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(23_400), "23.4k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
    }

    #[test]
    fn status_bar_shows_the_model_name() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("bedrock"), "provider in status bar:\n{text}");
        assert!(text.contains("claude-opus"), "model in status bar:\n{text}");
    }

    #[test]
    fn status_bar_hints_follow_the_mode() {
        let mut a = app();
        let idle = render_to_string(&mut a, 80, 24);
        // Idle footer is lean: provider/model only, no /command list.
        assert!(idle.contains("bedrock"), "idle footer shows the provider:\n{idle}");
        assert!(!idle.contains("/clear"), "idle footer drops the command list:\n{idle}");

        for c in "go".chars() {
            a.update(Msg::Char(c));
        }
        a.update(Msg::Submit);
        let running = render_to_string(&mut a, 80, 24);
        // The interrupt hint now lives on the running line; the bar keeps the
        // hard-quit reminder.
        assert!(
            running.contains("esc to interrupt"),
            "running line hint:\n{running}"
        );
        assert!(running.contains("Ctrl-C quit"), "running bar hint:\n{running}");

        a.update(Msg::PendingDiff(json!({
            "diff": { "added": ["U1"], "removed": [], "changed": [] }
        })));
        let gated = render_to_string(&mut a, 80, 24);
        // The gate's actions live on the card, not duplicated in the footer.
        assert!(gated.contains("approve"), "gate shows approve action:\n{gated}");
    }

    #[test]
    fn status_bar_ellipsizes_instead_of_colliding_with_hints() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 19_000,
            output_tokens: 200,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
        }));
        // Arming Esc surfaces the right-side unwind hint; once the HUD fields have
        // all collapsed and even the model anchor can't fit beside the hint, the
        // anchor itself ellipsizes rather than overlapping it.
        a.update(Msg::Cancel);
        let text = render_to_string(&mut a, 36, 24);
        let bar = text.lines().last().expect("status row");
        // The right-edge hint survives intact; the left status is ellipsized.
        assert!(bar.contains("unwind"), "right hint pinned to the edge:\n{bar}");
        assert!(bar.contains('…'), "left status is truncated, not overlapped:\n{bar}");
    }

    #[test]
    fn status_hud_collapses_fields_when_the_bar_is_narrow() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 23_000,
            output_tokens: 400,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
        }));
        // Wide: the full HUD (tokens, cost, context-left) is present.
        let wide = render_to_string(&mut a, 120, 24);
        let wide_bar = wide.lines().last().expect("status row");
        assert!(wide_bar.contains("ctx left"), "wide bar keeps context field:\n{wide_bar}");
        assert!(wide_bar.contains('$'), "wide bar keeps cost field:\n{wide_bar}");
        assert!(wide_bar.contains("tok"), "wide bar keeps token field:\n{wide_bar}");

        // Narrow: tail fields drop, but the model anchor always survives.
        let narrow = render_to_string(&mut a, 30, 24);
        let narrow_bar = narrow.lines().last().expect("status row");
        assert!(narrow_bar.contains("opus"), "model anchor survives the collapse:\n{narrow_bar}");
        assert!(!narrow_bar.contains("ctx left"), "tail field dropped when narrow:\n{narrow_bar}");
    }

    #[test]
    fn header_shows_schematic_and_kicad_state() {
        let mut a = app();
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("design.kicad_sch"), "sch path:\n{text}");
        assert!(text.contains("KiCAD"), "kicad indicator:\n{text}");
    }

    #[test]
    fn help_overlay_renders_when_toggled() {
        let mut a = app();
        a.help = true;
        let text = render_to_string(&mut a, 80, 24);
        assert!(text.contains("help"), "help overlay:\n{text}");
        assert!(text.contains("/undo"), "help lists commands:\n{text}");
        assert!(text.contains("Esc Esc"), "help covers unwind:\n{text}");
    }
}
