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
        kind: deptui::host::LogKind::Plain,
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
/// widths; the narrow end of the *supported* range is where it decides
/// to grow the strips. Anything below `MIN_WIDTH` x `MIN_HEIGHT` never
/// reaches that code — it gets the resize message instead, which
/// `too_small_terminals_get_a_resize_message_instead_of_a_broken_layout`
/// covers.
#[test]
fn renders_at_awkward_terminal_sizes() {
    for (w, h) in [(80, 24), (86, 30), (200, 60), (240, 80)] {
        let mut app = App::new(".".into(), nodes());
        for i in 0..50 {
            log_line(&mut app, &format!("line {i}"), "alpha");
        }
        let out = render(&mut app, w, h);
        assert!(!out.is_empty(), "empty render at {w}x{h}",);
    }
}

/// A terminal smaller than the layout's own minimums must not panic —
/// the user can drag a window to any size at any moment, including while
/// a popup is open. These sizes take the resize-message path, which is
/// itself rendered into an area of one cell in the worst case.
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
        // Both a roomy terminal and the smallest one that still draws the
        // real layout: `centered_rect` divides the area down, so the
        // minimum size is where a popup's own maths is most likely to
        // collapse to zero rows.
        for (w, h) in [(120, 40), (80, 24)] {
            let mut app = App::new(".".into(), nodes());
            app.input = mode.clone();
            let out = render(&mut app, w, h);
            assert!(
                !out.trim().is_empty(),
                "{label} rendered nothing at {w}x{h}"
            );
        }
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
    assert!(
        !out.contains("hunter2"),
        "password rendered in clear: {out}"
    );
    assert!(out.contains("•••••••"), "mask missing: {out}");
}

#[test]
fn help_popup_renders_and_scrolls() {
    let mut app = App::new(".".into(), nodes());
    app.show_help = true;
    let first = render(&mut app, 120, 40);
    assert!(
        first.contains("visual selection"),
        "help body missing: {first}"
    );

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

/// Below the layout's honest minimum the UI must say so rather than draw
/// five panes whose contents no longer fit inside them. The message has
/// to carry the numbers — "too small" alone leaves the user guessing how
/// much bigger.
#[test]
fn too_small_terminals_get_a_resize_message_instead_of_a_broken_layout() {
    for (w, h) in [(79, 40), (120, 23), (40, 12), (20, 6)] {
        let mut app = App::new(".".into(), nodes());
        log_line(&mut app, "some output", "alpha");
        let out = render(&mut app, w, h);
        assert!(
            out.contains("terminal too small"),
            "no resize message at {w}x{h}: {out}",
        );
        assert!(
            !out.contains("job log"),
            "real layout still drawn at {w}x{h}: {out}",
        );
        // Only the wider cases have room for the second line; the point
        // is that the ones that do actually report both sizes.
        if w >= 40 && h >= 3 {
            assert!(out.contains("80x24"), "required size missing: {out}");
        }
    }
}

/// The exact boundary renders the real thing — an off-by-one here would
/// hide the app from anyone running a stock 80x24 terminal.
#[test]
fn the_minimum_size_renders_the_real_layout() {
    let mut app = App::new(".".into(), nodes());
    let out = render(&mut app, 80, 24);
    assert!(!out.contains("terminal too small"), "gated at 80x24: {out}");
    assert!(out.contains("job log"), "job log pane missing: {out}");
}

/// Reachability is the one host-list column where colour used to be the
/// only signal. Each state needs its own glyph so the list survives
/// `NO_COLOR`, a monochrome terminal, and red/green colour blindness.
#[test]
fn each_reachability_state_has_its_own_glyph() {
    use deptui::host::{HostStatus, Reachability};

    // Assert against the host row itself: the toggles strip draws the
    // very same filled/hollow circles for its on/off dots, so a
    // whole-screen substring search would pass no matter what this
    // column rendered.
    fn host_row(out: &str, host: &str) -> String {
        out.lines()
            .find(|l| l.contains(host) && l.contains("sys:"))
            .unwrap_or_else(|| panic!("no host row for `{host}`: {out}"))
            .to_string()
    }

    for (reach, glyph, others) in [
        (Reachability::Online, "\u{25cf}", ["\u{25cb}", "\u{b7}"]),
        (Reachability::Offline, "\u{25cb}", ["\u{25cf}", "\u{b7}"]),
        (Reachability::Unknown, "\u{b7}", ["\u{25cf}", "\u{25cb}"]),
    ] {
        let mut app = App::new(".".into(), nodes());
        app.status.insert(
            "alpha".to_string(),
            HostStatus {
                reachability: reach,
                ..Default::default()
            },
        );

        let out = render(&mut app, 120, 40);
        let row = host_row(&out, "alpha");
        assert!(row.contains(glyph), "{reach:?} lost its glyph: {row}");
        for other in others {
            assert!(
                !row.contains(other),
                "{reach:?} rendered as `{other}`, which belongs to another state: {row}",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// agent view
// ---------------------------------------------------------------------------

fn agent_status() -> deptui_core::agentwire::AgentStatus {
    use deptui_core::agentwire::*;
    AgentStatus {
        version: "0.1.0".into(),
        paused: false,
        watches: vec![WatchStatus {
            name: "infra".into(),
            repo: "git@example.com:me/infra.git".into(),
            ref_label: "branch main".into(),
            paused: false,
            last_seen: Some("abcdef1234567890".into()),
            next_poll: None,
            running: None,
            hosts: vec![
                HostStatus {
                    name: "alpha".into(),
                    paused: false,
                    deployed_rev: Some("abcdef1234567890".into()),
                    deployed_time: Some(1),
                    failed_rev: None,
                    failed_time: None,
                    failed_message: None,
                    unreachable: None,
                    offline_rev: None,
                    offline_time: None,
                },
                HostStatus {
                    name: "beta".into(),
                    paused: false,
                    deployed_rev: None,
                    deployed_time: None,
                    failed_rev: Some("abcdef1234567890".into()),
                    failed_time: Some(2),
                    failed_message: Some("boom".into()),
                    unreachable: None,
                    offline_rev: None,
                    offline_time: None,
                },
            ],
        }],
    }
}

fn app_with_agent() -> App {
    let settings: deptui::settings::Settings =
        toml::from_str("default_agent = \"homelab\"\n[agents.homelab]\nssh = \"me@box\"\n")
            .expect("settings");
    App::with_settings(".".into(), nodes(), settings)
}

/// The agent view replaces the main layout wholesale; a panic or an
/// overflow in its hand-built line assembly only shows up here.
#[test]
fn agent_view_renders_status_and_tail() {
    let mut app = app_with_agent();
    app.agent.open = true;
    app.agent.status = Some(agent_status());
    app.agent
        .tail
        .push_back("[infra] run #1 (kick): deploying".into());
    let out = render(&mut app, 120, 40);
    assert!(out.contains("agent"), "{out}");
    assert!(out.contains("homelab"), "agent name missing: {out}");
    assert!(out.contains("infra"), "watch missing: {out}");
    assert!(out.contains("branch main"), "ref label missing: {out}");
    assert!(
        out.contains("FAILED abcdef1234"),
        "failed host state missing: {out}"
    );
    assert!(out.contains("run #1"), "tail missing: {out}");
    // The main layout is replaced, not underdrawn.
    assert!(
        !out.contains("job log"),
        "main layout leaked through: {out}"
    );
}

#[test]
fn agent_view_survives_empty_status_and_errors() {
    let mut app = app_with_agent();
    app.agent.open = true;
    let out = render(&mut app, 120, 40);
    assert!(out.contains("no status yet"), "{out}");

    app.agent.error = Some("connection refused".into());
    let out = render(&mut app, 120, 40);
    assert!(out.contains("connection refused"), "{out}");

    // Tiny-but-supported size must not panic.
    app.agent.error = None;
    app.agent.status = Some(agent_status());
    let _ = render(&mut app, 80, 24);
}

/// The host-list badge is part of the user-facing contract: glyph
/// carries the state ([agent] managed, [agent!] failed, [agent~]
/// offline-pending), colour only reinforces.
#[test]
fn managed_hosts_get_agent_badges_and_title_notice() {
    use deptui::app::AgentManaged;
    let mut app = app_with_agent();
    app.agent_managed.insert(
        "alpha".into(),
        AgentManaged {
            failed: false,
            offline: true,
        },
    );
    app.agent_managed.insert(
        "beta".into(),
        AgentManaged {
            failed: true,
            offline: false,
        },
    );
    let out = render(&mut app, 120, 40);
    assert!(out.contains("[agent~]"), "offline badge missing: {out}");
    assert!(out.contains("[agent!]"), "failed badge missing: {out}");
    assert!(
        out.contains("agent: 1 host deploy(s) failed"),
        "title notice missing: {out}"
    );
}

/// Q28(b): the confirm popup warns when the agent manages a host in
/// the batch and offers the one-key pause.
#[test]
fn confirm_popup_warns_about_agent_managed_hosts() {
    use deptui::app::AgentManaged;
    let mut app = app_with_agent();
    app.agent_managed
        .insert("alpha".into(), AgentManaged::default());
    app.input = InputMode::ConfirmDeploy {
        hosts: vec!["alpha".into()],
        mode: Mode::Switch,
        profile: ProfileSel::All,
    };
    let out = render(&mut app, 120, 40);
    assert!(
        out.contains("the agent auto-deploys some of these hosts"),
        "warning missing: {out}"
    );
    assert!(
        out.contains("pause the agent first"),
        "p hint missing: {out}"
    );
}

/// Unconfigured is the first-run state: `a` must show setup guidance,
/// not a dead screen — and a settings parse error must be visible.
#[test]
fn agent_view_explains_missing_and_broken_settings() {
    let mut app = App::new(".".into(), nodes());
    app.agent.open = true;
    let out = render(&mut app, 120, 40);
    assert!(out.contains("no agents configured"), "{out}");
    assert!(
        out.contains("[agents.homelab]"),
        "example snippet missing: {out}"
    );

    app.agent.settings_error = Some("config.toml: unknown field `agnets`".into());
    let out = render(&mut app, 120, 40);
    assert!(out.contains("could not be loaded"), "{out}");
    assert!(out.contains("agnets"), "parse error text missing: {out}");
}

/// The renderer rebuilds the mouse hit-test map every frame; clicks
/// resolve against exactly what was painted.
#[test]
fn draw_populates_the_mouse_map() {
    let mut app = App::new(".".into(), nodes());
    let _ = render(&mut app, 120, 40);
    let hosts = app.mouse.hosts.expect("hosts rect recorded");
    assert!(hosts.height > 0 && hosts.width > 0);
    assert!(app.mouse.job_log.is_some());
    assert!(app.mouse.toggles.is_some());
    assert!(app.mouse.commands.is_some());
    assert!(!app.mouse.toggle_items.is_empty());
    assert!(!app.mouse.command_items.is_empty());
    // Ranges are within the strip's drawable area.
    let toggles = app.mouse.toggles.unwrap();
    let max_end = app
        .mouse
        .toggle_items
        .iter()
        .map(|(_, e, _)| *e)
        .max()
        .unwrap();
    assert!(
        max_end <= (toggles.width as usize) * 2,
        "{max_end} vs {toggles:?}"
    );

    // The agent view replaces the layout — no stale main-screen rects
    // may survive into its frames.
    app.agent.open = true;
    let _ = render(&mut app, 120, 40);
    assert!(app.mouse.hosts.is_none(), "agent view must clear the map");
}

/// Profile selection renders as a set with on/off dots, not "all".
#[test]
fn profile_buttons_show_dots_and_selection_reads_as_a_set() {
    let mut app = App::new(".".into(), nodes());
    let out = render(&mut app, 120, 40);
    assert!(out.contains("s:● sys"), "sys dot on: {out}");
    assert!(out.contains("h:● home"), "home dot on: {out}");
    assert!(out.contains("system+home"), "details shows the set: {out}");
    assert!(!out.contains("switch / all"), "the word all is gone: {out}");

    app.input = InputMode::ConfirmDeploy {
        hosts: vec!["alpha".into()],
        mode: Mode::Switch,
        profile: ProfileSel::System,
    };
    let out = render(&mut app, 120, 40);
    assert!(out.contains("system"), "{out}");
}
