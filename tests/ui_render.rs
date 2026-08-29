//! Rendering smoke tests.
//!
//! `ui::draw` walks every pane, every popup, and the windowed job log on
//! each frame, and none of it is exercised by the unit tests — a panic or
//! a layout regression in there only shows up in front of the user. These
//! render the real widget tree into ratatui's `TestBackend` and assert on
//! the resulting cell buffer, which is also what guards the ratatui
//! upgrade: the API compiled unchanged, but "compiles" is not "renders".

use std::collections::BTreeMap;

use deptui::app::{App, FocusPane, InputMode, LogEntry, VisualMode, VisualSel};
use deptui::deploy::{Mode, ProfileSel};
use deptui::flake::{Node, Profile};
use deptui::ui;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Append a host-tagged log entry. Written against the public state
/// rather than a test-only setter so the tests exercise the same fields
/// the renderer reads.
fn log_line(app: &mut App, text: &str, host: &str) {
    app.log.push(LogEntry {
        text: text.to_string(),
        is_err: false,
        host: Some(host.to_string()),
    });
}

fn nodes() -> Vec<Node> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "system".to_string(),
        Profile {
            user: None,
            ssh_user: None,
        },
    );
    profiles.insert(
        "home".to_string(),
        Profile {
            user: Some("jd".into()),
            ssh_user: None,
        },
    );
    vec![
        Node {
            name: "alpha".into(),
            hostname: "alpha.lan".into(),
            ssh_user: Some("root".into()),
            profiles: profiles.clone(),
            profiles_order: None,
        },
        Node {
            name: "beta".into(),
            hostname: "beta.lan".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        },
    ]
}

/// Render one frame at `w`x`h` and return the buffer's text content.
fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal.draw(|f| ui::draw(f, app)).expect("draw");
    let buf = terminal.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn renders_the_default_screen() {
    let mut app = App::new(".".into(), nodes());
    let out = render(&mut app, 120, 40);
    assert!(out.contains("deptui"), "{out}");
    assert!(out.contains("alpha"), "host list missing: {out}");
    assert!(out.contains("beta"), "host list missing: {out}");
    assert!(out.contains("job log"), "job log pane missing: {out}");
}

/// The adaptive-height logic in `draw` measures content against column
/// widths; narrow terminals are where it decides to grow the strips.
#[test]
fn renders_at_awkward_terminal_sizes() {
    for (w, h) in [(40, 12), (60, 20), (200, 60), (240, 80)] {
        let mut app = App::new(".".into(), nodes());
        for i in 0..50 {
            log_line(&mut app, &format!("line {i}"), "alpha");
        }
        let out = render(&mut app, w, h);
        assert!(
            !out.is_empty(),
            "empty render at {w}x{h}",
        );
    }
}

/// A terminal smaller than the layout's own minimums must not panic —
/// the user can drag a window to any size at any moment, including while
/// a popup is open.
#[test]
fn survives_a_terminal_too_small_for_the_layout() {
    for (w, h) in [(1, 1), (2, 3), (10, 3), (20, 5), (5, 40)] {
        let mut app = App::new(".".into(), nodes());
        log_line(&mut app, "some output", "alpha");
        let _ = render(&mut app, w, h);

        // Popups do their own centred-rect maths, which is where a tiny
        // area is most likely to underflow.
        let mut app = App::new(".".into(), nodes());
        app.show_help = true;
        let _ = render(&mut app, w, h);

        let mut app = App::new(".".into(), nodes());
        app.input = InputMode::ConfirmQuit {
            deploy_running: true,
        };
        let _ = render(&mut app, w, h);
    }
}

/// Regression guard for the windowed job log: only the visible tail is
/// built, so an off-by-one in the window would drop the newest lines.
#[test]
fn job_log_shows_the_newest_lines_at_the_tail() {
    let mut app = App::new(".".into(), nodes());
    for i in 0..500 {
        log_line(&mut app, &format!("entry-{i:04}"), "alpha");
    }
    let out = render(&mut app, 120, 40);
    assert!(out.contains("entry-0499"), "tail line missing: {out}");
    assert!(
        !out.contains("entry-0000"),
        "the whole buffer should not be on screen",
    );
}

/// Scrolling back must reveal older lines, which is only true if the
/// window widens with the scroll offset.
#[test]
fn job_log_window_follows_the_scroll_offset() {
    let mut app = App::new(".".into(), nodes());
    for i in 0..500 {
        log_line(&mut app, &format!("entry-{i:04}"), "alpha");
    }
    render(&mut app, 120, 40); // establishes the viewport height
    app.job_log_scroll = 200;
    let out = render(&mut app, 120, 40);
    assert!(
        out.contains("entry-0299"),
        "scrolling back 200 entries should reveal entry-0299: {out}",
    );
    assert!(!out.contains("entry-0499"), "tail should be scrolled off");
}

#[test]
fn renders_every_popup_without_panicking() {
    let modes: Vec<(&str, InputMode)> = vec![
        ("overrides", InputMode::OverridesMenu),
        (
            "confirm deploy",
            InputMode::ConfirmDeploy {
                hosts: vec!["alpha".into(), "beta".into()],
                mode: Mode::Switch,
                profile: ProfileSel::All,
            },
        ),
        (
            "confirm quit",
            InputMode::ConfirmQuit {
                deploy_running: true,
            },
        ),
        (
            "identity picker",
            InputMode::EditIdentityPicker {
                entries: vec!["/home/me/.ssh/id_ed25519".into()],
                selected: 0,
                buf: String::new(),
            },
        ),
    ];
    for (label, mode) in modes {
        let mut app = App::new(".".into(), nodes());
        app.input = mode;
        let out = render(&mut app, 120, 40);
        assert!(!out.trim().is_empty(), "{label} rendered nothing");
    }
}

/// The masked password widget must never put the plaintext on screen.
#[test]
fn password_popup_masks_the_buffer() {
    let mut app = App::new(".".into(), nodes());
    app.input = InputMode::PasswordPrompt {
        prompt: "[sudo] password for root:".into(),
        buf: deptui::app::SecretBuf::from("hunter2"),
        source: deptui::app::PromptSource::SudoPre,
    };
    let out = render(&mut app, 120, 40);
    assert!(!out.contains("hunter2"), "password rendered in clear: {out}");
    assert!(out.contains("•••••••"), "mask missing: {out}");
}

#[test]
fn help_popup_renders_and_scrolls() {
    let mut app = App::new(".".into(), nodes());
    app.show_help = true;
    let first = render(&mut app, 120, 40);
    assert!(first.contains("visual selection"), "help body missing: {first}");

    // The cheat sheet is taller than the popup, so the newer keys only
    // appear once it is scrolled. Walk down until they show up — this
    // also exercises the in-place scroll clamping.
    let mut found_c = false;
    let mut found_p = false;
    for _ in 0..40 {
        app.help_scroll = app.help_scroll.saturating_add(4);
        let out = render(&mut app, 120, 40);
        found_c |= out.contains("Shift+C");
        found_p |= out.contains("Shift+P");
        if found_c && found_p {
            break;
        }
    }
    assert!(found_c, "Shift+C never appeared while scrolling the help");
    assert!(found_p, "Shift+P never appeared while scrolling the help");
}

/// The help scroll is clamped in place by the renderer; pushing far past
/// the end must settle rather than scroll into blank space.
#[test]
fn help_popup_scroll_clamps_at_the_bottom() {
    let mut app = App::new(".".into(), nodes());
    app.show_help = true;
    app.help_scroll = u16::MAX;
    let out = render(&mut app, 120, 40);
    assert!(
        out.contains("toggles") || out.contains("badges") || out.contains("deploy"),
        "over-scrolled help rendered nothing useful: {out}",
    );
    assert!(app.help_scroll < u16::MAX, "help_scroll was never clamped");
}

#[test]
fn visual_selection_renders() {
    let mut app = App::new(".".into(), nodes());
    for i in 0..20 {
        log_line(&mut app, &format!("out-{i}"), "alpha");
    }
    app.focus = FocusPane::JobLog;
    app.visual_sel = Some(VisualSel {
        mode: VisualMode::Line,
        anchor: (19, 0),
        cursor: (19, 0),
    });
    let out = render(&mut app, 120, 40);
    assert!(out.contains("VISUAL"), "visual mode chip missing: {out}");
}
