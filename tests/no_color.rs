//! `NO_COLOR` end-to-end guard.
//!
//! Lives in its own test binary on purpose. `theme::monochrome()` caches
//! its answer in a `OnceLock` — correct for a process whose environment
//! cannot change under it, but it means the very first caller in a
//! process fixes the result for every later one. A single test in a
//! private binary is the only place the environment can be set before
//! anything has asked.

use std::collections::BTreeMap;

use deptui::app::App;
use deptui::flake::{Node, Profile};
use deptui::ui;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

fn nodes() -> Vec<Node> {
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "system".to_string(),
        Profile {
            user: None,
            ssh_user: None,
        },
    );
    vec![Node {
        name: "alpha".into(),
        hostname: "alpha.lan".into(),
        ssh_user: None,
        profiles,
        profiles_order: None,
    }]
}

#[test]
fn no_color_strips_every_colour_but_keeps_the_chips_visible() {
    std::env::set_var("NO_COLOR", "1");
    assert!(deptui::theme::monochrome(), "NO_COLOR was not picked up");

    let mut app = App::new(".".into(), nodes());
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal.draw(|f| ui::draw(f, &mut app)).expect("draw");
    let buf = terminal.backend().buffer().clone();

    let mut reversed = 0usize;
    for cell in buf.content() {
        assert_eq!(cell.fg, Color::Reset, "a foreground colour survived");
        assert_eq!(cell.bg, Color::Reset, "a background colour survived");
        if cell.modifier.contains(Modifier::REVERSED) {
            reversed += 1;
        }
    }

    // The title chip and the selected host row both carried a filled
    // background. Losing colour must not lose them: reverse video is what
    // they fall back to.
    assert!(
        reversed > 0,
        "no cell fell back to reverse video — filled chips dissolved into body text",
    );
}
