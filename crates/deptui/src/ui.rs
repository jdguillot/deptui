//! TUI bootstrap + rendering primitives.
//!
//! `init` / `restore` set up the terminal in raw mode with the alternate
//! screen, and `draw` paints the current [`App`] state. Keep all crossterm
//! plumbing here so the App can stay focused on state transitions.

use std::collections::HashMap;
use std::io::{stdout, Stdout};

use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::app::{
    App, FocusPane, InputMode, LastDeploy, OverrideField, PromptSource, VisualMode, COMMANDS,
};
use crate::deploy::{Mode, ProfileSel, TOGGLES};
use crate::host::{LogKind, PkgChange, Reachability, UpdateState};
use crate::theme;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init(mouse: bool) -> Result<Tui> {
    install_panic_hook();
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    if mouse {
        // Mouse capture trades away the terminal's native text
        // selection (hold Shift for it); `--no-mouse` opts out.
        execute!(out, EnableMouseCapture).context("enabling mouse capture")?;
    }
    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("constructing terminal")?;
    Ok(terminal)
}

/// Put the terminal back the way we found it before a panic message is
/// printed.
///
/// `main` calls [`restore`] after `App::run` returns, but a panic unwinds
/// straight past it. The user is then left in raw mode inside the
/// alternate screen: no echo, no line editing, and the panic message
/// itself written to a screen that is about to be discarded — so they see
/// nothing and have to blind-type `reset`. The hook chains to the
/// previous one so the message still reaches the real terminal.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Already panicking — every step here is best-effort.
            let _ = restore();
            previous(info);
        }));
    });
}

/// Leave the alternate screen and raw mode. Safe to call when neither is
/// active, and safe to call more than once — the panic hook and `main`
/// can both reach it.
pub fn restore() -> Result<()> {
    let mut out = stdout();
    // Harmless when capture was never enabled.
    execute!(out, DisableMouseCapture).ok();
    execute!(out, LeaveAlternateScreen).ok();
    disable_raw_mode().ok();
    Ok(())
}

/// Smallest terminal this layout is honest at.
///
/// The body needs 16 rows before the details pane (11 summary rows + 2
/// borders) crowds the host list out entirely, and the two-column split
/// gives the host pane only 35% of the width — under 80 columns a row
/// like `+ ● hostname [ssh]  sys:✓ home:✓` no longer fits. Rather than
/// render a layout whose panes silently lie about their contents, we say
/// so and wait for the resize. 80x24 is also the conventional floor, so
/// nobody is surprised by the number.
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

/// Paint one frame of the app.
///
/// Prefer [`render`], which wraps this in a synchronized update. This is
/// public for the render tests, which drive a `TestBackend` directly and
/// have no terminal to synchronize against.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        finish_frame(frame, area);
        return;
    }

    // Vertical layout, top → bottom:
    //   1. title bar
    //   2. toggles strip — bordered block (3 or 4 rows — see below)
    //   3. body (hosts | details | job log)
    //   4. commands pane — bordered block (3 or 4 rows), split
    //      horizontally into an info-hints half (left) and a
    //      navigable command button row (right). The info half is
    //      context-aware: it re-renders its hint list based on the
    //      currently focused pane, so j/k mean different things in
    //      Hosts vs Details and the cheat-sheet follows.
    //   5. input prompt strip — only present (1 row) when an
    //      override/confirm input mode is active. In Normal mode the
    //      row collapses to 0 so the bottom edge of the commands box
    //      sits flush with the terminal border instead of leaving an
    //      empty gap.
    //
    // **Adaptive height.** The toggles/commands strips normally sit
    // at 3 rows (1 inner). On narrow terminals — or when the context
    // info line gets long — a single inner row can't fit the whole
    // content. Instead of truncating, we bump the relevant strip to
    // 4 rows (2 inner) and let ratatui's `Wrap` split the spans
    // across the two lines. Measurement below is pessimistic (adds a
    // small fudge for the leading space + trailing margin) so we
    // always bump *before* the content actually gets clipped.
    let needs_input_strip = !matches!(app.input, InputMode::Normal);
    let input_strip_height = if needs_input_strip { 1 } else { 0 };

    // Toggles: single-row content, full width minus borders.
    let toggles_content_w = toggles_content_width();
    let toggles_inner_w = area.width.saturating_sub(2) as usize;
    let toggles_height: u16 = if toggles_content_w > toggles_inner_w {
        4
    } else {
        3
    };

    // Commands row: two-column layout (60% commands / 40% info). Measure
    // each side independently and bump the whole strip if *either* side
    // overflows its column.
    let cmd_col_w = (area.width as usize * 60) / 100;
    let info_col_w = area.width as usize - cmd_col_w;
    let cmd_inner_w = cmd_col_w.saturating_sub(2);
    let info_inner_w = info_col_w.saturating_sub(2);
    let info_content_w = info_content_width(app);
    // The button row packs whole buttons onto as many rows as it
    // needs (capped); the info half still word-wraps onto up to two.
    let cmd_rows = layout_commands(app, cmd_inner_w).len().max(1);
    let info_rows = if info_content_w > info_inner_w { 2 } else { 1 };
    let commands_height: u16 = 2 + cmd_rows.max(info_rows) as u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(toggles_height),
            Constraint::Min(5),
            Constraint::Length(commands_height),
            Constraint::Length(input_strip_height),
        ])
        .split(area);

    // Rebuild the hit-test map for exactly this frame; stale rects
    // from a previous layout must never catch a click.
    app.mouse = crate::app::MouseMap::default();
    app.copy_panes.clear();
    if app.agent.open {
        // The agent view replaces the whole working area — it is a
        // different *mode* of the app, not a pane. Popups below still
        // overlay it (password prompts can arrive mid-view).
        draw_agent_screen(frame, area, app);
    } else {
        draw_title(frame, chunks[0], app);
        app.mouse.toggles = Some(inner_rect(chunks[1]));
        app.mouse.toggle_items = toggle_hit_ranges();
        draw_toggles_strip(frame, chunks[1], app);
        draw_body(frame, chunks[2], app);
        draw_commands_row(frame, chunks[3], app);
        if needs_input_strip {
            draw_input_strip(frame, chunks[4], app);
        }
    }
    // Note: draw_help_popup borrows `app.help_scroll` mutably, so it
    // runs after the body which already returned its &mut borrow.

    if app.show_help {
        // The popup clamps `help_scroll` against the rendered content
        // height in-place, so the next keypress can't accumulate
        // phantom offset past the bottom of the cheat sheet.
        draw_help_popup(frame, area, app);
    }
    if let InputMode::EditIdentityPicker {
        entries,
        selected,
        buf,
    } = &app.input
    {
        draw_identity_picker_popup(frame, area, entries, *selected, buf);
    }
    if let InputMode::ConfirmDeploy {
        hosts,
        mode,
        profile,
    } = &app.input
    {
        draw_confirm_popup(frame, area, app, hosts, *mode, *profile);
    }
    if let InputMode::ConfirmQuit { deploy_running } = &app.input {
        draw_confirm_quit_popup(frame, area, *deploy_running);
    }
    if let InputMode::PasswordPrompt {
        prompt,
        buf,
        source,
    } = &app.input
    {
        draw_password_popup(frame, area, prompt, buf, source);
    }

    finish_frame(frame, area);
}

/// Last pass over a finished frame: drop every colour when the user has
/// asked for monochrome. Runs after the popups so nothing painted later
/// can sneak a colour back in.
fn finish_frame(frame: &mut Frame, area: Rect) {
    if theme::monochrome() {
        frame.render_widget(theme::Monochrome, area);
    }
}

/// The whole screen when the terminal is below [`MIN_WIDTH`] x
/// [`MIN_HEIGHT`].
///
/// Deliberately plain: no borders, no centring maths that could underflow,
/// and every line optional. This is the one screen guaranteed to be drawn
/// into an area we already know is too small for the real layout, and a
/// panic here would take the TUI down mid-resize. It names the actual
/// numbers because "too small" without them just prompts the user to
/// guess how much bigger.
fn draw_too_small(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            " terminal too small",
            Style::default()
                .fg(theme::WARNING)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(" need ", Style::default().fg(theme::MUTED)),
            Span::raw(format!("{MIN_WIDTH}x{MIN_HEIGHT}")),
            Span::styled("  have ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format!("{}x{}", area.width, area.height),
                Style::default().fg(theme::ERROR),
            ),
        ]),
        Line::from(Span::styled(
            " resize, or q / Ctrl-C to quit",
            Style::default().fg(theme::MUTED),
        )),
    ];

    // Centre vertically, but only once the area is provably wide enough
    // that no line wraps (the longest is 30 columns) and tall enough to
    // spare the rows. Below that, top-left is the one placement that
    // cannot push the message off the bottom of the screen.
    const LONGEST_LINE: u16 = 30;
    let area = if area.width >= LONGEST_LINE && area.height >= lines.len() as u16 * 2 {
        let top = (area.height - lines.len() as u16) / 2;
        Rect {
            y: area.y + top,
            height: area.height - top,
            ..area
        }
    } else {
        area
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Draw one frame inside a terminal *synchronized update*.
///
/// A ratatui pass writes its diff as a stream of cursor moves and cell
/// writes; a terminal that repaints partway through shows the frame
/// half-applied, which reads as tearing while the job log scrolls.
/// `CSI ? 2026 h` / `l` asks the terminal to hold its repaint until the
/// whole frame has arrived. Terminals that don't implement it ignore the
/// private mode, so this costs nothing where it isn't supported.
///
/// The guard ends the update on drop, so a failed `draw` cannot leave the
/// terminal holding its output forever.
pub fn render(terminal: &mut Tui, app: &mut App) -> Result<()> {
    let _sync = SyncUpdate::begin();
    terminal
        .draw(|f| draw(f, app))
        .context("rendering a frame")?;
    Ok(())
}

struct SyncUpdate;

impl SyncUpdate {
    fn begin() -> Self {
        // Best-effort: a terminal that rejects the sequence still gets a
        // correct (just un-synchronized) frame.
        let _ = execute!(stdout(), BeginSynchronizedUpdate);
        Self
    }
}

impl Drop for SyncUpdate {
    fn drop(&mut self) {
        let _ = execute!(stdout(), EndSynchronizedUpdate);
    }
}

fn draw_title(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " deptui ",
            Style::default()
                .bg(theme::BRAND)
                .fg(theme::ON_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            concat!(" v", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme::MUTED),
        ),
        Span::raw(" "),
        Span::styled(&app.flake, Style::default().fg(theme::ACCENT)),
    ];
    if let Some(busy) = &app.busy_label {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("⟳ {busy}"),
            Style::default().fg(theme::BUSY),
        ));
        // Hint that the running job can be cancelled with `x`. Always
        // visible during a deploy so the user doesn't have to dig
        // through help.
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "[x to cancel]",
            Style::default().fg(theme::MUTED),
        ));
    } else if app.agent_managed.values().any(|m| m.failed) {
        let n = app.agent_managed.values().filter(|m| m.failed).count();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("agent: {n} host deploy(s) failed — press a"),
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(last) = &app.last_deploy {
        // Only show the last-deploy chip when nothing is currently
        // running, otherwise it's noise. Bright colours so the user
        // can't miss that "we are no longer mid-deploy".
        spans.push(Span::raw("  "));
        spans.push(deploy_outcome_chip(last));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Coloured pill rendering of a [`LastDeploy`]. Used in both the title
/// bar and the details summary so the same status is consistent.
fn deploy_outcome_chip(last: &LastDeploy) -> Span<'static> {
    let (icon, label, bg) = if last.ok {
        ("✓", "DONE", theme::SUCCESS)
    } else {
        ("✗", "FAILED", theme::ERROR)
    };
    Span::styled(
        format!(
            " {icon} {label}  {} ({} / {})  exit {} ",
            last.node,
            last.mode.label(),
            last.profile.label(),
            last.exit_code,
        ),
        Style::default()
            .bg(bg)
            .fg(theme::ON_ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_body(frame: &mut Frame, area: Rect, app: &mut App) {
    // Two-column layout: left = hosts (top) + details (bottom) stacked;
    // right = job log (full height). The job log gets the majority of
    // horizontal space since it's what the user watches during a run.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    // Left column: compute the details height first (node summary rows
    // + extras + 2 border rows), then give the remainder to hosts.
    let details_inner_rows = 11u16; // fixed node-summary rows
    let extras_height = {
        // Peek at how many extras lines the selected node would render
        // so we can reserve that space now.
        use crate::host::UpdateState;
        let node = app.selected_node();
        let status = node.map(|n| app.status_for(&n.name));
        let n = match (node, status.as_ref()) {
            (Some(node), Some(status)) => node
                .profiles
                .keys()
                .filter(|p| {
                    let ps = status.profile(p);
                    ps.extra.local_path.is_some() && ps.update != UpdateState::NotDeployed
                })
                .count(),
            _ => 0,
        };
        n as u16 * 3 // rough estimate: each profile section is ~3 lines
    };
    // border (top+bottom) = 2; summary = 11; extras; minimum hosts = 3
    let details_total = 2 + details_inner_rows + extras_height;
    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(details_total)])
        .split(cols[0]);

    app.mouse.hosts = Some(inner_rect(left_rows[0]));
    app.mouse.job_log = Some(inner_rect(cols[1]));
    draw_host_list(frame, left_rows[0], app);
    // Details needs to clamp its scroll offset against the rendered
    // visible height so we pass &mut App.
    draw_details(frame, left_rows[1], app);
    draw_job_log(frame, cols[1], app);
    capture_copy_pane(
        frame,
        app,
        crate::app::CopyPane::JobLog,
        inner_rect(cols[1]),
    );
}

/// Snapshot a pane's rendered cells for drag-to-copy, and paint the
/// active selection as reverse video. Runs *after* the pane's widgets
/// so the capture is exactly what the user sees.
fn capture_copy_pane(frame: &mut Frame, app: &mut App, pane: crate::app::CopyPane, rect: Rect) {
    let buf = frame.buffer_mut();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(rect.height as usize);
    for y in rect.y..rect.y + rect.height {
        let mut row = Vec::with_capacity(rect.width as usize);
        for x in rect.x..rect.x + rect.width {
            row.push(buf[(x, y)].symbol().to_string());
        }
        rows.push(row);
    }
    app.copy_panes.push((pane, rect, rows));

    if let Some(sel) = &app.copy_sel {
        if sel.pane == pane && sel.dragged {
            let clamp = |(x, y): (u16, u16)| {
                (
                    x.clamp(rect.x, rect.x + rect.width.saturating_sub(1)),
                    y.clamp(rect.y, rect.y + rect.height.saturating_sub(1)),
                )
            };
            let (a, b) = (clamp(sel.anchor), clamp(sel.cursor));
            let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
                (a, b)
            } else {
                (b, a)
            };
            for y in start.1..=end.1 {
                let from = if y == start.1 { start.0 } else { rect.x };
                let to = if y == end.1 {
                    end.0
                } else {
                    rect.x + rect.width - 1
                };
                for x in from..=to {
                    buf[(x, y)].modifier |= Modifier::REVERSED;
                }
            }
        }
    }
}

/// The border-less interior of a fully-bordered pane. All the panes
/// the mouse map records use `Borders::ALL`, so this stays in step
/// with `Block::inner` without needing the block itself.
fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn draw_host_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let status = app.status_for(&node.name);
            // Checking state uses the same Braille spinner the update
            // probes use so a reachability refresh visually matches the
            // `u` refresh (the user asked for parity).
            let reach = if status.checking_reachability {
                let frame = SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
                Span::styled(frame.to_string(), Style::default().fg(theme::ACCENT))
            } else {
                reachability_dot(status.reachability)
            };
            let sys = badge(
                "sys",
                node.has_system(),
                status.profile("system").update,
                status.profile("system").checking,
                app.tick_counter,
            );
            let home = badge(
                "home",
                node.has_home(),
                status.profile("home").update,
                status.profile("home").checking,
                app.tick_counter,
            );
            let selected = i == app.selected;
            let name_style = if selected {
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Multi-select marker. A bright cyan `+` for marked hosts,
            // a dim space for unmarked — the column always exists so
            // the rest of the row stays aligned.
            let mark = if app.is_marked(&node.name) {
                Span::styled(
                    "+",
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            };
            // Override marker — a small magenta bracket suffix when the
            // user has set any per-host SSH overrides for this node.
            let mut row = vec![
                mark,
                Span::raw(" "),
                reach,
                Span::raw(" "),
                Span::styled(node.name.clone(), name_style),
            ];
            if app.override_for(&node.name).is_active() {
                row.push(Span::styled(
                    " [ssh]",
                    Style::default()
                        .fg(theme::BRAND)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            // Agent badge. Glyph-first (house rule): the suffix inside
            // the bracket carries the state, colour only reinforces —
            // `!` failed, `~` offline-pending, bare = managed and fine.
            if let Some(m) = app.agent_managed.get(&node.name) {
                let (label, color) = if m.failed {
                    (" [agent!]", theme::ERROR)
                } else if m.held {
                    (" [agent≠]", theme::WARNING)
                } else if m.offline {
                    (" [agent~]", theme::WARNING)
                } else {
                    (" [agent]", theme::ACCENT)
                };
                row.push(Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
            row.push(Span::raw("  "));
            row.push(sys);
            row.push(Span::raw(" "));
            row.push(home);
            ListItem::new(Line::from(row))
        })
        .collect();

    // Title shows the marked count when the user has built a batch, so
    // they can spot at a glance how many hosts s/b/d will hit.
    let count_label = if app.marked.is_empty() {
        String::new()
    } else {
        format!(" [{} marked] ", app.marked.len())
    };
    let focused = app.focus == FocusPane::Hosts;
    // `g` was repurposed to "scroll to top" across every pane, so the
    // hosts jump letter is `f` (focus hosts) — the obvious `h` is
    // already the home-profile shortcut and `n` owns search-next.
    // We inject `f` manually (instead of letting pane_title_spans
    // find it in the label) so the bracket sits before the word.
    let mut title_spans = pane_title_spans("fhosts", 'f', focused);
    if !count_label.is_empty() {
        title_spans.push(Span::styled(
            count_label,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Line::from(title_spans)),
    );
    frame.render_widget(list, area);
}

/// The one-glyph reachability indicator in the host list.
///
/// The glyph carries the state on its own — filled, hollow, absent —
/// because colour cannot. A red/green pair is the textbook failure for
/// deuteranopia, and under `NO_COLOR` all three dots would collapse into
/// the same character. The `sys:`/`home:` badges next to it already work
/// this way; this column was the last colour-only signal in the UI.
fn reachability_dot(reach: Reachability) -> Span<'static> {
    let (glyph, _, color) = reachability_cue(reach);
    Span::styled(glyph, Style::default().fg(color))
}

/// The word form of the same cue, for the details pane's status row.
fn reachability_word(reach: Reachability) -> Span<'static> {
    let (_, word, color) = reachability_cue(reach);
    Span::styled(word, Style::default().fg(color))
}

/// `(glyph, word, colour)` for a reachability state — the single home
/// for the user-facing reachability contract (see README), so the host
/// row's dot and the details pane's word can't drift apart.
fn reachability_cue(reach: Reachability) -> (&'static str, &'static str, ratatui::style::Color) {
    match reach {
        Reachability::Online => ("●", "online", theme::SUCCESS),
        Reachability::Offline => ("○", "offline", theme::ERROR),
        Reachability::Unknown => ("·", "unknown", theme::MUTED),
    }
}

/// Border colour for a pane that can hold focus: [`theme::FOCUS`] when
/// the pane owns the keyboard, [`theme::MUTED`] otherwise. The same hue
/// marks the pane's title and its jump letter, so "this pane has the
/// keyboard" is one consistent cue across the UI.
fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(theme::FOCUS)
    } else {
        // Muted rather than default-coloured. With five bordered panes on
        // screen, a focus ring that only differs in *hue* from four
        // equally-bright neighbours is easy to lose; dimming the other
        // four makes the focused one the brightest thing in the frame.
        Style::default().fg(theme::MUTED)
    }
}

/// Title styling that matches the border. Focused = bold yellow
/// (same hue as the `[k]` hot-letter backgrounds so the focused pane
/// visually "owns" its jump key), otherwise the terminal default.
fn focus_title_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme::FOCUS)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// Build a title row for a pane that has a direct-jump hotkey. The
/// hotkey letter is rendered in yellow (or bright yellow when focused)
/// so the user can see at a glance which key bounces to this pane.
///
/// We split on the first case-insensitive occurrence of `jump` inside
/// `label`; if the letter isn't in the label, the jump letter gets
/// prefixed as `[l] label`.
fn pane_title_spans(label: &str, jump: char, focused: bool) -> Vec<Span<'static>> {
    let base = focus_title_style(focused);
    let hot = Style::default().fg(theme::FOCUS).add_modifier(
        Modifier::BOLD
            | if focused {
                Modifier::REVERSED
            } else {
                Modifier::empty()
            },
    );
    // Find the first matching letter inside `label` so the title reads
    // naturally (e.g. "[h]osts" rather than "[h] hosts").
    if let Some(idx) = label
        .char_indices()
        .find(|(_, c)| c.eq_ignore_ascii_case(&jump))
        .map(|(i, _)| i)
    {
        let before = &label[..idx];
        // Advance past the matched character; char_indices gives us
        // the starting byte, so we need to take one char-width.
        let mut rest = label[idx..].chars();
        let hotchar = rest.next().unwrap_or(jump);
        let after: String = rest.collect();
        return vec![
            Span::raw(" "),
            Span::styled(before.to_string(), base),
            Span::styled(format!("[{hotchar}]"), hot),
            Span::styled(after, base),
            Span::raw(" "),
        ];
    }
    vec![
        Span::raw(" "),
        Span::styled(format!("[{jump}]"), hot),
        Span::raw(" "),
        Span::styled(label.to_string(), base),
        Span::raw(" "),
    ]
}

/// Braille spinner — same frames `cargo`/`nix` use, distinct from any
/// static badge icon so an in-flight probe is unambiguously visible.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn badge(
    label: &str,
    present: bool,
    state: UpdateState,
    checking: bool,
    tick: u64,
) -> Span<'static> {
    if !present {
        return Span::styled(format!("{label}:-"), Style::default().fg(theme::MUTED));
    }
    if checking {
        // Render the previous icon (dimmed) followed by the spinner so the
        // user can simultaneously see "what we knew before" and "we are
        // re-checking right now". When there's no prior result this just
        // collapses to the spinner.
        let frame = SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()];
        let prior = match state {
            UpdateState::UpToDate => Some('✓'),
            UpdateState::NeedsUpdate => Some('↑'),
            UpdateState::NotDeployed => Some('—'),
            UpdateState::Error => Some('!'),
            UpdateState::Unknown => None,
        };
        let text = match prior {
            Some(p) => format!("{label}:{p}{frame}"),
            None => format!("{label}:{frame}"),
        };
        return Span::styled(text, Style::default().fg(theme::ACCENT));
    }
    let (icon, color) = match state {
        UpdateState::UpToDate => ("✓", theme::SUCCESS),
        UpdateState::NeedsUpdate => ("↑", theme::WARNING),
        UpdateState::NotDeployed => ("—", theme::INFO),
        UpdateState::Error => ("!", theme::ERROR),
        UpdateState::Unknown => ("?", theme::MUTED),
    };
    Span::styled(format!("{label}:{icon}"), Style::default().fg(color))
}

fn draw_details(frame: &mut Frame, area: Rect, app: &mut App) {
    let inner = Block::default().borders(Borders::ALL).inner(area);

    let extras_lines = build_profile_extras_lines(app);
    let extras_height = extras_lines.len() as u16;
    let (summary_area, extras_area) = if extras_height > 0 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(11), Constraint::Min(0)])
            .split(inner);
        (rows[0], Some(rows[1]))
    } else {
        (inner, None)
    };

    // The details pane is display-only — it takes no keys, so it is
    // not a focus stop and its title carries no jump letter.
    let title_spans = vec![
        Span::raw(" "),
        Span::styled("details", focus_title_style(false)),
        Span::raw(" "),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(false))
        .title(Line::from(title_spans));
    frame.render_widget(block, area);

    draw_node_summary(frame, summary_area, app);
    if let Some(ea) = extras_area {
        frame.render_widget(Paragraph::new(extras_lines).wrap(Wrap { trim: false }), ea);
    }
}

/// Build the "update details" lines for the currently-selected node.
/// Returns an empty vector when nothing has been probed yet so the
/// caller can omit the section entirely.
fn build_profile_extras_lines(app: &App) -> Vec<Line<'static>> {
    let Some(node) = app.selected_node() else {
        return Vec::new();
    };
    let status = app.status_for(&node.name);
    let has_any = node
        .ordered_profiles()
        .into_iter()
        .any(|p| status.profile(&p).extra.local_path.is_some());
    if !has_any {
        return Vec::new();
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "update details",
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    )));
    for label in node.ordered_profiles() {
        let extra = &status.profile(&label).extra;
        if extra.local_path.is_none() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<6} "),
                Style::default()
                    .fg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            short_hash_span("local ", extra.local_path.as_deref()),
            Span::raw("  "),
            short_hash_span("remote ", extra.remote_path.as_deref()),
        ]));
        // Activation time + closure size delta share one row when
        // both are available so short displays don't bloat vertically.
        let mut meta: Vec<Span<'static>> = vec![Span::raw("       ")];
        if let Some(t) = extra.activation_time {
            // The mtime we read is the remote profile *symlink* (e.g.
            // `/run/current-system`), which activate-rs rewrites every
            // time the profile is switched in — i.e. how long ago this
            // exact closure became the running one.
            meta.push(Span::styled(
                "activated ",
                Style::default().fg(theme::MUTED),
            ));
            meta.push(Span::styled(
                format_time_ago(t),
                Style::default().fg(theme::SUCCESS),
            ));
            meta.push(Span::raw("  "));
        }
        if extra.checking_size {
            let frame_ch = SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
            meta.push(Span::styled(
                format!("size {frame_ch}"),
                Style::default().fg(theme::ACCENT),
            ));
        } else if let (Some(local), Some(remote)) = (extra.local_size, extra.remote_size) {
            meta.push(Span::styled("size ", Style::default().fg(theme::MUTED)));
            meta.push(size_delta_span(local, remote));
        } else {
            meta.push(Span::styled(
                "size ?  (Shift+U)",
                Style::default().fg(theme::MUTED),
            ));
        }
        if meta.len() > 1 {
            lines.push(Line::from(meta));
        }
        // Package diff: one-line summary only. The per-package
        // version changes themselves stream into the job log via
        // `[pkg]`-tagged entries, so the details pane doesn't need to
        // (and shouldn't) duplicate them — keeping it compact lets
        // the actual log breathe and avoids two competing views of
        // the same data.
        if extra.checking_pkg {
            let frame_ch = SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled(
                    format!("packages {frame_ch}"),
                    Style::default().fg(theme::ACCENT),
                ),
            ]));
        } else if let Some(diff) = extra.pkg_diff.as_ref() {
            if diff.is_identical() {
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled("packages identical", Style::default().fg(theme::SUCCESS)),
                ]));
            } else if diff.is_content_only() {
                // Content-only case: every package name+version
                // matches on both sides but the actual store paths
                // still differ. The diff is typed, so this is a
                // variant check — a change-count badge here would
                // contradict the job log (all path lines, no
                // version changes).
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(
                        "packages identical, content differs",
                        Style::default()
                            .fg(theme::WARNING)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  see job log for paths", Style::default().fg(theme::MUTED)),
                ]));
            } else {
                let total = diff.change_count();
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(
                        format!(
                            "packages ({total} change{})",
                            if total == 1 { "" } else { "s" }
                        ),
                        Style::default()
                            .fg(theme::WARNING)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "  see job log for per-package details",
                        Style::default().fg(theme::MUTED),
                    ),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::raw("       "),
                Span::styled("packages ?  (Shift+U)", Style::default().fg(theme::MUTED)),
            ]));
        }
    }
    lines
}

/// Condensed renderer for a store path — shows `label <hash-prefix>` so
/// the user can eyeball whether the local/remote match without reading
/// a 50-char `/nix/store/...` path. Falls back to `(unknown)` when the
/// path is missing.
fn short_hash_span(label: &'static str, path: Option<&str>) -> Span<'static> {
    let text = match path {
        Some(p) => {
            // Nix store paths are `/nix/store/<hash>-<name>`; grab the
            // first 10 chars of the hash as a fingerprint.
            let hash = p
                .rsplit('/')
                .next()
                .unwrap_or(p)
                .split('-')
                .next()
                .unwrap_or("");
            let short = hash.chars().take(10).collect::<String>();
            format!("{label}{short}")
        }
        None => format!("{label}?"),
    };
    Span::styled(text, Style::default().fg(theme::ACCENT))
}

/// Humanised closure size delta: `+42.3 MiB` / `-7.0 MiB` / `±0 B`.
/// Zero delta renders green (same bytes — safe), any non-zero delta
/// renders yellow so the user's eye catches the "something changed"
/// state whether the closure grew or shrank.
fn size_delta_span(local: u64, remote: u64) -> Span<'static> {
    let (delta_abs, sign) = if local >= remote {
        (local - remote, '+')
    } else {
        (remote - local, '-')
    };
    let color = if delta_abs == 0 {
        theme::SUCCESS
    } else {
        theme::WARNING
    };
    let text = if delta_abs == 0 {
        format!("{} (unchanged)", humanise_bytes(local))
    } else {
        let formatted = humanise_bytes(delta_abs);
        format!(
            "{sign}{formatted} (local {}, remote {})",
            humanise_bytes(local),
            humanise_bytes(remote),
        )
    };
    Span::styled(text, Style::default().fg(color))
}

/// Bytes → short human-readable string (B / KiB / MiB / GiB). Uses
/// binary prefixes to match `nix`.
pub fn humanise_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a past `SystemTime` as a short "Xs ago" string. Falls back
/// to "just now" for anything under a second and for clock skew. Kept
/// simple on purpose — we only need enough resolution for a user to
/// judge "is this stale?".
fn format_time_ago(t: std::time::SystemTime) -> String {
    let now = std::time::SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 1 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        // Clock went backwards — treat as fresh so we don't render
        // nonsense negative durations.
        Err(_) => "just now".to_string(),
    }
}

fn draw_node_summary(frame: &mut Frame, area: Rect, app: &App) {
    let Some(node) = app.selected_node() else {
        frame.render_widget(Paragraph::new("no nodes"), area);
        return;
    };
    let status = app.status_for(&node.name);

    let override_ = app.override_for(&node.name);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("name     ", Style::default().fg(theme::MUTED)),
            Span::raw(node.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("hostname ", Style::default().fg(theme::MUTED)),
            Span::raw(node.hostname.clone()),
        ]),
        Line::from(vec![
            Span::styled("profiles ", Style::default().fg(theme::MUTED)),
            Span::raw(node.profiles.keys().cloned().collect::<Vec<_>>().join(", ")),
        ]),
        Line::from(vec![
            Span::styled("status   ", Style::default().fg(theme::MUTED)),
            if status.checking_reachability {
                Span::styled("checking…", Style::default().fg(theme::ACCENT))
            } else {
                reachability_word(status.reachability)
            },
        ]),
        // "last up" row: anchor the reachability badge to a wall-clock
        // timestamp so the user knows how stale it is after a refresh.
        // Hidden until we have actually seen the host up at least once
        // this session (otherwise it would claim "never" for hosts we
        // just haven't probed yet).
        Line::from(vec![
            Span::styled("last up  ", Style::default().fg(theme::MUTED)),
            match status.last_online {
                Some(t) => Span::styled(format_time_ago(t), Style::default().fg(theme::SUCCESS)),
                None => Span::styled("(never seen)", Style::default().fg(theme::MUTED)),
            },
        ]),
        Line::from(vec![
            Span::styled("mode     ", Style::default().fg(theme::MUTED)),
            Span::raw(format!(
                "{} / {}",
                app.mode.label(),
                profile_selection_label(app.profile_sel)
            )),
        ]),
        Line::from(vec![
            Span::styled("override ", Style::default().fg(theme::MUTED)),
            if override_.is_active() {
                Span::styled(override_.summary(), Style::default().fg(theme::BRAND))
            } else {
                Span::styled("(none)", Style::default().fg(theme::MUTED))
            },
        ]),
        // "last     " row: persistent finished/failed indicator scoped
        // to *this* host. The title bar shows the global last deploy;
        // the details pane has to be per-host so navigating between
        // hosts doesn't make a previous host's outcome appear to belong
        // to whichever host the cursor lands on. We render the same
        // chip the title bar uses so the visual contract stays
        // single-source.
        match (&app.busy_label, app.last_deploys.get(&node.name)) {
            (Some(busy), _) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(theme::MUTED)),
                Span::styled(format!("⟳ {busy}"), Style::default().fg(theme::BUSY)),
            ]),
            (None, Some(last)) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(theme::MUTED)),
                deploy_outcome_chip(last),
            ]),
            (None, None) => Line::from(vec![
                Span::styled("last     ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    "(no deploy this session)",
                    Style::default().fg(theme::MUTED),
                ),
            ]),
        },
    ];
    lines.push(build_plan_summary_line(&status, app.tick_counter));
    lines.push(cache_drift_summary_line(&status, app.tick_counter));
    if let Some(err) = &status.last_error {
        lines.push(Line::from(vec![
            Span::styled("error    ", Style::default().fg(theme::ERROR)),
            Span::raw(err.clone()),
        ]));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One-line build-plan verdict for the details pane.
///
/// Aggregates every profile that has been preflighted, because what the
/// user wants at a glance is "does this deploy compile anything", not a
/// per-profile breakdown — the job log has the detail.
fn build_plan_summary_line(status: &crate::host::HostStatus, tick: u64) -> Line<'static> {
    let label = Span::styled("plan     ", Style::default().fg(theme::MUTED));
    if status.checking_plan {
        return Line::from(vec![
            label,
            Span::styled(
                format!(
                    "{} preflighting build…",
                    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
                ),
                Style::default().fg(theme::BUSY),
            ),
        ]);
    }
    if status.build_plans().next().is_none() {
        return Line::from(vec![
            label,
            Span::styled("not checked (Shift+P)", Style::default().fg(theme::MUTED)),
        ]);
    }
    let builds: usize = status.build_plans().map(|(_, p)| p.to_build.len()).sum();
    let fetches: usize = status.build_plans().map(|(_, p)| p.to_fetch.len()).sum();
    let download: u64 = status
        .build_plans()
        .filter_map(|(_, p)| p.download_bytes)
        .sum();
    if builds == 0 && fetches == 0 {
        return Line::from(vec![
            label,
            Span::styled(
                "nothing to build or fetch",
                Style::default().fg(theme::SUCCESS),
            ),
        ]);
    }
    let mut spans = vec![label];
    if builds > 0 {
        spans.push(Span::styled(
            format!("{builds} to compile"),
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            "nothing to compile".to_string(),
            Style::default().fg(theme::SUCCESS),
        ));
    }
    if fetches > 0 {
        let size = if download > 0 {
            format!(" ({})", humanise_bytes(download))
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!(", {fetches} to fetch{size}"),
            Style::default().fg(theme::MUTED),
        ));
    }
    Line::from(spans)
}

/// One-line substituter-drift verdict for the details pane.
///
/// "Not checked" gets its own dim wording rather than defaulting to the
/// green "ok" state — the whole point of the check is that a deploy
/// looks fine right up until it spends 40 minutes compiling.
fn cache_drift_summary_line(status: &crate::host::HostStatus, tick: u64) -> Line<'static> {
    let label = Span::styled("cache    ", Style::default().fg(theme::MUTED));
    if status.checking_cache {
        return Line::from(vec![
            label,
            Span::styled(
                format!(
                    "{} checking substituters…",
                    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
                ),
                Style::default().fg(theme::BUSY),
            ),
        ]);
    }
    match &status.cache_drift {
        None => Line::from(vec![
            label,
            Span::styled("not checked (Shift+C)", Style::default().fg(theme::MUTED)),
        ]),
        Some(d) if d.has_drift() => Line::from(vec![
            label,
            Span::styled(
                format!(
                    "adds {} unusable cache(s) for the {} build",
                    d.added_substituters.len().max(d.added_keys.len()),
                    d.site.label()
                ),
                Style::default()
                    .fg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Some(d) if d.ssh_user_trusted == Some(false) => Line::from(vec![
            label,
            Span::styled(
                "no new caches, but the ssh user is not trusted",
                Style::default().fg(theme::WARNING),
            ),
        ]),
        Some(d) => Line::from(vec![
            label,
            Span::styled(
                format!("no new caches for the {} build", d.site.label()),
                Style::default().fg(theme::SUCCESS),
            ),
        ]),
    }
}

/// Stable host→colour mapping. Uses a tiny FNV-1a hash on the host
/// name so the same host always gets the same colour across frames
/// (and across runs). The palette is small enough that collisions are
/// expected for large fleets — that's fine, it's a rough guide.
fn job_log_color(host: &str) -> Color {
    let mut hash: u32 = 2166136261;
    for b in host.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    theme::JOB_LOG[(hash as usize) % theme::JOB_LOG.len()]
}

/// Longest host name among tagged log entries, used to align the
/// prefix column in `draw_job_log`. Capped so a 40-char hostname
/// doesn't eat the whole pane.
fn job_log_prefix_width(app: &App) -> usize {
    const MAX: usize = 14;
    app.log
        .iter()
        .filter_map(|e| e.host.as_deref().map(str::len))
        .max()
        .unwrap_or(0)
        .min(MAX)
}

#[derive(Debug, Clone)]
struct StyledSegment {
    text: String,
    style: Style,
}

fn styled_segment(text: impl Into<String>, style: Style) -> StyledSegment {
    StyledSegment {
        text: text.into(),
        style,
    }
}

fn dim_style(base: Style) -> Style {
    base.patch(
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::DIM),
    )
}

fn accent_style(base: Style, color: Color) -> Style {
    base.patch(Style::default().fg(color).add_modifier(Modifier::BOLD))
}

fn plain_segments(text: &str, style: Style) -> Vec<StyledSegment> {
    vec![styled_segment(text, style)]
}

/// Style one job-log entry from its typed [`LogKind`]. The text is
/// never re-parsed — the producer attached the data when the line was
/// created (see `host::ProgressLine`).
fn style_entry(
    entry: &crate::app::LogEntry,
    base: Style,
    local_size_hint: Option<u64>,
) -> Vec<StyledSegment> {
    let tag = dim_style(base);
    match &entry.kind {
        LogKind::Plain => plain_segments(&entry.text, base),
        LogKind::Note => plain_segments(&entry.text, dim_style(base)),
        LogKind::SizeLocal(bytes) => vec![
            styled_segment("[size] ", tag),
            styled_segment("local: ", tag),
            styled_segment(humanise_bytes(*bytes), dim_style(base)),
            styled_segment(format!(" ({bytes} bytes)"), dim_style(base)),
        ],
        LogKind::SizeRemote(bytes) => {
            let remote = *bytes;
            let mut spans = vec![
                styled_segment("[size] ", tag),
                styled_segment("remote: ", tag),
                styled_segment(humanise_bytes(remote), dim_style(base)),
                styled_segment(format!(" ({remote} bytes)"), dim_style(base)),
            ];
            if let Some(local) = local_size_hint {
                let (delta_abs, sign) = if local >= remote {
                    (local - remote, '+')
                } else {
                    (remote - local, '-')
                };
                spans.push(styled_segment("  delta ", tag));
                spans.push(styled_segment(
                    format!("{sign}{}", humanise_bytes(delta_abs)),
                    accent_style(base, theme::WARNING),
                ));
            }
            spans
        }
        LogKind::PkgDone(changes) => {
            let emphasis = if *changes == 0 {
                accent_style(base, theme::SUCCESS)
            } else {
                accent_style(base, theme::WARNING)
            };
            vec![
                styled_segment("[pkg] ", tag),
                styled_segment("done ", emphasis),
                styled_segment(format!("({changes} change(s))"), emphasis),
            ]
        }
        LogKind::Pkg(change) => style_pkg_change(change, base),
    }
}

/// Segment styling for one package-diff line. Colours come from the
/// variant, text from [`PkgChange::render`] — so what's shown always
/// matches what a yank copies.
fn style_pkg_change(change: &PkgChange, base: Style) -> Vec<StyledSegment> {
    let tag = dim_style(base);
    match change {
        PkgChange::Updated { name, from, to } => vec![
            styled_segment("[pkg] ", tag),
            styled_segment(name.clone(), accent_style(base, theme::WARNING)),
            styled_segment(": ", tag),
            styled_segment(from.clone(), dim_style(base)),
            styled_segment(" → ", tag),
            styled_segment(to.clone(), accent_style(base, theme::SUCCESS)),
        ],
        PkgChange::Added { name, versions } => vec![
            styled_segment("[pkg] ", tag),
            styled_segment(name.clone(), accent_style(base, theme::WARNING)),
            styled_segment(": ", tag),
            styled_segment("+ ", accent_style(base, theme::SUCCESS)),
            styled_segment(versions.clone(), accent_style(base, theme::SUCCESS)),
        ],
        PkgChange::Removed { name, versions } => vec![
            styled_segment("[pkg] ", tag),
            styled_segment(name.clone(), accent_style(base, theme::WARNING)),
            styled_segment(": ", tag),
            styled_segment("- ", accent_style(base, theme::ERROR)),
            styled_segment(versions.clone(), accent_style(base, theme::ERROR)),
        ],
        PkgChange::ContentOnly { .. } => {
            let summary = change.render();
            let rest = summary
                .strip_prefix("(content-only)")
                .unwrap_or("")
                .to_string();
            vec![
                styled_segment("[pkg] ", tag),
                styled_segment("(content-only)", accent_style(base, theme::WARNING)),
                styled_segment(rest, dim_style(base)),
            ]
        }
        PkgChange::SampleAdded { .. }
        | PkgChange::SampleRemoved { .. }
        | PkgChange::More { .. } => {
            plain_segments(&format!("[pkg] {}", change.render()), dim_style(base))
        }
    }
}

fn highlight_segments(
    segments: Vec<StyledSegment>,
    query: Option<&str>,
    current_match: Option<usize>,
    match_counter: &mut usize,
) -> Vec<Span<'static>> {
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        return segments
            .into_iter()
            .map(|seg| Span::styled(seg.text, seg.style))
            .collect();
    };

    let hi_patch = Style::default()
        .bg(theme::SEARCH_MATCH)
        .fg(theme::ON_ACCENT)
        .add_modifier(Modifier::BOLD);
    let hi_current_patch = Style::default()
        .bg(theme::SEARCH_ACTIVE)
        .fg(theme::ON_ACCENT)
        .add_modifier(Modifier::BOLD);

    let full_text = segments
        .iter()
        .map(|seg| seg.text.as_str())
        .collect::<String>();
    let mut matches = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = full_text[cursor..].find(q) {
        let start = cursor + found;
        let end = start + q.len();
        *match_counter += 1;
        matches.push((start, end, current_match == Some(*match_counter)));
        cursor = end;
    }

    if matches.is_empty() {
        return segments
            .into_iter()
            .map(|seg| Span::styled(seg.text, seg.style))
            .collect();
    }

    let mut spans = Vec::new();
    let mut seg_start = 0usize;
    let mut match_idx = 0usize;

    for seg in segments {
        let seg_end = seg_start + seg.text.len();
        let mut local_cursor = 0usize;

        while match_idx < matches.len() && matches[match_idx].1 <= seg_start {
            match_idx += 1;
        }

        let mut scan_idx = match_idx;
        while scan_idx < matches.len() && matches[scan_idx].0 < seg_end {
            let (match_start, match_end, active) = matches[scan_idx];
            let start = match_start.max(seg_start) - seg_start;
            let end = match_end.min(seg_end) - seg_start;

            if start > local_cursor {
                spans.push(Span::styled(
                    seg.text[local_cursor..start].to_string(),
                    seg.style,
                ));
            }

            let highlight_style = if active {
                seg.style.patch(hi_current_patch)
            } else {
                seg.style.patch(hi_patch)
            };
            spans.push(Span::styled(
                seg.text[start..end].to_string(),
                highlight_style,
            ));
            local_cursor = end;
            scan_idx += 1;
        }

        if local_cursor < seg.text.len() {
            spans.push(Span::styled(
                seg.text[local_cursor..].to_string(),
                seg.style,
            ));
        }

        seg_start = seg_end;
    }

    spans
}

/// Right-column job log. This is where the actual `deploy` stdout
/// lands — every tagged line (single-host or batch) with a coloured
/// host prefix so interleaved output stays legible. Untagged lines
/// (status messages, reachability banners) stay in the details pane;
/// this pane is for the running job specifically.
///
/// The pane is always drawn (empty-state message when no deploy has
/// run yet) and always focusable, so the user can Tab or `l` to it
/// before kicking off a job and then scroll once output starts.
fn draw_job_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::JobLog;

    // Compute inner dimensions first (borders only — title doesn't
    // shrink it) so we can clamp the scroll *before* building the
    // title. Without this ordering the `[↑N]` chip flashes the
    // pre-clamp value for one frame before snapping down, which looks
    // janky when holding `k` past the top.
    let inner = Block::default().borders(Borders::ALL).inner(area);

    let width = job_log_prefix_width(app);
    // The same filter the key handling, search, and yank use — one
    // implementation, so what the pane shows and what those operate on
    // can't drift apart.
    let tagged: Vec<&crate::app::LogEntry> = app
        .filtered_log_indices_for_job_log()
        .into_iter()
        .map(|i| &app.log[i])
        .collect();

    let query = app.log_search.as_deref();

    // The "current match" index (1-based) so the active search result
    // gets a distinct cyan highlight.
    let current_match = if query.is_some() {
        let (cur, _) = app.log_search_stats();
        if cur > 0 {
            Some(cur)
        } else {
            None
        }
    } else {
        None
    };
    // Snapshot visual selection state (if any) for line building below.
    let visual_range = app.visual_sel.as_ref().map(|sel| {
        let ((sl, sc), (el, ec)) = sel.normalized();
        (sel.mode, sl, sc, el, ec)
    });

    let visible = inner.height as usize;
    if app.job_log_scroll >= tagged.len() {
        app.job_log_scroll = tagged.len().saturating_sub(1);
    }

    // Only the tail of the buffer can ever be on screen, so only that
    // tail gets turned into styled `Line`s. Building all 2000 capped
    // entries every frame — each one allocating a handful of `Span`
    // strings, then handed to a `Paragraph` that re-wraps every one of
    // them before throwing away the ones above the scroll offset — made
    // the frame cost grow with the log, which is exactly when frames
    // matter most. `+ 2` of slack covers the partially-visible entries
    // at both edges; every entry occupies at least one row, so a window
    // of `scroll + visible + 2` entries always covers `visible` rows
    // above the tail slice.
    let window_len = app.job_log_scroll + visible + 2;
    let window_start = tagged.len().saturating_sub(window_len);

    // The two pieces of state that accumulate across the *whole* pane
    // have to be caught up over the entries the window skips. Both are
    // plain prefix tests — far cheaper than building their lines.
    let mut size_locals: HashMap<String, u64> = HashMap::new();
    let mut match_counter = 0usize;
    for entry in &tagged[..window_start] {
        let host = entry.host.as_deref().unwrap_or("");
        match entry.kind {
            LogKind::SizeLocal(bytes) => {
                size_locals.insert(host.to_string(), bytes);
            }
            LogKind::SizeRemote(_) => {
                size_locals.remove(host);
            }
            _ => {}
        }
        if let Some(q) = query {
            if !q.is_empty() {
                match_counter += entry.text.matches(q).count();
            }
        }
    }

    let all_lines: Vec<Line> = tagged[window_start..]
        .iter()
        .enumerate()
        .map(|(win_idx, entry)| {
            let line_idx = win_idx + window_start;
            let host = entry.host.as_deref().unwrap_or("");
            let pad = width.saturating_sub(host.len());
            let color = job_log_color(host);
            let prefix = format!("{host}{} │ ", " ".repeat(pad));
            let body_style = if entry.is_err {
                Style::default().fg(theme::ERROR)
            } else {
                Style::default()
            };
            let prefix_span = Span::styled(
                prefix,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            );
            let local_size_hint = size_locals.get(host).copied();
            // Catch the accumulator up on *every* path — the visual
            // branches return early below, and skipping the update
            // there desyncs the size deltas of every later line.
            match entry.kind {
                LogKind::SizeLocal(bytes) => {
                    size_locals.insert(host.to_string(), bytes);
                }
                LogKind::SizeRemote(_) => {
                    size_locals.remove(host);
                }
                _ => {}
            }

            if let Some((vmode, start_line, start_col, end_line, end_col)) = visual_range {
                let in_sel = line_idx >= start_line && line_idx <= end_line;
                if in_sel {
                    let sel_bg = Style::default().bg(theme::SURFACE_SEL);
                    match vmode {
                        VisualMode::Line => {
                            // Whole line highlighted — prefix + body both get bg.
                            let mut spans = vec![prefix_span.style(
                                Style::default()
                                    .fg(color)
                                    .add_modifier(Modifier::BOLD)
                                    .bg(theme::SURFACE_SEL),
                            )];
                            spans.extend(highlight_segments(
                                style_entry(entry, body_style.patch(sel_bg), local_size_hint),
                                query,
                                current_match,
                                &mut match_counter,
                            ));
                            return Line::from(spans);
                        }
                        VisualMode::Char => {
                            // Partial selection — split the body text at
                            // the same column bounds the yank path
                            // slices with.
                            let chars: Vec<char> = entry.text.chars().collect();
                            let (s, e) = crate::joblog::char_selection_bounds(
                                chars.len(),
                                line_idx,
                                start_line,
                                start_col,
                                end_line,
                                end_col,
                            );
                            let before: String = chars[..s].iter().collect();
                            let selected: String = chars[s..e].iter().collect();
                            let after: String = chars[e..].iter().collect();
                            let mut spans = vec![prefix_span];
                            if !before.is_empty() {
                                spans.push(Span::styled(before, body_style));
                            }
                            if !selected.is_empty() {
                                spans.push(Span::styled(selected, body_style.patch(sel_bg)));
                            }
                            if !after.is_empty() {
                                spans.push(Span::styled(after, body_style));
                            }
                            return Line::from(spans);
                        }
                    }
                }
            }

            // Default path — search highlighting, no visual selection.
            let mut spans = vec![prefix_span];
            spans.extend(highlight_segments(
                style_entry(entry, body_style, local_size_hint),
                query,
                current_match,
                &mut match_counter,
            ));
            Line::from(spans)
        })
        .collect();

    // `compute_tail_scroll_offset` works purely off row counts relative to
    // the tail, so running it over the window yields the same offset it
    // would over the full list. Its scroll clamp only ever bites when the
    // window already reaches entry 0 — at any smaller scroll the window
    // provably holds more rows than the clamp needs.
    let (y_offset, top_window_idx) = if tagged.is_empty() {
        app.job_log_scroll = 0;
        (0, 0)
    } else {
        compute_tail_scroll_offset(&all_lines, &mut app.job_log_scroll, inner.width, visible)
    };
    // Publish the from-tail offset of the entry on the pane's top row
    // so visual-mode edge scrolling reacts at the *real* top. Wrapped
    // lines mean fewer entries fit than the pane has rows, so this is
    // usually less than scroll + rows - 1.
    app.job_log_top_offset = tagged
        .len()
        .saturating_sub(1)
        .saturating_sub(window_start + top_window_idx);

    // Now build the title with the already-clamped scroll value.
    let in_visual = app.visual_sel.is_some();
    let mut title_spans = if in_visual {
        let mode_label = match app.visual_sel.as_ref().map(|s| s.mode) {
            Some(VisualMode::Char) => " VISUAL ",
            _ => " VISUAL LINE ",
        };
        vec![
            Span::raw(" "),
            Span::styled(
                mode_label,
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]
    } else {
        pane_title_spans("job log", 'p', focused)
    };
    if let Some(q) = app.log_search.as_ref() {
        let (cur, total) = app.log_search_stats();
        title_spans.push(search_chip(q, cur, total));
    }
    if app.job_log_scroll > 0 {
        title_spans.push(scroll_chip(app.job_log_scroll));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(title_spans));
    frame.render_widget(block, area);

    if tagged.is_empty() {
        let empty = Line::styled(
            " (no deploy output for this host — press s / b / d to start, or mark hosts with Space)",
            Style::default().fg(theme::MUTED),
        );
        frame.render_widget(Paragraph::new(empty), inner);
        return;
    }

    frame.render_widget(
        Paragraph::new(all_lines)
            .wrap(Wrap { trim: false })
            .scroll((y_offset, 0)),
        inner,
    );
}

/// Turn an *entry-based* scroll offset into a *row-based* paragraph
/// scroll offset so wrapped content anchors against the bottom of
/// `inner`. Shared by both log panes. Clamps `scroll` in place so
/// the caller's stored scroll state never exceeds the smallest
/// value that produces `y_offset == 0` — otherwise holding `k` past
/// the top would accumulate phantom entry counts that the `[↑N]`
/// chip would happily display, without any visual movement in the
/// pane. Returns `(y_offset, top_idx)`: the paragraph scroll in
/// physical rows, and the index (within `all_lines`) of the entry
/// whose rows occupy the top of the viewport — the renderer publishes
/// that as `App::job_log_top_offset` for visual-mode edge scrolling.
///
/// Implementation: measure each line's wrapped row count once, then
/// walk from the tail summing row counts. The smallest `scroll`
/// whose cumulative tail-rows reach `max_row_offset` is the useful
/// cap — beyond that we'd be asking the Paragraph to scroll past
/// its own top.
fn compute_tail_scroll_offset(
    all_lines: &[Line<'_>],
    scroll: &mut usize,
    width: u16,
    visible: usize,
) -> (u16, usize) {
    if all_lines.is_empty() || width == 0 {
        *scroll = 0;
        return (0, 0);
    }
    // Exact per-entry row counts from ratatui's own word-wrapper
    // (`Paragraph::line_count`). The old `Line::width()/width` estimate
    // undercounted whenever the wrapper moved a too-long-for-the-
    // remainder token (any /nix/store/… path) wholly onto the next
    // row; each undercount clipped one row of the tail below the
    // viewport, so the newest lines — where Shift+V anchors — weren't
    // on screen at all. Only the window's lines reach here, so the
    // per-line Paragraph cost is bounded by the pane height.
    let per_entry_rows: Vec<usize> = all_lines
        .iter()
        .map(|line| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(width)
                .max(1)
        })
        .collect();
    let total_rows: usize = per_entry_rows.iter().sum();
    let max_row_offset = total_rows.saturating_sub(visible);

    // Walk the tail until cumulative rows meet max_row_offset — the
    // first tail-slice that pushes enough rows off the bottom is the
    // smallest `scroll` that pins the pane against its top. Anything
    // larger collapses to the same visual state, so we cap there.
    let mut cap = 0usize;
    let mut acc = 0usize;
    if max_row_offset > 0 {
        for (i, r) in per_entry_rows.iter().enumerate().rev() {
            acc += r;
            cap = per_entry_rows.len() - i;
            if acc >= max_row_offset {
                break;
            }
        }
    }
    if *scroll > cap {
        *scroll = cap;
    }

    // Row offset = (max pushed rows) − (rows actually pushed by the
    // current tail slice). When scroll == 0 this anchors the tail
    // flush with the bottom edge.
    let row_scroll: usize = if *scroll == 0 {
        0
    } else {
        let tail_start = per_entry_rows.len().saturating_sub(*scroll);
        per_entry_rows[tail_start..].iter().sum()
    };
    let y = max_row_offset.saturating_sub(row_scroll);
    // The entry whose wrapped rows reach below `y` sits on the top row
    // of the viewport.
    let mut top_idx = 0usize;
    let mut rows_above = 0usize;
    for (i, r) in per_entry_rows.iter().enumerate() {
        if rows_above + r > y {
            top_idx = i;
            break;
        }
        rows_above += r;
    }
    (y.min(u16::MAX as usize) as u16, top_idx)
}

/// Pane-title chip rendered next to a log pane label when the user
/// has scrolled back from the tail. Replaces the in-log "scrolled ↑N"
/// banner that used to sit at the top of the visible window —
/// rendering that banner inside the paragraph confused the row-exact
/// tail anchoring, so it moved to the title instead. The yellow
/// background matches the focused-pane palette.
fn scroll_chip(scroll: usize) -> Span<'static> {
    Span::styled(
        format!("[↑{scroll}] "),
        Style::default()
            .fg(theme::ON_ACCENT)
            .bg(theme::FOCUS)
            .add_modifier(Modifier::BOLD),
    )
}

/// Pane-title chip rendered next to a log pane label when that pane
/// has an active committed search query. Includes `[current/total]`
/// match counters when any match exists in the filtered view, so the
/// user can see at a glance whether `n`/`Shift+N` will actually move
/// them anywhere. Compact so it doesn't push the title off the right
/// edge of narrow terminals.
fn search_chip(query: &str, current: usize, total: usize) -> Span<'static> {
    let label = if total > 0 {
        format!("[/{} {}/{}] ", query, current, total)
    } else {
        format!("[/{} 0/0] ", query)
    };
    Span::styled(
        label,
        Style::default()
            .fg(theme::ON_ACCENT)
            .bg(theme::SEARCH_MATCH)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_toggles_strip(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == FocusPane::Toggles;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(pane_title_spans("toggles", 't', focused)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let spans = build_toggles_spans(app, focused);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Build the full span list for the toggles strip. Pulled out of
/// `draw_toggles_strip` so `toggles_content_width` can measure the
/// exact same content the renderer is about to draw — otherwise the
/// adaptive-height decision in `draw()` could disagree with what
/// actually gets rendered and bump the strip to 2 inner lines for
/// content that fits on one (or vice versa).
fn build_toggles_spans(app: &App, focused: bool) -> Vec<Span<'static>> {
    let t = app.toggles;
    // When the toggles pane has focus, the currently-navigated toggle
    // gets a reverse-video highlight so the user knows which one
    // Enter will flip.
    let sub = if focused {
        Some(app.toggle_index)
    } else {
        None
    };
    let mut spans = Vec::with_capacity(TOGGLES.len() * 2 + 1);
    spans.push(Span::raw(" "));
    for (i, def) in TOGGLES.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let key = (i + 1).to_string();
        spans.push(toggle_span(
            &key,
            def.short_label,
            (def.get)(&t),
            sub == Some(i),
        ));
    }
    spans
}

/// Rendered width (in display columns) of the toggles strip content.
/// We ignore focus-dependent styling because the widths are the same
/// with or without focus — only the colours change.
fn toggles_content_width() -> usize {
    // Each toggle renders as ` <key>:<icon> <label> ` = 6 fixed chars
    // + label; plus a 2-char separator between toggles and a leading
    // space. The labels come from the same table the renderer walks,
    // so a longer label can't fall out of sync with this width.
    let per_toggle: usize = TOGGLES.iter().map(|d| 6 + d.short_label.len()).sum();
    let separators = 2 * (TOGGLES.len() - 1);
    1 + per_toggle + separators
}

/// Linear hit ranges for the toggles strip — same width maths as
/// `toggle_span` / `toggles_content_width`, so a click lands exactly
/// where the chip painted.
fn toggle_hit_ranges() -> Vec<(usize, usize, usize)> {
    let mut out = Vec::with_capacity(TOGGLES.len());
    let mut pos = 1usize; // leading space
    for (i, def) in TOGGLES.iter().enumerate() {
        if i > 0 {
            pos += 2; // separator
        }
        let w = 6 + def.short_label.len();
        out.push((pos, pos + w, i));
        pos += w;
    }
    out
}

fn toggle_span(key: &str, label: &str, on: bool, focused: bool) -> Span<'static> {
    // The indicator dot carries the on/off signal in colour. When
    // focused, the chip gets a darker (grey) background — distinct
    // from the cyan/black background that hosts use for their
    // selection. The intent: hosts persist their selection while the
    // user moves between panes, so they "own" the strong highlight;
    // toggles and commands are transient cursors and shouldn't
    // compete visually with that.
    let icon = if on { "●" } else { "○" };
    let style = if focused {
        Style::default()
            .fg(theme::EMPHASIS)
            .bg(theme::SURFACE_SEL)
            .add_modifier(Modifier::BOLD)
    } else if on {
        Style::default().fg(theme::SUCCESS)
    } else {
        Style::default()
    };
    Span::styled(format!(" {key}:{icon} {label} "), style)
}

/// Bottom commands row: bordered box with a left-side information hint
/// column and a right-side navigable command button row. Info holds
/// the non-command hints (select, mark, toggles, focus, help, quit);
/// commands holds the per-action buttons. Each half borders and
/// titles independently so focus lights up the commands pane without
/// lighting up the info pane.
///
/// The info column is **context-aware**: its hint list changes based
/// on `app.focus`, so `j/k` in Hosts says "move selection" while in
/// Details it says "scroll" and surfaces `g/G` and `/` instead. This
/// mirrors the way the pane-specific keys actually behave.
fn draw_commands_row(frame: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Left: navigable command buttons.
    let focused = app.focus == FocusPane::Commands;
    let cmd_block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
        .title(Line::from(pane_title_spans("commands", 'c', focused)));
    let cmd_inner = cmd_block.inner(cols[0]);
    frame.render_widget(cmd_block, cols[0]);
    app.mouse.commands = Some(cmd_inner);
    app.mouse.command_items = command_hit_ranges(app, cmd_inner.width as usize);

    let lines = build_commands_lines(app, focused, cmd_inner.width as usize);
    frame.render_widget(Paragraph::new(lines), cmd_inner);

    // Right: informational hints. Never takes focus.
    let info_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MUTED))
        .title(Span::styled(" info ", Style::default().fg(theme::MUTED)));
    let info_inner = info_block.inner(cols[1]);
    frame.render_widget(info_block, cols[1]);
    let info_spans = build_info_spans(app);
    frame.render_widget(
        Paragraph::new(Line::from(info_spans)).wrap(Wrap { trim: false }),
        info_inner,
    );
}

/// Build the info-column hint spans for whichever pane currently has
/// focus. Each variant returns a `(key, description)` list rendered
/// the same way (yellow key + plain description), keeping the visual
/// style consistent no matter what's focused.
///
/// Kept paired with `info_content_width` below — if you add or
/// remove a hint here, the measurement function needs to match or
/// the adaptive-height decision in `draw()` will disagree with the
/// actual rendered width.
fn build_info_spans(app: &App) -> Vec<Span<'static>> {
    let hints: Vec<(&'static str, &'static str)> = info_hints_for(app);
    let yellow = Style::default().fg(theme::KEY);
    let mut spans = Vec::with_capacity(hints.len() * 3 + 1);
    spans.push(Span::raw(" "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, yellow));
        spans.push(Span::raw(format!(" {desc}")));
    }
    spans
}

/// The list of `(key, description)` pairs for the focused pane.
/// Pulled out of `build_info_spans` so both the renderer and the
/// width-measurer walk the exact same list.
fn info_hints_for(app: &App) -> Vec<(&'static str, &'static str)> {
    let search_active_here = matches!((app.focus, &app.log_search), (FocusPane::JobLog, Some(_)));
    match app.focus {
        FocusPane::Hosts => vec![
            ("j/k", "move"),
            ("Space", "mark"),
            ("A/X", "mark all/none"),
            ("g/G", "top/bottom"),
            ("/", "search log"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
        FocusPane::JobLog => {
            if app.visual_sel.is_some() {
                return vec![
                    ("j/k", "extend"),
                    ("h/l", "col (char mode)"),
                    ("y", "yank to clipboard"),
                    ("Esc", "cancel"),
                ];
            }
            let mut v: Vec<(&'static str, &'static str)> =
                vec![("j/k", "scroll"), ("g/G", "top/tail"), ("/", "search")];
            if search_active_here {
                v.push(("n/N", "next/prev"));
                v.push(("Esc", "clear"));
            }
            v.push(("V", "line-select"));
            v.push(("v", "char-select"));
            v.push(("Tab", "focus"));
            v.push(("?", "help"));
            v.push(("q", "quit"));
            v
        }
        FocusPane::Toggles => vec![
            ("h/l", "move"),
            ("Enter", "flip"),
            ("1-5", "direct"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
        FocusPane::Commands => vec![
            ("h/l", "move"),
            ("Enter", "run"),
            ("Tab", "focus"),
            ("?", "help"),
            ("q", "quit"),
        ],
    }
}

/// Rendered width (in display columns) of the context-aware info
/// line for the currently focused pane. Mirrors the formatting in
/// `build_info_spans`: 1 leading space, then for each hint a
/// 2-char separator (after the first), the key, a space, and the
/// description.
fn info_content_width(app: &App) -> usize {
    let hints = info_hints_for(app);
    let mut w = 1; // leading space
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            w += 2; // "  " separator
        }
        w += key.len() + 1 + desc.len();
    }
    w
}

/// Build the right-hand commands button row. Pulled out of
/// `draw_commands_row` for the same width-measurement reason as
/// `build_toggles_spans`.
/// One visible command button, hint-resolved, with its rendered width.
struct CmdBtn {
    idx: usize,
    key: &'static str,
    label: &'static str,
    dot: Option<bool>,
    width: usize,
}

/// Pack whole buttons onto rows of `inner_w` columns — a button is
/// never split mid-chip, so the rendered lines and the click hit
/// ranges agree exactly (word-wrap would drift them apart at every
/// row break). Each row leads with one space; buttons are separated
/// by one.
fn layout_commands(app: &App, inner_w: usize) -> Vec<Vec<CmdBtn>> {
    let mut rows: Vec<Vec<CmdBtn>> = vec![Vec::new()];
    let mut pos = 1usize;
    for (i, (cmd, key, label)) in COMMANDS.iter().enumerate() {
        if !app.command_is_visible(i) {
            continue;
        }
        let (key, label) = command_hint(app, *cmd, key, label);
        let dot = command_dot(app, *cmd);
        let width = 3 + key.len() + label.len() + if dot.is_some() { 2 } else { 0 };
        let row_full = {
            let row = rows.last().expect("starts non-empty");
            !row.is_empty() && pos + 1 + width > inner_w + 1
        };
        if row_full {
            rows.push(Vec::new());
            pos = 1;
        }
        let row = rows.last_mut().expect("just ensured");
        if !row.is_empty() {
            pos += 1;
        }
        row.push(CmdBtn {
            idx: i,
            key,
            label,
            dot,
            width,
        });
        pos += width;
    }
    rows
}

fn build_commands_lines(app: &App, focused: bool, inner_w: usize) -> Vec<Line<'static>> {
    let sub = if focused {
        Some(app.command_index)
    } else {
        None
    };
    layout_commands(app, inner_w)
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = vec![Span::raw(" ")];
            for (j, b) in row.iter().enumerate() {
                if j > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(command_button(b.key, b.label, b.dot, sub == Some(b.idx)));
            }
            Line::from(spans)
        })
        .collect()
}

/// The displayed key + label for a command button. Most are static;
/// the mark-all button flips to `X unmark` while any host is marked so
/// the pane always shows the binding and action the press performs
/// (matching the Shift+A / Shift+X keys). The width maths below uses
/// the same helper, so the row can't misreport its own width.
fn command_hint(
    app: &App,
    cmd: crate::app::Command,
    key: &'static str,
    label: &'static str,
) -> (&'static str, &'static str) {
    match cmd {
        crate::app::Command::MarkAll if !app.marked.is_empty() => ("X", "unmark"),
        _ => (key, label),
    }
}

/// Two-span command button: the key in yellow (same colour as the
/// info pane hints) and the label in default text. When focused all
/// three spans share a single grey background — distinct from the
/// cyan/black highlight on the focused host row, because the host
/// selection persists across pane focus changes while the command
/// cursor is transient and shouldn't compete with it.
fn command_button(
    key: &'static str,
    label: &'static str,
    dot: Option<bool>,
    focused: bool,
) -> Vec<Span<'static>> {
    let key_style;
    let sep_style;
    let label_style;
    if focused {
        let bg = theme::SURFACE_SEL;
        key_style = Style::default()
            .fg(theme::KEY)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        sep_style = Style::default().fg(theme::MUTED).bg(bg);
        label_style = Style::default()
            .fg(theme::EMPHASIS)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
    } else {
        key_style = Style::default().fg(theme::KEY);
        sep_style = Style::default().fg(theme::MUTED);
        label_style = Style::default();
    }
    let mut spans = vec![
        Span::styled(format!(" {key}"), key_style),
        Span::styled(":", sep_style),
    ];
    // Stateful buttons (the profile toggles) carry an on/off dot like
    // the numbered toggles strip, so the current selection is visible
    // as a set — `s:● sys h:● home` reads as "both", not as a mystery
    // three-state cycle.
    if let Some(on) = dot {
        let icon = if on { "● " } else { "○ " };
        let dot_style = if focused {
            Style::default().fg(theme::EMPHASIS).bg(theme::SURFACE_SEL)
        } else if on {
            Style::default().fg(theme::SUCCESS)
        } else {
            Style::default().fg(theme::MUTED)
        };
        spans.push(Span::styled(icon.to_string(), dot_style));
    }
    spans.push(Span::styled(format!("{label} "), label_style));
    spans
}

/// Which command buttons render a state dot, and its current value.
fn command_dot(app: &App, cmd: crate::app::Command) -> Option<bool> {
    match cmd {
        crate::app::Command::ProfileSystem => Some(matches!(
            app.profile_sel,
            ProfileSel::All | ProfileSel::System
        )),
        crate::app::Command::ProfileHome => Some(matches!(
            app.profile_sel,
            ProfileSel::All | ProfileSel::Home
        )),
        _ => None,
    }
}

/// Linear hit ranges for the command buttons, derived from the same
/// packed layout the renderer draws — a click can only disagree with
/// a chip if the two ever read different layouts.
fn command_hit_ranges(app: &App, inner_w: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (row_i, row) in layout_commands(app, inner_w).into_iter().enumerate() {
        let mut pos = row_i * inner_w + 1;
        for (j, b) in row.iter().enumerate() {
            if j > 0 {
                pos += 1;
            }
            out.push((pos, pos + b.width, b.idx));
            pos += b.width;
        }
    }
    out
}

/// Bottom input strip: renders prompt text when the user is mid-input
/// (override menu, edit field, confirm popup, etc.) and is left blank
/// in Normal mode — the commands row above already carries every
/// informational hint the old cheat sheet used to show.
/// Human form of the profile selection: the *set* that is selected
/// (`system+home`), never the word "all" — toggling reads as adding
/// and removing members, which is what the s/h keys do.
fn profile_selection_label(sel: ProfileSel) -> &'static str {
    match sel {
        ProfileSel::All => "system+home",
        ProfileSel::System => "system",
        ProfileSel::Home => "home",
    }
}

fn draw_input_strip(frame: &mut Frame, area: Rect, app: &App) {
    let line = match &app.input {
        InputMode::ConfirmDeploy { .. } => Line::from(vec![
            Span::styled(
                " confirm ▸ ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::WARNING)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("y", Style::default().fg(theme::KEY)),
            Span::raw(" / "),
            Span::styled("Enter", Style::default().fg(theme::KEY)),
            Span::raw(" confirm  "),
            Span::styled("n", Style::default().fg(theme::KEY)),
            Span::raw(" / "),
            Span::styled("Esc", Style::default().fg(theme::KEY)),
            Span::raw(" cancel"),
        ]),
        InputMode::EditIdentityPicker { .. } => Line::from(vec![
            Span::styled(
                " identity ▸ ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Ctrl+J/K", Style::default().fg(theme::KEY)),
            Span::raw(" pick  "),
            Span::styled("type", Style::default().fg(theme::KEY)),
            Span::raw(" custom path  "),
            Span::styled("Enter", Style::default().fg(theme::KEY)),
            Span::raw(" save  "),
            Span::styled("Esc", Style::default().fg(theme::KEY)),
            Span::raw(" cancel"),
        ]),
        InputMode::EditOverride { field, buf } => {
            let label = match field {
                OverrideField::Hostname => "hostname / IP",
                OverrideField::User => "ssh user",
                OverrideField::Identity => "identity file",
                OverrideField::Opts => "extra ssh opts",
            };
            Line::from(vec![
                Span::styled(
                    format!(" {label} ▸ "),
                    Style::default()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::BRAND)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(buf.clone()),
                Span::styled("▎", Style::default().fg(theme::BRAND)),
                Span::raw("   "),
                Span::styled("Enter", Style::default().fg(theme::KEY)),
                Span::raw(" save  "),
                Span::styled("Esc", Style::default().fg(theme::KEY)),
                Span::raw(" cancel"),
            ])
        }
        InputMode::OverridesMenu => Line::from(vec![
            Span::styled(
                " override ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("h", Style::default().fg(theme::KEY)),
            Span::raw(" host  "),
            Span::styled("u", Style::default().fg(theme::KEY)),
            Span::raw(" user  "),
            Span::styled("k", Style::default().fg(theme::KEY)),
            Span::raw(" key  "),
            Span::styled("o", Style::default().fg(theme::KEY)),
            Span::raw(" opts  "),
            Span::styled("c", Style::default().fg(theme::KEY)),
            Span::raw(" clear  "),
            Span::styled("Esc", Style::default().fg(theme::KEY)),
            Span::raw(" back"),
        ]),
        InputMode::SearchLog { buf } => {
            let label = " /search job log ▸ ";
            Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::BRAND)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(buf.clone()),
                Span::styled("▎", Style::default().fg(theme::BRAND)),
                Span::raw("   "),
                Span::styled("Enter", Style::default().fg(theme::KEY)),
                Span::raw(" commit  "),
                Span::styled("Esc", Style::default().fg(theme::KEY)),
                Span::raw(" cancel"),
            ])
        }
        InputMode::SearchHelp { buf } => Line::from(vec![
            Span::styled(
                " /filter help ▸ ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::raw(buf.clone()),
            Span::styled("▎", Style::default().fg(theme::BRAND)),
            Span::raw("   "),
            Span::styled("Enter", Style::default().fg(theme::KEY)),
            Span::raw(" commit  "),
            Span::styled("Esc", Style::default().fg(theme::KEY)),
            Span::raw(" clear"),
        ]),
        InputMode::ConfirmQuit { .. } => Line::from(vec![
            Span::styled(
                " quit? ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("y", Style::default().fg(theme::KEY)),
            Span::raw(" / "),
            Span::styled("Enter", Style::default().fg(theme::KEY)),
            Span::raw(" confirm  "),
            Span::styled("n", Style::default().fg(theme::KEY)),
            Span::raw(" / "),
            Span::styled("Esc", Style::default().fg(theme::KEY)),
            Span::raw(" cancel"),
        ]),
        // Normal mode has nothing extra to say — the commands row
        // above already surfaces every hint.
        InputMode::Normal => Line::raw(""),
        InputMode::PasswordPrompt { .. } => Line::raw(""),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Centered help popup. We use ratatui's `Clear` widget to wipe the
/// underlying area before drawing, so the popup looks like a real
/// modal instead of overlapping the host list.
///
/// `scroll` is a `&mut` to the user's current offset into the
/// content. We clamp it in-place against the actual rendered length
/// so a held `j` past the bottom (or `k` past the top) cannot
/// accumulate phantom offset that the user would then have to grind
/// back through with the opposite key.
fn draw_help_popup(frame: &mut Frame, area: Rect, app: &mut App) {
    let popup = centered_rect(82, 86, area);
    frame.render_widget(Clear, popup);

    // Title swaps in a `/` reminder when no filter is active so the
    // user discovers the feature.
    let title = if app.help_search.is_some() || matches!(app.input, InputMode::SearchHelp { .. }) {
        " help — ? / Esc close · / filter ".bold()
    } else {
        " help — ? / Esc close · j/k scroll · / filter ".bold()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let dim = Style::default().fg(theme::MUTED);

    // Every entry in the popup uses `key_line` so the description column
    // is at the same x for every row in every section. This is what the
    // earlier hand-padded version got wrong — different sections used
    // different widths and the SSH-overrides parent/child indent looked
    // misaligned.
    let all_lines: Vec<Line> = vec![
        section("navigation"),
        key_line("↑/↓ j/k", "within pane: hosts = selection, joblog = scroll"),
        key_line("←/→ h/l", "toggles/commands = sub-cursor (vim-style hjkl)"),
        key_line("Shift+H/L", "horizontal pane move (hosts ↔ job log)"),
        key_line("Shift+←/→", "same as Shift+H/L"),
        key_line("Shift+J/K", "vertical pane move (toggles ↔ hosts / job log ↔ commands)"),
        key_line("Shift+↑/↓", "same as Shift+J/K"),
        key_line("Tab", "cycle focus forward (toggles → hosts → joblog → commands)"),
        key_line("Shift+Tab", "cycle focus backward"),
        key_line("f/p/t/c", "btop-style jump: (f)ocus hosts, (p)ipeline log, (t)oggles, (c)ommands"),
        key_line("Enter", "activate the focused toggle or command button"),
        key_line(
            "g",
            "vim-style 'go to top' — hosts=first host, details/joblog=oldest line, help=top",
        ),
        key_line("Shift+G", "vim-style 'go to bottom' — joblog snaps to tail, help=bottom"),
        key_line("q", "quit (Esc closes popups/edits but never quits)"),
        key_line("Ctrl-C", "quit and kill any running deploy"),
        Line::raw(""),

        section("search"),
        key_line(
            "/",
            "open job-log search from any pane (n/Shift+N navigate matches after commit)",
        ),
        key_line(
            "n / Shift+N",
            "next / previous match in the job log (after committing /)",
        ),
        key_line(
            "Esc",
            "in job log: clear the committed search. While typing: abort the prompt.",
        ),
        Line::raw(""),

        section("visual selection (job log)"),
        key_line("V", "enter visual line mode — select whole lines with j/k (from any pane)"),
        key_line("v", "enter visual char mode — select by character with j/k/h/l (from any pane)"),
        key_line("y", "yank selected text to clipboard (wl-copy / xclip / xsel / pbcopy)"),
        key_line("Esc", "cancel visual selection without copying"),
        key_line(
            "mouse drag",
            "select rendered text in the job log / agent panes — copies on release",
        ),
        Line::raw(""),

        section("status"),
        key_line("r", "refresh nodes from flake.nix + online/offline (TCP probe via ssh -G) for every host"),
        key_line(
            "u",
            "check selected host (cheap tier: paths, activation time)",
        ),
        key_line(
            "Shift+U",
            "closure size delta + package diff (needs prior u)",
        ),
        key_line(
            "Shift+P",
            "build plan preflight: what this deploy compiles + download size",
        ),
        Line::from(Span::styled(
            "              runs `nix build --dry-run` the way deploy-rs would — against the",
            dim,
        )),
        Line::from(Span::styled(
            "              target's store when toggle 4 (--remote-build) is on, else locally.",
            dim,
        )),
        key_line(
            "Shift+C",
            "substituter drift: does this deploy add a cache its own build can't use?",
        ),
        Line::from(Span::styled(
            "              a new nix.settings.substituters entry only goes live after nix-daemon",
            dim,
        )),
        Line::from(Span::styled(
            "              restarts — which happens *after* the build. Compares against the",
            dim,
        )),
        Line::from(Span::styled(
            "              target's nix.conf for remote builds, this machine's for local ones.",
            dim,
        )),
        Line::from(Span::styled(
            "              badges: ✓ up-to-date   ↑ behind   — not deployed   ! error   ? unchecked   - n/a   ⠋ checking",
            dim,
        )),
        Line::raw(""),

        section("deploy"),
        key_line(
            "s / h",
            "toggle the system (NixOS) / home (home-manager) profile in the deploy target — both on = all; the last one can't be turned off",
        ),
        key_line("Shift+S", "switch — apply now (asks for confirmation)"),
        key_line(
            "Shift+B",
            "boot — install as next boot entry, don't activate now (not available for home-only)",
        ),
        key_line("Shift+D", "dry-run — `deploy --dry-activate`, build + diff only"),
        key_line("x", "cancel running deploy AND drop any queued hosts (SIGKILL the child)"),
        Line::raw(""),

        section("agent"),
        key_line("a", "open/close the agent view (configure agents in ~/.config/deptui/config.toml)"),
        key_line("j/k", "  in the view: select host"),
        key_line("u", "  ask the agent to check for updates now (kick)"),
        key_line("p / P", "  pause/resume the selected host / the whole agent (future polls only)"),
        key_line("x", "  cancel the run in flight — kills the deploy, parks its hosts at that rev"),
        key_line("d", "  force-deploy the selected host at the last-seen revision"),
        key_line("[ / ]", "  switch between configured agents"),
        Line::raw(""),

        section("multi-select / batch"),
        key_line("Space", "mark or unmark the highlighted host (the [+] column lights up)"),
        key_line("Shift+A", "mark every host"),
        key_line("Shift+X", "clear all marks"),
        Line::from(Span::styled(
            "              when any host is marked, s/b/d operate on the marked set",
            dim,
        )),
        Line::from(Span::styled(
            "              in insertion order; failures stop the queue, x cancels the rest",
            dim,
        )),
        Line::raw(""),

        section("toggles (number keys)"),
        key_line("1", TOGGLES[0].help),
        key_line("2", TOGGLES[1].help),
        key_line("3", TOGGLES[2].help),
        key_line("4", TOGGLES[3].help),
        key_line("5", TOGGLES[4].help),
        Line::raw(""),

        section("ssh overrides (per host)"),
        key_line("o", "open the overrides menu for the selected host"),
        key_line("o → h", "set hostname / IP override"),
        key_line("o → u", "set ssh user override"),
        key_line(
            "o → k",
            "pick identity file (Ctrl+J/K to scroll list, type to enter custom path)",
        ),
        key_line(
            "o → o",
            "set extra ssh -o opts (whitespace-separated, e.g. `Port=2222`)",
        ),
        key_line("o → c", "clear all overrides for this host"),
        Line::from(Span::styled(
            "              hosts with active overrides show a magenta [ssh] tag in the list",
            dim,
        )),
    ];

    // Lazygit-style filter: the live buffer (while typing) takes
    // precedence over the committed query so the popup updates as the
    // user types. Empty buffer = show everything.
    let live_query: Option<String> = match &app.input {
        InputMode::SearchHelp { buf } if !buf.is_empty() => Some(buf.clone()),
        _ => app.help_search.clone(),
    };

    // Reserve one row at the bottom for the inline search prompt
    // whenever filtering is active OR the user is mid-type. Keeps the
    // input visible inside the popup itself rather than only on the
    // app-level input strip.
    let show_search_row =
        matches!(app.input, InputMode::SearchHelp { .. }) || app.help_search.is_some();
    let (content_area, search_area) = if show_search_row {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        (rows[0], Some(rows[1]))
    } else {
        (inner, None)
    };

    let lines: Vec<Line> = if let Some(q) = live_query.as_deref() {
        // Filter section-and-detail rows by substring match. Section
        // headers themselves don't get hidden by the filter — they
        // anchor the surviving rows under the right header. Blank
        // spacer lines also stay so the layout doesn't collapse.
        all_lines
            .into_iter()
            .filter(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.is_empty() || text.contains(q)
            })
            .collect()
    } else {
        all_lines
    };

    // Clamp scroll against the actual content length so j/k can't
    // reveal a blank popup once the user has passed the last line.
    // We mutate the App-owned scroll in place so a held key past the
    // end doesn't accumulate phantom offset that the user has to
    // grind back through with the opposite key.
    let total = lines.len() as u16;
    let visible = content_area.height;
    let max_scroll = total.saturating_sub(visible);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.help_scroll, 0)),
        content_area,
    );

    // Bottom row: inline search bar. Shows the live buffer while the
    // user is typing, or the committed query (greyed out) when a
    // filter is in effect but not currently being edited.
    if let Some(rect) = search_area {
        let body = match &app.input {
            InputMode::SearchHelp { buf } => vec![
                Span::styled(
                    " /",
                    Style::default()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::FOCUS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(buf.clone()),
                Span::styled("▎", Style::default().fg(theme::FOCUS)),
            ],
            _ => vec![
                Span::styled(" /", Style::default().fg(theme::MUTED)),
                Span::raw(" "),
                Span::styled(
                    app.help_search.clone().unwrap_or_default(),
                    Style::default().fg(theme::MUTED),
                ),
                Span::styled(
                    "  (press / to edit, Esc to clear)",
                    Style::default().fg(theme::MUTED),
                ),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(body)), rect);
    }
}

/// Section header in the help popup. Bold cyan, with a blank-spacer
/// convention enforced by the call site.
fn section(name: &str) -> Line<'static> {
    Line::styled(
        name.to_string(),
        Style::default()
            .fg(theme::ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

/// One row in the help popup: a fixed-width key column followed by a
/// description. The padding is computed in display columns so different
/// chord lengths (e.g. `Tab` vs `o → h`) all line up.
const HELP_KEY_COL: usize = 12;

fn key_line(keys: &str, desc: &str) -> Line<'static> {
    // 2-space indent, then `keys`, then enough spaces to reach
    // HELP_KEY_COL, then the description. Visible width is what matters
    // because every char in `keys` is single-width ASCII or one of `→ ↑↓`.
    let visible = keys.chars().count();
    let pad = HELP_KEY_COL.saturating_sub(visible);
    let key_col = format!("  {keys}{}", " ".repeat(pad));
    Line::from(vec![
        Span::styled(
            key_col,
            Style::default().fg(theme::KEY).add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

/// Confirmation popup for s/b/d. Lists every host that will be touched
/// (sized to a sane window with overflow indication), the mode, and the
/// profile, then waits for `y`/`n`. The popup is intentionally yellow
/// so it reads as a "are you sure?" rather than a passive info dialog.
fn draw_confirm_popup(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    hosts: &[String],
    mode: Mode,
    profile: ProfileSel,
) {
    // Pick a popup size that scales with host count but stays bounded —
    // big batches still fit in a fixed window with a "+N more" tail.
    let popup = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::WARNING))
        .title(Span::styled(
            " confirm deploy ",
            Style::default()
                .fg(theme::WARNING)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();

    // Summary header — mode + profile + host count.
    lines.push(Line::from(vec![
        Span::styled("mode    ", Style::default().fg(theme::MUTED)),
        Span::styled(
            mode.label(),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("profile ", Style::default().fg(theme::MUTED)),
        Span::styled(
            profile_selection_label(profile),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    // Q28(b): warn when the agent also auto-deploys any of these hosts
    // — no lock exists, so offer the one-key pause right here.
    if app.agent_manages_any(hosts) {
        lines.push(Line::from(vec![Span::styled(
            "⚠ the agent auto-deploys some of these hosts",
            Style::default()
                .fg(theme::WARNING)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(vec![
            Span::styled("  press ", Style::default().fg(theme::MUTED)),
            Span::styled(
                "p",
                Style::default().fg(theme::KEY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to pause the agent first",
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("hosts   ", Style::default().fg(theme::MUTED)),
        Span::styled(
            format!("{}", hosts.len()),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(if hosts.len() == 1 { " host" } else { " hosts" }),
    ]));
    lines.push(Line::raw(""));

    // Host list. Capped so a 30-host batch doesn't blow past the popup
    // height; the user can read the full list in the host pane behind
    // the popup if they need to verify everything.
    const MAX_LIST: usize = 12;
    let visible = hosts.len().min(MAX_LIST);
    for name in &hosts[..visible] {
        lines.push(Line::from(vec![
            Span::styled("  • ", Style::default().fg(theme::WARNING)),
            Span::raw(name.clone()),
        ]));
    }
    if hosts.len() > MAX_LIST {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!("… +{} more", hosts.len() - MAX_LIST),
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.extend(build_plan_warning_lines(app, hosts));
    lines.extend(cache_drift_warning_lines(app, hosts));
    lines.push(Line::from(vec![
        Span::styled(
            "  y / Enter ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  confirm    "),
        Span::styled(
            " n / Esc ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  cancel"),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The build-plan banner shown inside the deploy confirmation: what this
/// deploy is about to compile, before the user commits to it.
fn build_plan_warning_lines(app: &App, hosts: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for name in hosts {
        let status = app.status_for(name);
        let builds: Vec<String> = status
            .build_plans()
            .flat_map(|(_, p)| p.build_labels())
            .collect();
        if builds.is_empty() {
            continue;
        }
        let download: u64 = status
            .build_plans()
            .filter_map(|(_, p)| p.download_bytes)
            .sum();
        let suffix = if download > 0 {
            format!(", {} to fetch", humanise_bytes(download))
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(
                " plan ",
                Style::default()
                    .fg(theme::ON_ACCENT)
                    .bg(theme::WARNING)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {name}: compiles {}{suffix}", builds.len()),
                Style::default().fg(theme::WARNING),
            ),
        ]));
        for b in builds.iter().take(4) {
            lines.push(Line::styled(
                format!("    ⚒ {b}"),
                Style::default().fg(theme::WARNING),
            ));
        }
        if builds.len() > 4 {
            lines.push(Line::styled(
                format!("    … +{} more", builds.len() - 4),
                Style::default().fg(theme::MUTED),
            ));
        }
    }
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// The substituter-drift banner shown inside the deploy confirmation.
///
/// Deliberately distinguishes "checked, and this deploy adds a cache it
/// can't use" from "never checked" — the second is not reassurance, and
/// showing nothing would read as if it were.
fn cache_drift_warning_lines(app: &App, hosts: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut unchecked = Vec::new();
    for name in hosts {
        let status = app.status_for(name);
        match status.cache_drift {
            Some(drift) if drift.has_drift() => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " cache ",
                        Style::default()
                            .fg(theme::ON_ACCENT)
                            .bg(theme::ERROR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            " {name}: adds {} cache(s) this {} build cannot use",
                            drift.added_substituters.len(),
                            drift.site.label()
                        ),
                        Style::default().fg(theme::ERROR),
                    ),
                ]));
                for url in drift.added_substituters.iter().take(3) {
                    lines.push(Line::styled(
                        format!("    + {url}"),
                        Style::default().fg(theme::ERROR),
                    ));
                }
                if drift.ssh_user_trusted == Some(false) {
                    lines.push(Line::styled(
                        format!(
                            "    `{}` is not in trusted-users — overrides are ignored silently",
                            drift.ssh_user.as_deref().unwrap_or("ssh user"),
                        ),
                        Style::default().fg(theme::ERROR),
                    ));
                }
            }
            Some(_) => {}
            None => unchecked.push(name.clone()),
        }
    }
    if !unchecked.is_empty() {
        lines.push(Line::styled(
            format!(
                "  substituter drift not checked for {} host(s) — Shift+C",
                unchecked.len()
            ),
            Style::default().fg(theme::MUTED),
        ));
    }
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

fn draw_confirm_quit_popup(frame: &mut Frame, area: Rect, deploy_running: bool) {
    let popup = centered_rect(40, 30, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ERROR))
        .title(Span::styled(
            " confirm quit ",
            Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    if deploy_running {
        lines.push(Line::from(vec![
            Span::styled("  ⚠ ", Style::default().fg(theme::WARNING)),
            Span::styled(
                "A deploy is currently running!",
                Style::default()
                    .fg(theme::WARNING)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "    It will be killed if you quit.",
            Style::default().fg(theme::WARNING),
        )));
        lines.push(Line::raw(""));
    }
    lines.push(Line::raw("  Are you sure you want to quit?"));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  y / Enter ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  quit    "),
        Span::styled(
            " n / Esc ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  cancel"),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_password_popup(
    frame: &mut Frame,
    area: Rect,
    prompt: &str,
    buf: &crate::app::SecretBuf,
    source: &PromptSource,
) {
    let popup = centered_rect(50, 30, area);
    frame.render_widget(Clear, popup);

    let (title, border_color) = match source {
        PromptSource::Askpass => (" auth ", theme::WARNING),
        PromptSource::Sudo => (" sudo ", theme::ERROR),
        PromptSource::SudoPre => (" sudo (pre-deploy) ", theme::BRAND),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let label = {
        let p = prompt.trim_end_matches(' ');
        if p.ends_with(':') {
            format!("{p} ")
        } else {
            format!("{p}: ")
        }
    };
    // Only ever the length is read — the plaintext never reaches a widget.
    let masked: String = "•".repeat(buf.char_count());

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("  {label}"),
        Style::default().fg(theme::WARNING),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(masked),
        Span::styled("▎", Style::default().fg(border_color)),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  Enter ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  send    "),
        Span::styled(
            " Esc ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  dismiss"),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Identity-file picker popup. Renders a scrollable list of scanned
/// `~/.ssh` candidates above a single-line text input. Ctrl+J/K (handled
/// in `app::handle_key_identity_picker`) moves the selection and syncs
/// the buffer; typing freely overrides the buffer with a custom path.
fn draw_identity_picker_popup(
    frame: &mut Frame,
    area: Rect,
    entries: &[std::path::PathBuf],
    selected: usize,
    buf: &str,
) {
    let popup = centered_rect(70, 60, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" identity file — Ctrl+J/K pick · type custom · Enter save · Esc cancel ".bold());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split: list (fills) + 1-line buffer at the bottom.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);

    // List items. Empty-state message keeps the layout stable when the
    // user has no scanned keys (e.g. fresh machine, or non-standard ssh
    // dir) — the typed buffer below still works.
    let items: Vec<ListItem> = if entries.is_empty() {
        vec![ListItem::new(Line::styled(
            "  (no keys found in ~/.ssh — type a path below)",
            Style::default().fg(theme::MUTED),
        ))]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let style = if i == selected {
                    Style::default()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::styled(format!(" {}", p.display()), style))
            })
            .collect()
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::BOTTOM).title("keys")),
        rows[0],
    );

    // Text input row. We render an explicit caret so the cursor is
    // visible even though we don't move the terminal cursor here.
    let input_line = Line::from(vec![
        Span::styled(
            " path ▸ ",
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::BRAND)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(buf.to_string()),
        Span::styled("▎", Style::default().fg(theme::BRAND)),
    ]);
    frame.render_widget(Paragraph::new(input_line), rows[1]);
}

/// Compute a centered popup `Rect` of the requested percentage size.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

// ---------------------------------------------------------------------------
// agent view
// ---------------------------------------------------------------------------

/// Unix-seconds variant of [`format_time_ago`] for agent wire times.
fn format_unix_ago(t: u64) -> String {
    format_time_ago(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t))
}

fn short_rev(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

/// The full-screen agent view: connection header, watch/host listing
/// with runtime controls, and the live run-log tail. Replaces the main
/// layout while [`App::agent`] is open; popups still draw on top.
fn draw_agent_screen(frame: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    // --- header ---
    let mut spans = vec![Span::styled(
        " agent ",
        Style::default()
            .bg(theme::BRAND)
            .fg(theme::ON_ACCENT)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some((name, ssh)) = app.agent.current_agent() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" ({ssh})"),
            Style::default().fg(theme::MUTED),
        ));
    }
    if app.agent.agents.len() > 1 {
        spans.push(Span::styled(
            format!("  [{}/{}]", app.agent.current + 1, app.agent.agents.len()),
            Style::default().fg(theme::MUTED),
        ));
    }
    if let Some(status) = &app.agent.status {
        spans.push(Span::styled(
            format!("  v{}", status.version),
            Style::default().fg(theme::MUTED),
        ));
        if status.paused {
            spans.push(Span::styled(
                "  [PAUSED]",
                Style::default()
                    .fg(theme::WARNING)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    if app.agent.loading {
        let sp = SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
        spans.push(Span::styled(
            format!("  {sp} fetching…"),
            Style::default().fg(theme::BUSY),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

    // --- body: watches (left) | live log (right) ---
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);
    draw_agent_watches(frame, body[0], app);
    capture_copy_pane(
        frame,
        app,
        crate::app::CopyPane::AgentWatches,
        inner_rect(body[0]),
    );
    draw_agent_tail(frame, body[1], app);
    capture_copy_pane(
        frame,
        app,
        crate::app::CopyPane::AgentTail,
        inner_rect(body[1]),
    );

    // --- footer ---
    let mut foot = vec![Span::styled(
        " j/k select  u kick  x cancel run  p pause host  P pause agent  d deploy  r refresh  [/] agent  q close",
        Style::default().fg(theme::MUTED),
    )];
    if let Some(op) = &app.agent.last_op {
        foot.push(Span::styled(
            format!("  │ {op}"),
            Style::default().fg(theme::ACCENT),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(foot)), chunks[2]);
}

fn draw_agent_watches(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MUTED))
        .title(Span::styled(
            " watches ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if app.agent.agents.is_empty() && app.agent.scanning {
        let sp = SPINNER_FRAMES[(app.tick_counter as usize) % SPINNER_FRAMES.len()];
        lines.push(Line::from(Span::styled(
            format!("{sp} scanning your deploy nodes for agents…"),
            Style::default().fg(theme::BUSY),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "hosts running deptui-agent (reachable non-interactively) appear here",
            Style::default().fg(theme::MUTED),
        )));
    } else if app.agent.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            if app.agent.scanned {
                "no agents found on your deploy nodes"
            } else {
                "no agents configured"
            },
            Style::default()
                .fg(theme::WARNING)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        if app.agent.scanned {
            // Show what each node actually said — "command not found"
            // vs "permission denied" vs a timeout are three different
            // fixes, and guessing between them is miserable.
            if !app.agent.scan_failures.is_empty() {
                lines.push(Line::from(Span::styled(
                    "what the deploy nodes said:",
                    Style::default().fg(theme::MUTED),
                )));
                for (node, err) in app.agent.scan_failures.iter().take(6) {
                    // The pane wraps; cap only pathological lengths so
                    // the informative tail ("Permission denied") is
                    // never the part that gets cut.
                    let mut err = err.clone();
                    if err.len() > 200 {
                        let cut = (0..=200).rfind(|&i| err.is_char_boundary(i)).unwrap_or(0);
                        err.truncate(cut);
                        err.push('…');
                    }
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {node}: "), Style::default().fg(theme::ACCENT)),
                        Span::styled(err, Style::default().fg(theme::ERROR)),
                    ]));
                }
                if app.agent.scan_failures.len() > 6 {
                    lines.push(Line::from(Span::styled(
                        format!("  … and {} more", app.agent.scan_failures.len() - 6),
                        Style::default().fg(theme::MUTED),
                    )));
                }
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(Span::styled(
                "\"command not found\" → the agent host must expose the CLI over ssh:",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "  the NixOS module now installs it (environment.systemPackages);",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "  rebuild the agent host with the current module.",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "socket \"Permission denied\" → add the node's deploy ssh user to",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "  services.deptui-agent.users (root needs no grant); \"No such",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "  file\" or \"Connection refused\" → is the service running?",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::from(Span::styled(
                "  (systemctl status deptui-agent). r rescans. To pin a non-node agent:",
                Style::default().fg(theme::MUTED),
            )));
            lines.push(Line::raw(""));
        }
        if let Some(err) = &app.agent.settings_error {
            lines.push(Line::from(Span::styled(
                "the settings file exists but could not be loaded:",
                Style::default().fg(theme::ERROR),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(theme::ERROR),
            )));
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "add an agent to {}:",
                crate::settings::Settings::config_path().display()
            ),
            Style::default().fg(theme::MUTED),
        )));
        lines.push(Line::raw(""));
        for l in [
            r#"  default_agent = "homelab""#,
            "",
            "  [agents.homelab]",
            r#"  ssh = "me@deploy-box"   # any ssh destination"#,
        ] {
            lines.push(Line::from(Span::styled(
                l,
                Style::default().fg(theme::ACCENT),
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "the agent itself runs on that host — see the README's agent section",
            Style::default().fg(theme::MUTED),
        )));
    } else if let Some(err) = &app.agent.error {
        lines.push(Line::from(Span::styled(
            format!("! {err}"),
            Style::default().fg(theme::ERROR),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "r retries — is the agent running and reachable over ssh?",
            Style::default().fg(theme::MUTED),
        )));
    } else if let Some(status) = &app.agent.status {
        let mut row_idx = 0usize;
        for w in &status.watches {
            let mut head = vec![
                Span::styled(
                    w.name.clone(),
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", w.ref_label),
                    Style::default().fg(theme::MUTED),
                ),
            ];
            if let Some(rev) = &w.last_seen {
                head.push(Span::styled(
                    format!("  @{}", short_rev(rev)),
                    Style::default().fg(theme::MUTED),
                ));
            }
            if w.paused {
                head.push(Span::styled(
                    "  [paused]",
                    Style::default().fg(theme::WARNING),
                ));
            }
            if let Some(r) = &w.running {
                head.push(Span::styled(
                    format!("  ⟳ deploying {} ({})", short_rev(&r.rev), r.trigger),
                    Style::default().fg(theme::BUSY),
                ));
            } else if let Some(t) = w.next_poll {
                head.push(Span::styled(
                    format!("  next poll {}", format_unix_ago_or_in(t)),
                    Style::default().fg(theme::MUTED),
                ));
            }
            lines.push(Line::from(head));

            for h in &w.hosts {
                let is_sel = app.agent.sel == row_idx;
                row_idx += 1;

                // Glyph carries the state (colour reinforces).
                let (glyph, style) = if h.failed_rev.is_some() {
                    ("!", Style::default().fg(theme::ERROR))
                } else if h.held_rev.is_some() {
                    ("≠", Style::default().fg(theme::WARNING))
                } else if h.offline_rev.is_some() {
                    ("~", Style::default().fg(theme::WARNING))
                } else if h.paused {
                    ("‖", Style::default().fg(theme::MUTED))
                } else if h.deployed_rev.is_some() {
                    ("✓", Style::default().fg(theme::SUCCESS))
                } else {
                    ("·", Style::default().fg(theme::MUTED))
                };
                let mut state = Vec::new();
                if h.paused {
                    state.push("paused".to_string());
                }
                if let (Some(rev), Some(t)) = (&h.deployed_rev, h.deployed_time) {
                    state.push(format!(
                        "deployed {} {}",
                        short_rev(rev),
                        format_unix_ago(t)
                    ));
                }
                if let Some(rev) = &h.failed_rev {
                    state.push(format!("FAILED {}", short_rev(rev)));
                }
                if let Some(rev) = &h.held_rev {
                    state.push(format!(
                        "HELD {} — target differs from repo; d adopts",
                        short_rev(rev)
                    ));
                }
                if let (Some(rev), Some(t)) = (&h.offline_rev, h.offline_time) {
                    state.push(format!(
                        "offline {} — {} pending",
                        format_unix_ago(t),
                        short_rev(rev)
                    ));
                }
                if let Some(u) = &h.unreachable {
                    state.push(format!("unreachable: {u}"));
                }
                if state.is_empty() {
                    state.push("never deployed".to_string());
                }
                let name_style = if is_sel {
                    Style::default()
                        .fg(theme::ON_ACCENT)
                        .bg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph.to_string(), style),
                    Span::raw(" "),
                    Span::styled(h.name.clone(), name_style),
                    Span::styled(
                        format!("  {}", state.join("; ")),
                        Style::default().fg(theme::MUTED),
                    ),
                ]));
            }
            lines.push(Line::raw(""));
        }
    } else if app.agent.loading {
        lines.push(Line::from(Span::styled(
            "fetching agent status…",
            Style::default().fg(theme::BUSY),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "no status yet — r to fetch",
            Style::default().fg(theme::MUTED),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// "in 3m" / "12s ago" for next-poll stamps that can sit either side
/// of now.
fn format_unix_ago_or_in(t: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if t > now {
        let secs = t - now;
        if secs < 60 {
            format!("in {secs}s")
        } else if secs < 3600 {
            format!("in {}m", secs / 60)
        } else {
            format!("in {}h", secs / 3600)
        }
    } else {
        format_unix_ago(t)
    }
}

fn draw_agent_tail(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MUTED))
        .title(Span::styled(
            " agent log (live) ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Tail-window: the last lines that fit, like the job log's tail.
    let viewport = inner.height as usize;
    let start = app.agent.tail.len().saturating_sub(viewport);
    let lines: Vec<Line> = app
        .agent
        .tail
        .iter()
        .skip(start)
        .map(|l| Line::raw(l.clone()))
        .collect();
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "waiting for agent output…",
                Style::default().fg(theme::MUTED),
            ))),
            inner,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ProgressLine;

    fn joined_text(segments: &[StyledSegment]) -> String {
        segments.iter().map(|seg| seg.text.as_str()).collect()
    }

    /// Build a log entry the way the app does when a probe's typed
    /// progress line arrives, so the tests exercise the same
    /// producer→renderer pair the running TUI uses.
    fn entry_from(line: ProgressLine) -> crate::app::LogEntry {
        crate::app::LogEntry {
            text: line.text,
            is_err: false,
            host: Some("h".into()),
            kind: line.kind,
        }
    }

    #[test]
    fn styled_pkg_done_text_matches_the_canonical_line() {
        let entry = entry_from(ProgressLine::pkg_done(0));
        let segments = style_entry(&entry, Style::default(), None);
        assert_eq!(joined_text(&segments), entry.text);
        assert_eq!(entry.text, "[pkg] done (0 change(s))");
    }

    #[test]
    fn styled_pkg_update_text_matches_the_canonical_line() {
        let entry = entry_from(ProgressLine::pkg(PkgChange::Updated {
            name: "usbutils".into(),
            from: "018".into(),
            to: "018, 019, 019-man".into(),
        }));
        let segments = style_entry(&entry, Style::default(), None);
        assert_eq!(joined_text(&segments), entry.text);
        assert_eq!(entry.text, "[pkg] usbutils: 018 → 018, 019, 019-man");
    }

    #[test]
    fn tail_scroll_reports_the_wrapped_top_entry() {
        // 10 entries, each 10 chars wide, in a 5-col / 4-row pane:
        // every entry wraps to 2 rows, so only 2 entries are visible
        // and the top of the pane shows entry 8 — not entry 6, which a
        // row-count model would predict.
        let lines: Vec<Line<'_>> = (0..10).map(|_| Line::raw("aaaaaaaaaa")).collect();
        let mut scroll = 0usize;
        let (y, top_idx) = compute_tail_scroll_offset(&lines, &mut scroll, 5, 4);
        assert_eq!(y, 16);
        assert_eq!(top_idx, 8);
    }

    #[test]
    fn size_remote_line_shows_human_size_and_delta() {
        let entry = entry_from(ProgressLine::size_remote(13886547224));
        let segments = style_entry(&entry, Style::default(), Some(13886874912));
        let text = joined_text(&segments);
        assert!(text.contains("12.9 GiB"));
        assert!(text.contains("delta +320.0 KiB"));
    }
}
