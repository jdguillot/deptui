//! Semantic colour slots, plus the monochrome fallback.
//!
//! Widget code never names a concrete colour — it asks for a *role*
//! (focus, warning, muted, …) and this module decides what that role
//! looks like. Two reasons this matters beyond tidiness:
//!
//! - **Retheming is a one-file change.** The slots below are the only
//!   place a colour is chosen; `ui.rs` is 3000 lines of `theme::WARNING`
//!   and friends. Swapping the palette (or adding a light variant) never
//!   means re-reading every widget to work out which yellow meant
//!   "focus" and which meant "look at this".
//! - **Roles that share a hue today can diverge tomorrow.** [`FOCUS`],
//!   [`KEY`], [`WARNING`] and [`BUSY`] are all yellow right now. They are
//!   still four separate constants because they answer four different
//!   questions, and a future theme is free to answer them differently.
//!
//! Every slot is a 16-colour ANSI name on purpose. Those are *relative*
//! — the user's own terminal theme decides what "yellow" is — so the UI
//! sits correctly on light and dark backgrounds without us guessing at
//! RGB values. 256-colour and true-colour would each pin an absolute
//! shade that clashes with somebody's theme.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;

/// Foreground for text drawn *on top of* a filled accent background
/// (chips, the selected host row, search highlights). Black reads
/// against every accent below in both light and dark terminals.
pub const ON_ACCENT: Color = Color::Black;

/// Secondary text: field labels, timestamps, "not checked" states,
/// anything the eye should skip on the way to the real content.
pub const MUTED: Color = Color::DarkGray;

/// Brighter-than-body text, used inside filled chips where the default
/// foreground would sit too close to the chip's own background.
pub const EMPHASIS: Color = Color::White;

/// The pane that currently owns the keyboard: its border, its title,
/// and its hot-letter marker.
pub const FOCUS: Color = Color::Yellow;

/// A keybinding letter in a hint row, command button, or help entry.
pub const KEY: Color = Color::Yellow;

/// Something the user should read before continuing: a closure that
/// grew, packages that changed, a deploy that will compile.
pub const WARNING: Color = Color::Yellow;

/// Work in flight — the title-bar job label and the preflight spinners.
pub const BUSY: Color = Color::Yellow;

/// A failure, a refusal, or a destructive confirmation.
pub const ERROR: Color = Color::Red;

/// A check that came back clean.
pub const SUCCESS: Color = Color::Green;

/// Neutral, non-actionable state — "this profile has never been
/// deployed" is neither good nor bad news.
pub const INFO: Color = Color::Blue;

/// Primary accent: the selected host, section headers, store-path
/// fingerprints, in-flight spinners.
pub const ACCENT: Color = Color::Cyan;

/// Secondary accent: per-host SSH overrides and the input prompts that
/// edit them. Distinct from [`ACCENT`] so "you are typing into
/// something" never looks like "this row is selected".
pub const BRAND: Color = Color::Magenta;

/// Background for a transient cursor or a visual-mode selection. The
/// host list owns the strong [`ACCENT`] highlight because its selection
/// persists across pane changes; toggles, command buttons and visual
/// selections are transient and get this quieter surface instead.
pub const SURFACE_SEL: Color = Color::DarkGray;

/// Every search hit except the one the cursor is on.
pub const SEARCH_MATCH: Color = Color::Magenta;

/// The search hit `n` / `Shift+N` last landed on.
pub const SEARCH_ACTIVE: Color = Color::Cyan;

/// Palette used to colour host prefixes in the job-log pane. Chosen to
/// be distinct from each other and to avoid the roles reserved above,
/// so an interleaved batch stays readable without a legend.
pub const JOB_LOG: &[Color] = &[
    Color::LightBlue,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
    Color::LightRed,
];

/// Whether this session must render without colour.
///
/// Cached: the environment cannot change under a running process, and
/// this is read for effectively every cell of every frame.
pub fn monochrome() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        monochrome_for(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// The [`monochrome`] decision, as a pure function of the two variables
/// it reads, so it can be tested without mutating the process
/// environment.
///
/// `NO_COLOR` follows <https://no-color.org>: *present and non-empty*
/// disables colour. An empty value is explicitly not a request for
/// monochrome, which is why this is not a plain `is_some`.
fn monochrome_for(no_color: Option<&str>, term: Option<&str>) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return true;
    }
    // `TERM=dumb` (and an empty TERM) means a terminal with no
    // capabilities to speak of. Sending it SGR colour is at best noise.
    matches!(term, Some("dumb") | Some(""))
}

/// Full-screen pass that strips colour from an already-rendered frame.
///
/// Rendered *last*, over `frame.area()`, when [`monochrome`] is set.
/// Doing it as a post-pass rather than by neutering every slot above is
/// what keeps the filled chips legible: a cell that carried meaning in
/// its background gets reverse video instead, which is the one highlight
/// every terminal has had since the 1970s. Modifiers (bold, dim) survive
/// untouched — they are the remaining hierarchy once hue is gone.
///
/// This only handles the *decorative* half of the contract. Anything
/// where colour was the sole signal has to encode that signal in a glyph
/// as well; see the reachability dots and `sys:`/`home:` badges.
#[derive(Debug, Clone, Copy)]
pub struct Monochrome;

impl Widget for Monochrome {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let Some(cell) = buf.cell_mut((x, y)) else {
                    continue;
                };
                if cell.bg != Color::Reset {
                    // XOR rather than OR: a chip that was *already*
                    // reversed (the focused pane's hot letter) and also
                    // carried a background would otherwise come out
                    // indistinguishable from its neighbours.
                    cell.modifier ^= Modifier::REVERSED;
                }
                cell.fg = Color::Reset;
                cell.bg = Color::Reset;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn no_color_needs_a_non_empty_value() {
        assert!(monochrome_for(Some("1"), Some("xterm-256color")));
        assert!(monochrome_for(Some("anything"), Some("xterm-256color")));
        assert!(!monochrome_for(Some(""), Some("xterm-256color")));
        assert!(!monochrome_for(None, Some("xterm-256color")));
    }

    #[test]
    fn dumb_terminals_get_no_color() {
        assert!(monochrome_for(None, Some("dumb")));
        assert!(monochrome_for(None, Some("")));
        assert!(!monochrome_for(None, None));
    }

    #[test]
    fn monochrome_pass_clears_colour_and_reverses_filled_cells() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        // A plain coloured word, a filled chip, and an untouched cell.
        buf.cell_mut((0, 0))
            .unwrap()
            .set_style(Style::default().fg(WARNING));
        buf.cell_mut((1, 0))
            .unwrap()
            .set_style(Style::default().fg(ON_ACCENT).bg(ACCENT));

        Monochrome.render(area, &mut buf);

        for x in 0..3 {
            let cell = buf.cell((x, 0)).unwrap();
            assert_eq!(cell.fg, Color::Reset, "fg survived at x={x}");
            assert_eq!(cell.bg, Color::Reset, "bg survived at x={x}");
        }
        assert!(!buf
            .cell((0, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        assert!(buf
            .cell((1, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
        assert!(!buf
            .cell((2, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED));
    }
}
