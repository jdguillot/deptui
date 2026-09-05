//! TUI application state and the main event loop.
//!
//! The App owns:
//! - the discovered nodes and their per-node status
//! - the currently selected node + deploy mode + profile selection
//! - a tail-buffered log
//! - any in-flight background work (status checks, deploy run)
//!
//! The loop is a single `tokio::select!` over (a) terminal/tick events,
//! (b) status-check completions, and (c) deploy log lines.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use crate::agentclient;
use crate::askpass::{AskpassEnv, AskpassServer};
use crate::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use crate::event::{spawn as spawn_events, AppEvent};
use crate::flake::Node;
use crate::host::{
    BuildPlan, BuildSite, HostStatus, LogKind, ProfileExtra, Reachability, SubstituterDrift,
    UpdateState,
};
use crate::joblog;
pub use crate::joblog::{LogEntry, VisualMode, VisualSel};
use crate::probe;
use crate::settings::Settings;
use crate::ssh::SshOverride;
use crate::ui::{self, Tui};
use deptui_core::agentwire;

/// Focusable regions of the UI. Each one has its own keyboard
/// affordance when focused: Hosts moves the selection, Toggles lets
/// you flip the deploy-rs flags without hitting 1–5, and Commands
/// exposes every keybind action as a navigable button row. Tab/Shift-Tab cycles forward/back; Shift+H/L also crosses
/// sub-nav boundaries inside Toggles and Commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Toggles,
    Hosts,
    JobLog,
    Commands,
}

impl FocusPane {
    /// Row in the grid layout. 0 = toggles (top), 1 = middle (hosts /
    /// job log), 2 = commands (bottom). Used by the vertical
    /// pane-move keys to decide what "up" and "down" mean.
    pub fn row(self) -> usize {
        match self {
            FocusPane::Toggles => 0,
            FocusPane::Hosts | FocusPane::JobLog => 1,
            FocusPane::Commands => 2,
        }
    }
}

/// Number of toggle cells — derived from the toggle table so the nav
/// bounds check can't drift from what the strip renders.
pub const TOGGLE_COUNT: usize = deploy::TOGGLES.len();

/// Every action that can be bound to a command-pane button. The pane
/// renders each variant as a short label and `activate_command`
/// dispatches by index. The order is the order the buttons appear in
/// the pane; reordering here is how you rearrange the bottom row.
///
/// Note: `?` (help) is intentionally NOT a command button — it lives
/// in the info pane next to the other meta hints (quit, focus, …) so
/// the commands row stays scoped to "things that act on hosts".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Refresh,
    Updates,
    /// Shift+U — closure-size delta + package diff for the selection.
    SizeDiff,
    /// Shift+P — build-plan preflight.
    BuildPlan,
    /// Shift+C — substituter-drift check.
    CacheDrift,
    /// Mark every host / clear all marks. The button's key + label
    /// flip between "A mark all" and "X unmark" with the mark state,
    /// mirroring the long-standing Shift+A / Shift+X bindings.
    MarkAll,
    /// Open the agent view (key `a`).
    Agent,
    ProfileSystem,
    ProfileHome,
    Switch,
    Boot,
    DryRun,
    Cancel,
    Override,
}

/// Single source of truth for the command pane — label + key hint per
/// command. The key column is informational (the real binding lives in
/// `handle_key_normal`); if you rename a binding, update both.
pub const COMMANDS: &[(Command, &str, &str)] = &[
    // Probes first — the row scans in groups: what you check, what you
    // select, what you deploy, then the rest.
    (Command::Refresh, "r", "refresh"),
    (Command::Updates, "u", "updates"),
    (Command::SizeDiff, "U", "size"),
    (Command::BuildPlan, "P", "plan"),
    (Command::CacheDrift, "C", "drift"),
    // The stored key/label are the unmarked spelling; `ui::command_hint`
    // swaps in ("X", "unmark") while any host is marked. The real
    // bindings are the Shift+A / Shift+X arms in `handle_key_normal`.
    (Command::MarkAll, "A", "mark all"),
    (Command::ProfileSystem, "s", "sys"),
    (Command::ProfileHome, "h", "home"),
    (Command::Switch, "S", "switch"),
    (Command::Boot, "B", "boot"),
    (Command::DryRun, "D", "dry"),
    (Command::Cancel, "x", "cancel"),
    (Command::Override, "o", "ssh"),
    (Command::Agent, "a", "agent"),
];

/// Which override field the user is currently editing. Drives both the
/// prompt label and where the parsed buffer gets stored on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideField {
    Hostname,
    User,
    Identity,
    Opts,
}

impl OverrideField {
    pub fn label(self) -> &'static str {
        match self {
            OverrideField::Hostname => "hostname / IP",
            OverrideField::User => "ssh user",
            OverrideField::Identity => "identity file",
            OverrideField::Opts => "extra ssh opts",
        }
    }
}

/// A password being typed into the prompt widget.
///
/// A plain `String` here leaked three ways, all of which matter given the
/// lengths the rest of this file goes to (mlock, zeroize, core dumps
/// disabled):
///
/// 1. `String::push` reallocates as it grows and frees the old buffer
///    without wiping it, scattering plaintext prefixes across the heap.
///    We reserve up front, and grow by hand so the old allocation is
///    zeroed before it is released.
/// 2. Dropping it left the bytes in freed memory. `Zeroizing` wipes them.
/// 3. `InputMode` derives `Debug`, so one stray `tracing::debug!` of the
///    input state would have written the plaintext to `--log-file`. The
///    `Debug` impl below redacts.
#[derive(Clone, Default)]
pub struct SecretBuf(Zeroizing<String>);

/// Reserved capacity. Comfortably longer than any real password, so the
/// grow path below is a safety net rather than a routine occurrence.
const SECRET_BUF_CAPACITY: usize = 512;

impl SecretBuf {
    pub fn new() -> Self {
        Self(Zeroizing::new(String::with_capacity(SECRET_BUF_CAPACITY)))
    }

    pub fn push(&mut self, c: char) {
        if self.0.len() + c.len_utf8() > self.0.capacity() {
            // Grow explicitly: `String::push` would copy into a new
            // allocation and free the old one with the password still in
            // it. Moving the old buffer into a `Zeroizing` wipes it on
            // drop instead.
            let mut bigger =
                String::with_capacity((self.0.capacity() * 2).max(SECRET_BUF_CAPACITY));
            bigger.push_str(&self.0);
            let old = std::mem::replace(&mut *self.0, bigger);
            drop(Zeroizing::new(old));
        }
        self.0.push(c);
    }

    pub fn pop(&mut self) {
        self.0.pop();
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Number of characters, for the masked `•` rendering.
    pub fn char_count(&self) -> usize {
        self.0.chars().count()
    }

    /// Take the inner buffer. Still `Zeroizing`, so whatever the caller
    /// does with it, it is wiped when they drop it.
    pub fn into_inner(self) -> Zeroizing<String> {
        self.0
    }
}

impl From<&str> for SecretBuf {
    fn from(s: &str) -> Self {
        let mut buf = Self::new();
        for c in s.chars() {
            buf.push(c);
        }
        buf
    }
}

/// Comparison against a plain `&str`, for tests and assertions. Not
/// constant-time; never use it to check a password against a secret.
impl PartialEq<str> for SecretBuf {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SecretBuf {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Debug for SecretBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBuf(<redacted, {} chars>)", self.char_count())
    }
}

/// Top-level input mode. The vast majority of the time we're in `Normal`;
/// when the user opens an override prompt or the overrides menu we route
/// keys differently.
#[derive(Debug, Clone)]
pub enum InputMode {
    Normal,
    /// User pressed `o` and is picking which field to edit (or `c` to
    /// clear). Single-key sub-menu.
    OverridesMenu,
    /// User is typing into a single-line text buffer for `field`.
    EditOverride {
        field: OverrideField,
        buf: String,
    },
    /// Picking an SSH identity file. The user can either pick one of the
    /// scanned `entries` with Ctrl+J/K or type a custom path into `buf`.
    /// `entries` may be empty if `~/.ssh` couldn't be read or had no
    /// candidate keys; the buffer is the source of truth on save.
    EditIdentityPicker {
        entries: Vec<PathBuf>,
        selected: usize,
        buf: String,
    },
    /// Confirmation popup for `s`/`b`/`d`. The popup snapshots which
    /// hosts will be hit and how, so the user can review (and bail) on
    /// `n`/`Esc` before any side effects happen.
    ConfirmDeploy {
        hosts: Vec<String>,
        mode: Mode,
        profile: ProfileSel,
    },
    /// Quit confirmation popup. Shown when the user presses `q` or
    /// `Ctrl+C`. `deploy_running` is true when a deploy is in flight so
    /// the popup can warn that it will be killed.
    ConfirmQuit {
        deploy_running: bool,
    },
    /// User pressed `/` while one of the log panes was focused and is
    /// typing a search query. Enter commits (`App.log_search` set,
    /// jumps to the nearest match), Esc cancels (search cleared).
    /// While in this mode `n`/`Shift+N` are still typed into the buf —
    /// they only become "next match" / "previous match" after Enter.
    SearchLog {
        buf: String,
    },
    /// User pressed `/` while the help popup was open and is typing a
    /// filter. Lazygit-style: lines that don't contain the buf are
    /// hidden as the user types. Enter commits the filter, Esc clears
    /// it. The popup stays open the whole time.
    SearchHelp {
        buf: String,
    },
    /// A deploy child (or SSH) is waiting for a password. The TUI
    /// renders a masked input widget. The password is NEVER written to
    /// the log buffer. Enter sends it via the appropriate channel
    /// (askpass socket or child stdin); Esc dismisses the prompt.
    PasswordPrompt {
        /// Raw prompt text, e.g. `[sudo] password for root: ` or
        /// `Enter passphrase for key '…': `.
        prompt: String,
        /// Password being typed. Rendered as `•` characters, wiped on
        /// drop, and redacted in `Debug` — see [`SecretBuf`].
        buf: SecretBuf,
        /// Where to send the password on Enter.
        source: PromptSource,
    },
}

/// Distinguishes whether a password prompt came from the SSH_ASKPASS
/// mechanism (routed through [`DeployHandle::askpass_tx`]), from a
/// sudo prompt detected on stderr (routed through
/// [`DeployHandle::stdin_tx`]), or from a pre-deploy prompt asked
/// before spawning when `--interactive-sudo` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSource {
    /// SSH password / passphrase via SSH_ASKPASS → respond through
    /// `askpass_password_tx`.
    Askpass,
    /// Remote sudo prompt detected on stderr → respond through
    /// `deploy_stdin_tx`. Retained for completeness but rarely fires
    /// in practice because `--interactive-sudo` reads via `/dev/tty`
    /// (now a PTY) rather than stderr.
    Sudo,
    /// Pre-deploy sudo prompt collected BEFORE the child spawns.
    /// On submit, the password is cached AND passed to
    /// [`deploy::run`] so it can be pre-written into the child's
    /// controlling-tty PTY.
    SudoPre,
}

/// What we remember about the most recently completed deploy. Rendered
/// in the title bar and the details summary so the user can tell at a
/// glance that a deploy actually finished (instead of staring at a
/// quiet log and wondering whether magic-rollback ate it).
#[derive(Debug, Clone)]
pub struct LastDeploy {
    pub node: String,
    pub mode: Mode,
    pub profile: ProfileSel,
    pub exit_code: i32,
    pub ok: bool,
}

/// Everything the agent view needs, plus the ambient state (badges,
/// failure notice) the main screen shows even when the view is closed.
pub struct AgentUi {
    /// Full-screen agent view open? Not an [`InputMode`]: password
    /// prompts and quit confirmation must still overlay it.
    pub open: bool,
    /// `(name, ssh)` pairs from settings; default agent first.
    pub agents: Vec<(String, String)>,
    /// Index into `agents` of the one currently shown.
    pub current: usize,
    pub status: Option<agentwire::AgentStatus>,
    pub error: Option<String>,
    pub loading: bool,
    /// Selected row in the flattened watch/host listing.
    pub sel: usize,
    /// Live tail from `deptui-agent tail`, capped like the job log.
    pub tail: VecDeque<String>,
    tail_task: Option<JoinHandle<()>>,
    /// Last op ack, shown in the view's footer.
    pub last_op: Option<String>,
    /// Settings-file load error, surfaced in the view's empty state.
    pub settings_error: Option<String>,
    /// A deploy-node scan for agents is in flight.
    pub scanning: bool,
    /// At least one scan has completed (distinguishes "none found"
    /// from "haven't looked yet" in the empty state).
    pub scanned: bool,
}

impl AgentUi {
    fn new(settings: &Settings) -> Self {
        Self {
            open: false,
            agents: settings.agent_list(),
            current: 0,
            status: None,
            error: None,
            loading: false,
            sel: 0,
            tail: VecDeque::new(),
            tail_task: None,
            last_op: None,
            settings_error: settings.load_error.clone(),
            scanning: false,
            scanned: false,
        }
    }

    pub fn current_agent(&self) -> Option<(&str, &str)> {
        self.agents
            .get(self.current)
            .map(|(n, s)| (n.as_str(), s.as_str()))
    }

    /// Flattened `(watch index, host index)` rows of the status listing,
    /// the order the view renders and `sel` indexes.
    pub fn host_rows(&self) -> Vec<(usize, usize)> {
        let Some(status) = &self.status else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (wi, w) in status.watches.iter().enumerate() {
            for hi in 0..w.hosts.len() {
                rows.push((wi, hi));
            }
        }
        rows
    }
}

/// What the host list needs to know about a host the agent manages.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentManaged {
    pub failed: bool,
    pub offline: bool,
}

/// Hit-test map for mouse input, rebuilt by `ui::draw` every frame so
/// clicks resolve against exactly what is on screen. Rects are the
/// *inner* (border-less) areas. Strip items are linear column ranges
/// (row-major over the inner rect, so a wrapped strip still resolves).
#[derive(Debug, Default, Clone)]
pub struct MouseMap {
    pub toggles: Option<Rect>,
    pub hosts: Option<Rect>,
    pub job_log: Option<Rect>,
    pub commands: Option<Rect>,
    /// `(start, end, toggle index)` linear ranges inside `toggles`.
    pub toggle_items: Vec<(usize, usize, usize)>,
    /// `(start, end, COMMANDS index)` linear ranges inside `commands`.
    pub command_items: Vec<(usize, usize, usize)>,
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Resolve a click inside `inner` to a strip item via its linear
/// position. Wrap-aware by construction: `Wrap` lays the spans out
/// row-major, which is exactly how the linear position walks.
fn linear_hit(items: &[(usize, usize, usize)], inner: Rect, x: u16, y: u16) -> Option<usize> {
    if !rect_contains(inner, x, y) {
        return None;
    }
    let pos = (y - inner.y) as usize * inner.width as usize + (x - inner.x) as usize;
    items
        .iter()
        .find(|(start, end, _)| (*start..*end).contains(&pos))
        .map(|(_, _, idx)| *idx)
}

/// Background work updates we receive over the status channel.
#[derive(Debug)]
enum StatusUpdate {
    /// A background probe reported progress or its final result.
    Probe(probe::Report),
    /// Re-discovered flake nodes from the last `r` refresh. Merges new
    /// nodes into the running list without disturbing existing state.
    FlakeDiscover(Vec<Node>),
    /// Agent status fetched over ssh (agent name, result).
    AgentStatus(String, Result<agentwire::AgentStatus, String>),
    /// Ack (or error) from a mutating agent verb.
    AgentOp(Result<String, String>),
    /// One live log line from `deptui-agent tail`.
    AgentTail(String),
    /// Deploy-node scan finished: `(node name, ssh target)` for every
    /// node that answered `deptui-agent status`.
    AgentsDiscovered(Vec<(String, String)>),
}

impl From<probe::Report> for StatusUpdate {
    fn from(report: probe::Report) -> Self {
        StatusUpdate::Probe(report)
    }
}

/// Everything that exists only while a deploy batch is running: the
/// in-flight child's plumbing plus the queue of hosts behind it. Held
/// as one `Option<DeploySession>` on [`App`] so "is a deploy running"
/// is a single question and teardown cannot forget a field — dropping
/// the session drops the state. Every teardown path (exit, spawn
/// error, cancel, quit) takes the session and reads what it needs off
/// the taken value.
struct DeploySession {
    /// Log lines from the running child.
    rx: mpsc::Receiver<LogLine>,
    /// The task driving the child. Detached (not aborted) on cancel so
    /// its process-group teardown can finish — see `cancel_deploy`.
    task: JoinHandle<()>,
    /// Tears down the deploy's whole process group. Aborting `task`
    /// only reaches the direct `deploy` child, leaving the `nix`
    /// builders and `ssh` it forked running.
    cancel: Option<deploy::DeployCanceller>,
    /// When the deploy was started with `--interactive-sudo`, lets the
    /// TUI write the sudo password to the child's piped stdin.
    stdin_tx: Option<mpsc::Sender<String>>,
    /// The host currently being deployed. Not in `queue`.
    current: String,
    /// Hosts still waiting after the current one finishes.
    queue: VecDeque<String>,
    /// Parameters the user confirmed for the whole batch, applied to
    /// every host in it.
    mode: Mode,
    profile: ProfileSel,
    /// Progress: `total` stays fixed while the queue drains.
    total: usize,
    done: usize,
}

pub struct App {
    pub flake: String,
    pub nodes: Vec<Node>,
    pub status: HashMap<String, HostStatus>,
    /// Per-node SSH overrides keyed by node name. Empty unless the user
    /// explicitly sets something.
    pub overrides: HashMap<String, SshOverride>,

    pub selected: usize,
    /// Multi-selection for batch deploy. Insertion-ordered so the queue
    /// runs in the order the user clicked them. Empty means "operate on
    /// the highlighted host only" — the existing single-host behaviour.
    pub marked: Vec<String>,
    pub focus: FocusPane,
    /// Cursor inside the toggles pane when focused. `0..TOGGLE_COUNT`.
    /// Stays stable when focus leaves so returning to the pane lands in
    /// the same place the user left it.
    pub toggle_index: usize,
    /// Cursor inside the commands pane when focused. `0..COMMANDS.len()`.
    pub command_index: usize,
    pub mode: Mode,
    pub profile_sel: ProfileSel,
    pub toggles: Toggles,

    pub log: Vec<LogEntry>,
    pub busy_label: Option<String>,
    /// Committed log search query. `Some(q)` means a search has been
    /// committed via Enter from `SearchLog` and `n`/`Shift+N` will jump
    /// between matches. `None` means no search is active and matching
    /// lines aren't highlighted. Cleared by Esc in the prompt or by
    /// committing an empty query.
    pub log_search: Option<String>,
    /// 1-based index of the "active" match occurrence across the entire
    /// targeted pane. `n` increments, `N` decrements, wrapping at the
    /// edges. The rendering pass highlights this occurrence in cyan
    /// while all other matches stay magenta. Reset to 0 (no active
    /// match) when the search is cleared.
    pub log_search_match_idx: usize,
    /// Committed help-popup filter. `Some(q)` hides every help line
    /// that doesn't contain the substring; `None` shows everything.
    /// Lives outside InputMode because the help popup is its own modal
    /// layer that sits *over* the InputMode dispatcher.
    pub help_search: Option<String>,
    /// Most-recent finished deploy across the whole session. Drives the
    /// title-bar chip so the user can tell at a glance what the last
    /// thing they ran was, regardless of which host they're inspecting.
    pub last_deploy: Option<LastDeploy>,
    /// Per-host outcome of the most-recent deploy that touched each
    /// host. Drives the details-pane "last" chip so navigating between
    /// hosts shows the right history per host instead of bleeding the
    /// global last-deploy onto every selection.
    pub last_deploys: HashMap<String, LastDeploy>,
    /// Lines from the bottom of the job log the user has scrolled up.
    /// `0` means "auto-tail" (always show the latest line).
    pub job_log_scroll: usize,
    /// Entries-from-tail offset of the topmost *visible* job-log
    /// entry, published by `draw_job_log` each frame and consumed by
    /// `visual_move_cursor` for vim-style edge scrolling. This is NOT
    /// `job_log_scroll + rows - 1`: visible lines wrap, so fewer
    /// entries fit than the pane has rows — an edge check against the
    /// row count let the cursor walk one entry past the visible top
    /// per wrapped row before scrolling started.
    pub job_log_top_offset: usize,
    /// Active visual selection in the job log (`v` / `V`). `None` when not in
    /// visual mode. Indices are into `filtered_log_indices_for_job_log()`.
    pub visual_sel: Option<VisualSel>,
    pub show_help: bool,
    /// Vertical scroll position of the help popup. 0 = top; bumped by
    /// arrow keys / j/k while the popup is open so the help works on
    /// small terminals where the full cheat sheet would overflow.
    pub help_scroll: u16,
    pub input: InputMode,
    /// Monotonic counter incremented on every tick. The UI uses it to pick
    /// a spinner frame so in-flight work animates without us tracking time
    /// explicitly per host.
    pub tick_counter: u64,

    /// Channel that background tasks publish status updates on.
    status_tx: mpsc::Sender<StatusUpdate>,
    status_rx: mpsc::Receiver<StatusUpdate>,

    /// The running deploy batch, if any. See [`DeploySession`].
    deploy: Option<DeploySession>,
    /// Extra `nix build` arguments from `--build-arg`, forwarded to
    /// deploy-rs after `--`.
    pub extra_build_args: Vec<String>,

    /// App-level askpass environment: script and socket paths, cloned
    /// into every task that spawns SSH.
    askpass_env: AskpassEnv,
    /// Send passwords to the askpass server (which relays them to the
    /// SSH_ASKPASS helper over the Unix socket).
    askpass_password_tx: mpsc::Sender<Zeroizing<String>>,
    /// Receives prompt text from the askpass server — polled in the
    /// main `select!` loop.
    askpass_prompt_rx: mpsc::Receiver<String>,
    /// Keep the server's background task alive. `None` before `run()`.
    _askpass_task: Option<JoinHandle<()>>,
    /// Cached password for the current deploy action. Auto-replayed on
    /// subsequent prompts within the same action, then securely zeroed
    /// when the action ends (exit, cancel, or new action start).
    /// Never written to disk or logs.
    cached_password: Option<Zeroizing<String>>,
    /// Stashed deploy parameters while the SudoPre password prompt is
    /// on screen. Consumed by [`handle_key_password_prompt`] on Enter
    /// (actually starts the deploy) or cleared on Esc (cancels).
    pending_deploy: Option<(Vec<String>, Mode, ProfileSel)>,

    /// Background probe tasks (update / closure-size / package-diff
    /// checks). Held so `x` can abort them mid-flight; finished
    /// handles are pruned opportunistically each time we spawn a new
    /// one. The aborted tasks' Commands run with `kill_on_drop(true)`
    /// inside `host.rs` so the underlying nix/ssh children are
    /// reaped, not orphaned.
    probe_tasks: Vec<JoinHandle<()>>,
    /// Mouse hit-test map, rebuilt each frame by the renderer.
    pub mouse: MouseMap,
    /// Agent view state + the ambient managed-host map for badges.
    pub agent: AgentUi,
    /// Node name → what the agent last reported about it. Drives the
    /// host-list badge and the info-row failure notice.
    pub agent_managed: HashMap<String, AgentManaged>,
    /// True once we receive a quit request.
    should_quit: bool,
}

impl App {
    /// Construct with default (empty) settings — no disk reads, so
    /// tests stay hermetic. The binary calls [`Self::with_settings`]
    /// with [`Settings::load`].
    pub fn new(flake: String, nodes: Vec<Node>) -> Self {
        Self::with_settings(flake, nodes, Settings::default())
    }

    pub fn with_settings(flake: String, nodes: Vec<Node>, settings: Settings) -> Self {
        let (status_tx, status_rx) = mpsc::channel(64);
        let mut status = HashMap::new();
        for n in &nodes {
            status.insert(n.name.clone(), HostStatus::default());
        }

        // Askpass channels are created now (cheap); the actual server
        // is started in `run()` which has a tokio runtime. Until then
        // `askpass_env` holds a dummy value — it's overwritten before
        // any SSH commands are spawned.
        let (askpass_password_tx, _placeholder_rx) = mpsc::channel::<Zeroizing<String>>(4);
        let (_placeholder_tx, askpass_prompt_rx) = mpsc::channel::<String>(4);

        Self {
            flake,
            nodes,
            status,
            overrides: HashMap::new(),
            selected: 0,
            marked: Vec::new(),
            focus: FocusPane::Hosts,
            toggle_index: 0,
            command_index: 0,
            mode: Mode::Switch,
            profile_sel: ProfileSel::All,
            toggles: Toggles::default(),
            log: Vec::new(),
            busy_label: None,
            log_search: None,
            log_search_match_idx: 0,
            help_search: None,
            last_deploy: None,
            last_deploys: HashMap::new(),
            job_log_scroll: 0,
            job_log_top_offset: 0,
            visual_sel: None,
            show_help: false,
            help_scroll: 0,
            input: InputMode::Normal,
            tick_counter: 0,
            status_tx,
            status_rx,
            deploy: None,
            extra_build_args: Vec::new(),
            askpass_env: AskpassEnv {
                script_path: "/dev/null".into(),
                socket_path: "/dev/null".into(),
            },
            askpass_password_tx,
            askpass_prompt_rx,
            _askpass_task: None,
            cached_password: None,
            pending_deploy: None,
            probe_tasks: Vec::new(),
            mouse: MouseMap::default(),
            agent: AgentUi::new(&settings),
            agent_managed: HashMap::new(),
            should_quit: false,
        }
    }

    /// Cache a password in memory, locking its pages to prevent swapping.
    fn set_cached_password(&mut self, password: &str) {
        self.clear_cached_password();
        let pw = Zeroizing::new(password.to_string());
        // Best-effort: lock the heap buffer into RAM so it can't be swapped
        // to disk. Failure (e.g. low RLIMIT_MEMLOCK) is non-fatal.
        unsafe {
            libc::mlock(pw.as_ptr() as *const libc::c_void, pw.len());
        }
        self.cached_password = Some(pw);
    }

    /// Clear the cached password, unlocking and zeroing memory.
    fn clear_cached_password(&mut self) {
        if let Some(ref pw) = self.cached_password {
            unsafe {
                libc::munlock(pw.as_ptr() as *const libc::c_void, pw.len());
            }
        }
        self.cached_password = None; // Zeroizing zeros the buffer on drop
    }

    /// True if `name` is in the multi-select set.
    pub fn is_marked(&self, name: &str) -> bool {
        self.marked.iter().any(|n| n == name)
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.nodes.get(self.selected)
    }

    pub fn status_for(&self, name: &str) -> HostStatus {
        self.status.get(name).cloned().unwrap_or_default()
    }

    /// Borrow the SSH override for a node. Returns a reference to a
    /// shared default-empty override when nothing is set, so callers
    /// don't need to handle `Option`.
    pub fn override_for(&self, name: &str) -> &SshOverride {
        // A `'static` empty override avoids returning a temporary.
        static EMPTY: std::sync::OnceLock<SshOverride> = std::sync::OnceLock::new();
        self.overrides
            .get(name)
            .unwrap_or_else(|| EMPTY.get_or_init(SshOverride::default))
    }

    fn override_mut(&mut self, name: &str) -> &mut SshOverride {
        self.overrides.entry(name.to_string()).or_default()
    }

    /// Returns true when background work is in flight (spinners are
    /// animating), meaning tick-driven redraws are needed.
    fn has_inflight_work(&self) -> bool {
        if self.deploy.is_some() {
            return true;
        }
        if self.probe_tasks.iter().any(|h| !h.is_finished()) {
            return true;
        }
        false
    }

    pub async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        // Start the app-level SSH_ASKPASS server now that we have a
        // tokio runtime. Every SSH-spawning operation (status probes,
        // deploys) will route password prompts through this server.
        let askpass_server = AskpassServer::new().context("setting up SSH_ASKPASS")?;
        self.askpass_env = askpass_server.env.clone();
        let (askpass_prompt_tx, askpass_prompt_rx) = mpsc::channel::<String>(4);
        let (askpass_password_tx, askpass_password_rx) = mpsc::channel::<Zeroizing<String>>(4);
        self.askpass_password_tx = askpass_password_tx;
        self.askpass_prompt_rx = askpass_prompt_rx;
        self._askpass_task = Some(tokio::spawn(async move {
            askpass_server
                .serve(askpass_prompt_tx, askpass_password_rx)
                .await;
        }));

        let mut events = spawn_events();

        // Ambient agent badges: one status fetch at startup when any
        // agent is configured (the view itself fetches again on open).
        if !self.agent.agents.is_empty() {
            self.fetch_agent_status();
        }

        // Kick off an initial reachability sweep so the first frame isn't
        // all "unknown".
        self.refresh_reachability();

        ui::render(terminal, self)?;
        let mut last_draw = std::time::Instant::now();

        while !self.should_quit {
            let needs_redraw;
            // Deploy output is the one source that can arrive faster than
            // we can paint. Its redraws are rate-limited; everything else
            // (keystrokes above all) still paints immediately.
            let mut rate_limited = false;
            tokio::select! {
                biased;

                ev = events.recv() => {
                    match ev {
                        Some(ev) => {
                            // Ticks only need a redraw when something is
                            // animating (spinners). Otherwise skip the
                            // expensive draw pass.
                            needs_redraw =
                                !matches!(ev, AppEvent::Tick) || self.has_inflight_work();
                            self.handle_event(ev);
                        }
                        // The terminal event stream ended — the terminal
                        // went away underneath us. There is nobody left to
                        // draw for, and `status_rx` never closes, so
                        // without this the loop would wait forever on a
                        // dead session.
                        None => {
                            self.should_quit = true;
                            needs_redraw = false;
                        }
                    }
                }

                Some(update) = self.status_rx.recv() => {
                    needs_redraw = true;
                    self.apply_status(update);
                }

                Some(line) = recv_deploy(&mut self.deploy) => {
                    self.handle_deploy_line(line);
                    // Drain whatever else is already queued before painting.
                    // `nix` can emit output far faster than a full-screen
                    // ratatui pass completes; one draw per line makes the
                    // renderer the bottleneck, the bounded channel fills,
                    // and back-pressure stalls the child's pipe — which is
                    // what the log "just stopping" mid-deploy looks like.
                    let mut drained = 0usize;
                    while drained < MAX_LOG_DRAIN {
                        let Some(rx) = self.deploy.as_mut().map(|s| &mut s.rx) else {
                            // An `Exit` line cleared the receiver (and may
                            // have started the next host in the batch).
                            break;
                        };
                        match rx.try_recv() {
                            Ok(next) => {
                                self.handle_deploy_line(next);
                                drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    needs_redraw = true;
                    // While a deploy is in flight the 120ms tick guarantees
                    // a follow-up paint, so a skipped frame is never a
                    // dropped one. Once it finishes there are no more ticks
                    // to rely on, so that frame has to go out now.
                    rate_limited = self.deploy.is_some();
                }

                Some(prompt) = self.askpass_prompt_rx.recv() => {
                    if let Some(ref pw) = self.cached_password {
                        let _ = self
                            .askpass_password_tx
                            .try_send(Zeroizing::new(pw.to_string()));
                        needs_redraw = false;
                    } else {
                        needs_redraw = true;
                        self.input = InputMode::PasswordPrompt {
                            prompt,
                            buf: SecretBuf::new(),
                            source: PromptSource::Askpass,
                        };
                    }
                }
            }

            if needs_redraw && !(rate_limited && last_draw.elapsed() < MIN_FRAME_INTERVAL) {
                ui::render(terminal, self)?;
                last_draw = std::time::Instant::now();
            }
        }

        // Tear down any running deploy before we leave. Same reasoning as
        // `cancel_deploy`: signal the process group and give the task a
        // moment to reap it, otherwise quitting the TUI would leave a
        // detached `nix` build running.
        if let Some(mut session) = self.deploy.take() {
            match session.cancel.take() {
                Some(c) => {
                    c.cancel();
                    // Teardown takes at least `CANCEL_GRACE`, so say what
                    // we're waiting on instead of freezing on the last
                    // frame.
                    self.busy_label = Some("stopping deploy…".to_string());
                    let _ = ui::render(terminal, self);
                    // Bounded so a wedged child can't strand the terminal
                    // in the alternate screen.
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(5), session.task).await;
                }
                None => session.task.abort(),
            }
        }

        Ok(())
    }

    // ---------- event handling ----------

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => self.tick_counter = self.tick_counter.wrapping_add(1),
            AppEvent::Term(CtEvent::Key(key)) => self.handle_key(key),
            AppEvent::Term(CtEvent::Mouse(me)) => self.handle_mouse(me),
            AppEvent::Term(_) => {}
        }
    }

    /// Mouse input. Wheel scrolls whichever pane is under the pointer
    /// (job log, host list, help popup); left click focuses panes,
    /// selects host rows, flips toggles, and presses command buttons.
    /// Everything routes through the same handlers the keyboard uses —
    /// the mouse adds no new abilities, only new reach.
    fn handle_mouse(&mut self, me: MouseEvent) {
        // The help popup is modal: the wheel scrolls it, nothing else
        // reacts underneath.
        if self.show_help {
            match me.kind {
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.help_scroll = self.help_scroll.saturating_add(3),
                _ => {}
            }
            return;
        }
        // Modal inputs (confirm popups, password prompts) and the agent
        // view are keyboard-only for now; a stray click must not
        // confirm or cancel anything.
        if self.agent.open || !matches!(self.input, InputMode::Normal) {
            return;
        }
        let (x, y) = (me.column, me.row);
        let map = self.mouse.clone();
        match me.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                if map.job_log.is_some_and(|r| rect_contains(r, x, y)) {
                    // Wheel-up scrolls back in history, matching `k`.
                    self.scroll_job_log(if up { 3 } else { -3 });
                } else if map.hosts.is_some_and(|r| rect_contains(r, x, y)) {
                    self.move_selection(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = map
                    .toggles
                    .and_then(|r| linear_hit(&map.toggle_items, r, x, y))
                {
                    self.focus = FocusPane::Toggles;
                    self.toggle_index = i;
                    self.activate_toggle(i);
                } else if let Some(i) = map
                    .commands
                    .and_then(|r| linear_hit(&map.command_items, r, x, y))
                {
                    if self.command_is_visible(i) {
                        self.focus = FocusPane::Commands;
                        self.command_index = i;
                        self.activate_command(i);
                    }
                } else if let Some(r) = map.hosts.filter(|r| rect_contains(*r, x, y)) {
                    self.focus = FocusPane::Hosts;
                    let idx = (y - r.y) as usize;
                    if idx < self.nodes.len() {
                        self.selected = idx;
                    }
                } else if map.job_log.is_some_and(|r| rect_contains(r, x, y)) {
                    self.focus = FocusPane::JobLog;
                } else if map.toggles.is_some_and(|r| rect_contains(r, x, y)) {
                    self.focus = FocusPane::Toggles;
                } else if map.commands.is_some_and(|r| rect_contains(r, x, y)) {
                    self.focus = FocusPane::Commands;
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Ctrl-C shows the quit confirmation (same as `q`). If we're already
        // showing it, Ctrl-C confirms immediately.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if matches!(self.input, InputMode::ConfirmQuit { .. }) {
                self.should_quit = true;
            } else {
                self.input = InputMode::ConfirmQuit {
                    deploy_running: self.deploy.is_some(),
                };
            }
            return;
        }

        // The help popup is modal: ?/Esc/Enter/q close it, and j/k/arrow
        // keys scroll so the cheat sheet stays usable on small terminals
        // where the full content can't fit in the popup at once.
        //
        // While the help popup is open AND a `SearchHelp` prompt is
        // active we must NOT consume the keystrokes here — they need to
        // reach the InputMode dispatch path so the search-prompt handler
        // can append to the buffer. Same logic applies if a help search
        // has already been committed: `/` would re-open the prompt and
        // typing letters mustn't be eaten by the j/k scroll fall-through.
        if self.show_help && !matches!(self.input, InputMode::SearchHelp { .. }) {
            match key.code {
                // `/` opens the lazygit-style filter prompt. We hand
                // off to the InputMode dispatch by transitioning into
                // SearchHelp here and falling through.
                KeyCode::Char('/') => {
                    self.input = InputMode::SearchHelp { buf: String::new() };
                    return;
                }
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.show_help = false;
                    // Reset so the next `?` lands at the top again.
                    self.help_scroll = 0;
                    // Closing the popup also drops any committed
                    // help filter so reopening starts clean.
                    self.help_search = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(5);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(5);
                }
                KeyCode::Home => self.help_scroll = 0,
                // Vim-style "g" → top of the popup, "G" → bottom.
                // The renderer clamps `u16::MAX` against the rendered
                // content height in-place.
                KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::Char('G') => self.help_scroll = u16::MAX,
                _ => {}
            }
            return;
        }

        // Route by current input mode.
        // The agent view owns Normal-mode keys while open. Modal
        // popups (password prompt, quit confirm) still take priority
        // because their InputMode is not Normal and falls through to
        // the dispatch below.
        if self.agent.open && matches!(self.input, InputMode::Normal) {
            self.handle_key_agent(key);
            return;
        }

        match std::mem::replace(&mut self.input, InputMode::Normal) {
            InputMode::Normal => {
                self.input = InputMode::Normal;
                self.handle_key_normal(key);
            }
            InputMode::OverridesMenu => self.handle_key_overrides_menu(key),
            InputMode::EditOverride { field, buf } => {
                self.handle_key_edit_override(key, field, buf);
            }
            InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            } => {
                self.handle_key_identity_picker(key, entries, selected, buf);
            }
            InputMode::ConfirmDeploy {
                hosts,
                mode,
                profile,
            } => {
                self.handle_key_confirm_deploy(key, hosts, mode, profile);
            }
            InputMode::ConfirmQuit { deploy_running } => {
                self.handle_key_confirm_quit(key, deploy_running);
            }
            InputMode::SearchLog { buf } => {
                self.handle_key_search_log(key, buf);
            }
            InputMode::SearchHelp { buf } => {
                self.handle_key_search_help(key, buf);
            }
            InputMode::PasswordPrompt {
                prompt,
                buf,
                source,
            } => {
                self.handle_key_password_prompt(key, prompt, buf, source);
            }
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent) {
        // Treat "uppercase letter" as shift-held even if the modifier
        // bit isn't set — some terminals report Char('H') without
        // SHIFT, others report Char('h')+SHIFT. Accepting both keeps
        // the bindings consistent regardless of terminal quirks.
        let shift = key.modifiers.contains(KeyModifiers::SHIFT)
            || matches!(key.code, KeyCode::Char(c) if c.is_ascii_uppercase());

        // ---- pane-navigation layer (vim-style) ----
        //
        // Shifted keys always move focus between panes (never within).
        //   horizontal (row 2 only): Shift+H/L, Shift+Left/Right
        //   vertical (between rows): Shift+J/K, Shift+Up/Down
        //
        // h/l mean "left/right" exactly like vim, and j/k mean
        // "down/up". The earlier version swapped these and confused
        // anyone with vim muscle memory.
        if shift {
            match key.code {
                // Horizontal pane move (h = left, l = right).
                KeyCode::Char('H')
                | KeyCode::Char('h')
                | KeyCode::Char('L')
                | KeyCode::Char('l')
                | KeyCode::Left
                | KeyCode::Right => {
                    let left = matches!(
                        key.code,
                        KeyCode::Char('H') | KeyCode::Char('h') | KeyCode::Left
                    );
                    self.pane_move_horizontal(if left { -1 } else { 1 });
                    return;
                }
                // Vertical pane move (j = down, k = up).
                KeyCode::Char('J')
                | KeyCode::Char('j')
                | KeyCode::Char('K')
                | KeyCode::Char('k')
                | KeyCode::Up
                | KeyCode::Down => {
                    let up = matches!(
                        key.code,
                        KeyCode::Char('K') | KeyCode::Char('k') | KeyCode::Up
                    );
                    self.pane_move_vertical(if up { -1 } else { 1 });
                    return;
                }
                // Shift+A / Shift+X: batch mark/unmark. Global.
                KeyCode::Char('A') => {
                    self.mark_all();
                    return;
                }
                KeyCode::Char('X') => {
                    self.clear_marks();
                    return;
                }
                // Shift+U: medium-tier update details (closure size
                // delta). Requires a prior `u` to have populated the
                // cached paths; `refresh_sizes_for_selected` logs a
                // hint if not.
                KeyCode::Char('U') => {
                    self.refresh_sizes_for_selected();
                    return;
                }
                // Shift+C: substituter-drift check — does this deploy add
                // a binary cache that its own build can't use?
                KeyCode::Char('C') => {
                    self.refresh_cache_drift_for_selected();
                    return;
                }
                // Shift+P: build-plan preflight — what will this deploy
                // compile, and how much will it download?
                KeyCode::Char('P') => {
                    self.refresh_build_plan_for_selected();
                    return;
                }
                // Shift+G: vim-style "go to end" — snap the focused
                // scroll pane back to its tail (auto-follow). Useful
                // after the user has scrolled up to read history and
                // wants to resume tailing the live log.
                KeyCode::Char('G') => {
                    self.snap_to_tail();
                    return;
                }
                _ => {}
            }
        }

        // ---- global keys (any focus, unshifted) ----
        match key.code {
            KeyCode::Tab => {
                self.focus_next();
                return;
            }
            KeyCode::BackTab => {
                self.focus_prev();
                return;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
                return;
            }
            KeyCode::Char('q') => {
                self.input = InputMode::ConfirmQuit {
                    deploy_running: self.deploy.is_some(),
                };
                return;
            }
            // Esc was an accidental quit before — now it just no-ops
            // in Normal mode so a stray escape doesn't kill the TUI.
            // Modal handlers (override / confirm / identity picker)
            // still consume Esc to back out themselves. If visual
            // selection is active, Esc clears it first.
            KeyCode::Esc => {
                // One step back per press, like vim: cancel the selection
                // first, and only clear the search once nothing is
                // selected. Doing both at once made a stray Esc during a
                // selection also lose the query the user was following.
                if self.visual_sel.is_some() {
                    self.visual_sel = None;
                } else if self.log_search.is_some() {
                    self.clear_log_search();
                }
                return;
            }
            // Vim-style "g" → scroll/jump to the top of whatever the
            // focused pane is showing. This used to be a direct-jump
            // to the Hosts pane; it got repurposed because `gg`/`G`
            // for top/bottom is more useful on the log panes and the
            // user reaches Hosts via Tab/Shift+H anyway. `G` snaps
            // to the tail (handled in the shift block above).
            KeyCode::Char('g') => {
                self.jump_to_top();
                return;
            }
            // `v` / `V` start a job-log selection from *any* pane.
            // They used to be reachable only once the job log already
            // had focus, so the obvious gesture right after a deploy —
            // focus is still on Hosts — silently did nothing. Focusing
            // the pane here also means an empty log shows its "no
            // output for this host" hint instead of just ignoring the
            // keypress.
            KeyCode::Char('v') => {
                self.focus = FocusPane::JobLog;
                self.enter_visual_mode(VisualMode::Char);
                return;
            }
            KeyCode::Char('V') => {
                self.focus = FocusPane::JobLog;
                self.enter_visual_mode(VisualMode::Line);
                return;
            }
            // btop-style direct pane jumps. Picked letters that don't
            // collide with anything else: `f` = focus hosts (the
            // obvious `h` is taken by the home-profile shortcut and
            // `n` is taken by search-next), `p` = pipeline (job) log,
            // `t` = toggles, `c` = commands.
            KeyCode::Char('f') => {
                self.focus = FocusPane::Hosts;
                return;
            }
            KeyCode::Char('p') => {
                self.focus = FocusPane::JobLog;
                return;
            }
            KeyCode::Char('t') => {
                self.focus = FocusPane::Toggles;
                return;
            }
            KeyCode::Char('c') => {
                self.focus = FocusPane::Commands;
                return;
            }
            _ => {}
        }

        // ---- per-pane within-pane actions ----
        //
        // Unshifted arrows + j/k/h/l stay within the focused pane.
        // Toggles and Commands accept h/l as vim-style sub-cursor
        // motion (left/right); the row-2 panes use j/k for scroll
        // but leave h/l alone so they fall through to the global
        // action keys below (e.g. `h` = home profile).

        // `/` opens the job-log search from any pane, matching how vim
        // and lazygit make search always reachable.
        if key.code == KeyCode::Char('/') && !shift {
            self.input = InputMode::SearchLog { buf: String::new() };
            return;
        }

        // `n`/`N` jump between search matches from any pane — the search
        // is global so navigating results should be too.
        if self.log_search.is_some() {
            match key.code {
                KeyCode::Char('n') if !shift => {
                    self.search_job_log_jump(1);
                    return;
                }
                KeyCode::Char('N') => {
                    self.search_job_log_jump(-1);
                    return;
                }
                _ => {}
            }
        }

        match self.focus {
            FocusPane::Hosts => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_selection(1);
                    return;
                }
                KeyCode::Char(' ') => {
                    self.toggle_mark_selected();
                    return;
                }
                _ => {}
            },
            FocusPane::JobLog => {
                // --- visual mode intercept ---
                if self.visual_sel.is_some() {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.visual_move_cursor(-1, 0);
                            return;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.visual_move_cursor(1, 0);
                            return;
                        }
                        KeyCode::Left | KeyCode::Char('h')
                            if matches!(
                                self.visual_sel.as_ref().map(|s| s.mode),
                                Some(VisualMode::Char)
                            ) =>
                        {
                            self.visual_move_cursor(0, -1);
                            return;
                        }
                        KeyCode::Right | KeyCode::Char('l')
                            if matches!(
                                self.visual_sel.as_ref().map(|s| s.mode),
                                Some(VisualMode::Char)
                            ) =>
                        {
                            self.visual_move_cursor(0, 1);
                            return;
                        }
                        KeyCode::Char('y') => {
                            self.yank_visual();
                            return;
                        }
                        // Esc is handled by the global early-exit above.
                        // Any other key exits visual mode and falls through.
                        _ => {
                            self.visual_sel = None;
                        }
                    }
                }

                match key.code {
                    // Enter char-visual mode.
                    KeyCode::Char('v') => {
                        self.enter_visual_mode(VisualMode::Char);
                        return;
                    }
                    // Enter line-visual mode.
                    KeyCode::Char('V') => {
                        self.enter_visual_mode(VisualMode::Line);
                        return;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.scroll_job_log(1);
                        return;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.scroll_job_log(-1);
                        return;
                    }
                    _ => {}
                }
            }
            FocusPane::Toggles => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.move_toggle_index(-1);
                    return;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.move_toggle_index(1);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.activate_toggle(self.toggle_index);
                    return;
                }
                _ => {}
            },
            FocusPane::Commands => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.move_command_index(-1);
                    return;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.move_command_index(1);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.activate_command(self.command_index);
                    return;
                }
                _ => {}
            },
        }

        // ---- global unshifted action keys ----
        //
        // These fire from any focus. The pane-jump block above has
        // already consumed g/i/v/t/c, and the per-pane block above
        // consumed h/l when Toggles/Commands are focused — so there
        // are no remaining collisions here.
        match key.code {
            KeyCode::Char('r') => self.refresh_reachability(),
            KeyCode::Char('u') => self.refresh_updates_for_selected(),

            // Profile selection: `s` and `h` are independent toggles
            // (system on/off, home on/off). Both on = deploy-rs's
            // "all profiles"; toggling the last one off is refused so
            // a deploy always targets something. This freed `a`, which
            // now opens the agent view.
            KeyCode::Char('a') => self.open_agent_view(),
            KeyCode::Char('s') => self.toggle_profile_system(),
            KeyCode::Char('h') => self.toggle_profile_home(),

            // Deploy modes — Shift versions so the lowercase letters are free
            // for profile selection above. Boot (Shift+B) is blocked when only
            // the home-manager profile is selected (home-manager has no boot).
            KeyCode::Char('S') => self.request_deploy(Mode::Switch),
            KeyCode::Char('B') => self.request_deploy(Mode::Boot),
            KeyCode::Char('D') => self.request_deploy(Mode::DryRun),
            KeyCode::Char('x') => self.cancel_deploy(),

            // Toggles by direct number key.
            KeyCode::Char('1') => self.activate_toggle(0),
            KeyCode::Char('2') => self.activate_toggle(1),
            KeyCode::Char('3') => self.activate_toggle(2),
            KeyCode::Char('4') => self.activate_toggle(3),
            KeyCode::Char('5') => self.activate_toggle(4),

            // Overrides menu.
            KeyCode::Char('o') => self.input = InputMode::OverridesMenu,

            _ => {}
        }
    }

    /// Advance focus in reading order: Toggles → Hosts → JobLog →
    /// Commands → Toggles. Tab uses this; Shift+Tab uses
    /// [`focus_prev`]. The details pane is display-only and is not a
    /// focus stop.
    fn focus_next(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Hosts,
            FocusPane::Hosts => FocusPane::JobLog,
            FocusPane::JobLog => FocusPane::Commands,
            FocusPane::Commands => FocusPane::Toggles,
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Commands,
            FocusPane::Hosts => FocusPane::Toggles,
            FocusPane::JobLog => FocusPane::Hosts,
            FocusPane::Commands => FocusPane::JobLog,
        };
    }

    /// Horizontal pane move. With the new two-column layout:
    ///   Left column:  Hosts (top) | Details (bottom)
    ///   Right column: JobLog (full height)
    ///
    /// Shift+L from Hosts moves to JobLog.
    /// Shift+H from JobLog moves back to Hosts.
    /// Clamped at both ends (no wrap) so stray Shift+L at the right
    /// edge doesn't teleport back to the host list.
    fn pane_move_horizontal(&mut self, delta: i32) {
        self.focus = match (self.focus, delta) {
            (FocusPane::Hosts, 1) => FocusPane::JobLog,
            (FocusPane::JobLog, -1) => FocusPane::Hosts,
            _ => self.focus, // already at edge or not in the middle row
        };
    }

    /// Vertical pane move.
    ///
    ///   Shift+J:  Toggles → Hosts → Commands
    ///             JobLog  → Commands
    ///   Shift+K:  Commands → JobLog
    ///             Hosts    → Toggles
    ///
    /// Clamped at the top and bottom edges.
    fn pane_move_vertical(&mut self, delta: i32) {
        self.focus = match (self.focus, delta) {
            // Down
            (FocusPane::Toggles, 1) => FocusPane::Hosts,
            (FocusPane::Hosts, 1) | (FocusPane::JobLog, 1) => FocusPane::Commands,
            // Up
            (FocusPane::Commands, -1) => FocusPane::JobLog,
            (FocusPane::Hosts, -1) | (FocusPane::JobLog, -1) => FocusPane::Toggles,
            // Edges / no-ops
            _ => self.focus,
        };
    }

    fn move_toggle_index(&mut self, delta: i32) {
        let len = TOGGLE_COUNT as i32;
        self.toggle_index = ((self.toggle_index as i32 + delta).rem_euclid(len)) as usize;
    }

    /// Returns `true` if the command at `idx` in `COMMANDS` should be
    /// rendered and reachable given the current app state.
    pub fn command_is_visible(&self, idx: usize) -> bool {
        match COMMANDS.get(idx).map(|(c, _, _)| c) {
            Some(Command::Boot) => self.profile_sel != ProfileSel::Home,
            _ => true,
        }
    }

    fn move_command_index(&mut self, delta: i32) {
        if COMMANDS.is_empty() {
            return;
        }
        let len = COMMANDS.len() as i32;
        let mut next = ((self.command_index as i32 + delta).rem_euclid(len)) as usize;
        // Skip invisible commands (e.g. Boot when home-only). Guard against
        // infinite loop in case all commands somehow become invisible.
        for _ in 0..COMMANDS.len() {
            if self.command_is_visible(next) {
                break;
            }
            next = ((next as i32 + delta.signum()).rem_euclid(len)) as usize;
        }
        self.command_index = next;
    }

    /// Flip the toggle at `idx`. `idx` is expected to be `0..TOGGLE_COUNT`
    /// — out-of-range input is ignored so callers don't have to bounds
    /// check themselves. Kept in one place so both direct-number keys
    /// (`1-5`) and Enter-on-focus go through identical logic.
    fn activate_toggle(&mut self, idx: usize) {
        let Some(def) = deploy::TOGGLES.get(idx) else {
            return;
        };
        let on = (def.toggle)(&mut self.toggles);
        self.log_toggle(def.name, on);
        if on {
            if let Some(hint) = def.on_hint {
                self.push_log(hint, false);
            }
        }
    }

    /// Dispatch a command-pane button. This is the single source of
    /// truth for what each command does; the direct-key shortcuts above
    /// call the same underlying helpers.
    fn activate_command(&mut self, idx: usize) {
        let Some((cmd, _, _)) = COMMANDS.get(idx).copied() else {
            return;
        };
        match cmd {
            Command::Refresh => self.refresh_reachability(),
            Command::Updates => self.refresh_updates_for_selected(),
            Command::SizeDiff => self.refresh_sizes_for_selected(),
            Command::BuildPlan => self.refresh_build_plan_for_selected(),
            Command::CacheDrift => self.refresh_cache_drift_for_selected(),
            Command::MarkAll => {
                if self.marked.is_empty() {
                    self.mark_all();
                } else {
                    self.clear_marks();
                }
            }
            Command::Agent => self.open_agent_view(),
            Command::ProfileSystem => self.toggle_profile_system(),
            Command::ProfileHome => self.toggle_profile_home(),
            Command::Switch => self.request_deploy(Mode::Switch),
            Command::Boot => self.request_deploy(Mode::Boot),
            Command::DryRun => self.request_deploy(Mode::DryRun),
            Command::Cancel => self.cancel_deploy(),
            Command::Override => self.input = InputMode::OverridesMenu,
        }
    }

    fn handle_key_overrides_menu(&mut self, key: KeyEvent) {
        let Some(node) = self.selected_node().cloned() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            KeyCode::Char('h') => self.begin_edit_override(OverrideField::Hostname, &node),
            KeyCode::Char('u') => self.begin_edit_override(OverrideField::User, &node),
            KeyCode::Char('k') => self.begin_edit_override(OverrideField::Identity, &node),
            KeyCode::Char('o') => self.begin_edit_override(OverrideField::Opts, &node),
            KeyCode::Char('c') => {
                self.overrides.remove(&node.name);
                self.push_log_tagged(
                    format!("→ cleared SSH overrides for {}", node.name).as_str(),
                    false,
                    Some(node.name.clone()),
                );
                self.input = InputMode::Normal;
            }
            _ => {
                // Unknown sub-key — stay in the menu so the user can try again.
                self.input = InputMode::OverridesMenu;
            }
        }
    }

    fn begin_edit_override(&mut self, field: OverrideField, node: &Node) {
        // Pre-fill the buffer with the current value so the user can edit
        // rather than retype.
        let current = self.override_for(&node.name);
        let buf = match field {
            OverrideField::Hostname => current.hostname.clone().unwrap_or_default(),
            OverrideField::User => current.user.clone().unwrap_or_default(),
            OverrideField::Identity => current
                .identity
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            OverrideField::Opts => current.extra_opts.clone().unwrap_or_default(),
        };
        // Identity gets a richer modal: scan `~/.ssh` for candidate keys
        // so the user can scroll-and-pick instead of remembering paths.
        // The buf is still authoritative on save, so a typed custom path
        // wins over the highlighted entry.
        if field == OverrideField::Identity {
            let entries = scan_ssh_keys();
            // If the pre-filled buf matches one of the scanned entries,
            // start with that entry highlighted.
            let selected = entries
                .iter()
                .position(|p| p.display().to_string() == buf)
                .unwrap_or(0);
            self.input = InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            };
            return;
        }
        self.input = InputMode::EditOverride { field, buf };
    }

    fn handle_key_edit_override(&mut self, key: KeyEvent, field: OverrideField, mut buf: String) {
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            KeyCode::Enter => {
                let Some(node_name) = self.selected_node().map(|n| n.name.clone()) else {
                    self.input = InputMode::Normal;
                    return;
                };
                let trimmed = buf.trim().to_string();
                let entry = self.override_mut(&node_name);
                let value: Option<String> = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                match field {
                    OverrideField::Hostname => entry.hostname = value.clone(),
                    OverrideField::User => entry.user = value.clone(),
                    OverrideField::Identity => entry.identity = value.clone().map(PathBuf::from),
                    OverrideField::Opts => entry.extra_opts = value.clone(),
                }
                let active = entry.is_active();
                if !active {
                    // Cleaning every field clears the entry entirely so
                    // the indicator and `override_for` agree.
                    self.overrides.remove(&node_name);
                }
                self.push_log_tagged(
                    format!(
                        "→ set {} for {}: {}",
                        field.label(),
                        node_name,
                        value.as_deref().unwrap_or("(cleared)")
                    )
                    .as_str(),
                    false,
                    Some(node_name.clone()),
                );
                self.input = InputMode::Normal;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::EditOverride { field, buf };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.input = InputMode::EditOverride { field, buf };
            }
            _ => {
                self.input = InputMode::EditOverride { field, buf };
            }
        }
    }

    fn handle_key_identity_picker(
        &mut self,
        key: KeyEvent,
        entries: Vec<PathBuf>,
        mut selected: usize,
        mut buf: String,
    ) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+J / Ctrl+K (and bare Up/Down for ergonomics) navigate the
        // scanned key list. Moving the highlight syncs `buf` so Enter
        // saves the highlighted path with no extra step. Plain typing
        // overrides the buffer freely so a custom path always wins.
        let nav_down = (ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J')))
            || matches!(key.code, KeyCode::Down);
        let nav_up = (ctrl && matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K')))
            || matches!(key.code, KeyCode::Up);
        if !entries.is_empty() && (nav_down || nav_up) {
            let len = entries.len() as i32;
            let delta: i32 = if nav_down { 1 } else { -1 };
            selected = ((selected as i32 + delta).rem_euclid(len)) as usize;
            buf = entries[selected].display().to_string();
            self.input = InputMode::EditIdentityPicker {
                entries,
                selected,
                buf,
            };
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            KeyCode::Enter => {
                let Some(node_name) = self.selected_node().map(|n| n.name.clone()) else {
                    self.input = InputMode::Normal;
                    return;
                };
                let trimmed = buf.trim().to_string();
                let value: Option<String> = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                let entry = self.override_mut(&node_name);
                entry.identity = value.clone().map(PathBuf::from);
                let active = entry.is_active();
                if !active {
                    self.overrides.remove(&node_name);
                }
                self.push_log_tagged(
                    format!(
                        "→ set identity file for {}: {}",
                        node_name,
                        value.as_deref().unwrap_or("(cleared)")
                    )
                    .as_str(),
                    false,
                    Some(node_name.clone()),
                );
                self.input = InputMode::Normal;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
            _ => {
                self.input = InputMode::EditIdentityPicker {
                    entries,
                    selected,
                    buf,
                };
            }
        }
    }

    fn handle_key_confirm_deploy(
        &mut self,
        key: KeyEvent,
        hosts: Vec<String>,
        mode: Mode,
        profile: ProfileSel,
    ) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.input = InputMode::Normal;
                // Interactive sudo needs the password BEFORE the child
                // spawns (deploy-rs's `rpassword::prompt_password`
                // reads from /dev/tty, which we back with a PTY that
                // we pre-feed). If there's already a cached password
                // from an earlier prompt in this session we reuse it;
                // otherwise pop the pre-prompt widget and stash the
                // deploy parameters until Enter is pressed.
                if self.toggles.interactive_sudo && self.cached_password.is_none() {
                    let first_host = hosts.first().cloned().unwrap_or_else(|| "host".into());
                    self.pending_deploy = Some((hosts, mode, profile));
                    self.input = InputMode::PasswordPrompt {
                        prompt: format!("sudo password for {first_host}: "),
                        buf: SecretBuf::new(),
                        source: PromptSource::SudoPre,
                    };
                } else {
                    self.run_confirmed(hosts, mode, profile);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.input = InputMode::Normal;
                self.push_log("• deploy cancelled at confirmation", false);
            }
            // Q28(b): deploying over the agent's shoulder is the one
            // genuinely confusing race — offer a one-key pause right
            // in the confirmation. The popup stays open; y/n still
            // decide the deploy itself.
            KeyCode::Char('p') if self.agent_manages_any(&hosts) => {
                self.agent_op(vec!["pause".into()]);
                self.push_log("[agent] pause requested before manual deploy", false);
                self.input = InputMode::ConfirmDeploy {
                    hosts,
                    mode,
                    profile,
                };
            }
            _ => {
                // Re-arm the modal so unrelated keystrokes don't dismiss
                // it accidentally — only y/n/Enter/Esc resolve.
                self.input = InputMode::ConfirmDeploy {
                    hosts,
                    mode,
                    profile,
                };
            }
        }
    }

    fn handle_key_confirm_quit(&mut self, key: KeyEvent, deploy_running: bool) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.should_quit = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.input = InputMode::Normal;
            }
            _ => {
                self.input = InputMode::ConfirmQuit { deploy_running };
            }
        }
    }

    /// Handle a keystroke while the user is typing a `/` search query
    /// for one of the log panes. Enter commits, Esc cancels (clearing
    /// any prior committed search), Backspace edits, every other
    /// printable char appends to the buffer.
    fn handle_key_search_log(&mut self, key: KeyEvent, mut buf: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // Cancel: drop the buffer AND any previously-committed
                // query so the highlights vanish. Most-explicit way
                // for the user to "turn search off entirely".
                self.input = InputMode::Normal;
                self.log_search = None;
            }
            KeyCode::Enter => {
                self.input = InputMode::Normal;
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    // Committing empty == clearing.
                    self.log_search = None;
                    return;
                }
                self.log_search = Some(trimmed);
                // Jump to the first match nearest the tail (newest).
                self.job_log_scroll = 0;
                self.search_job_log_jump_initial();
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::SearchLog { buf };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                self.input = InputMode::SearchLog { buf };
            }
            _ => {
                self.input = InputMode::SearchLog { buf };
            }
        }
    }

    /// Same contract as [`handle_key_search_log`] but for the help
    /// popup filter. Lazygit-style: every keystroke updates the live
    /// filter, Enter commits (drops the typing UI but keeps the
    /// filter), Esc clears.
    fn handle_key_search_help(&mut self, key: KeyEvent, mut buf: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.input = InputMode::Normal;
                self.help_search = None;
            }
            KeyCode::Enter => {
                self.input = InputMode::Normal;
                let trimmed = buf.trim().to_string();
                self.help_search = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            KeyCode::Backspace => {
                buf.pop();
                self.help_search = if buf.is_empty() {
                    None
                } else {
                    Some(buf.clone())
                };
                self.input = InputMode::SearchHelp { buf };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                // Live filter — every keystroke updates the visible
                // line set so the user sees results as they type.
                self.help_search = Some(buf.clone());
                self.input = InputMode::SearchHelp { buf };
            }
            _ => {
                self.input = InputMode::SearchHelp { buf };
            }
        }
    }

    /// Handle keystrokes while the password prompt widget is active.
    ///
    /// The password buffer is kept inside `InputMode::PasswordPrompt` and
    /// is NEVER written to the log. On Enter, it is moved into `try_send`
    /// and immediately dropped; on Esc, it is dropped without being used.
    ///
    /// ### Password memory safety
    /// The `buf` String is moved (not copied) into `try_send`; after the
    /// move the local binding is gone. The channel then moves it into the
    /// writer task, which moves it into `write_all` and then drops it.
    /// At each step there is at most one live copy in memory.
    fn handle_key_password_prompt(
        &mut self,
        key: KeyEvent,
        prompt: String,
        mut buf: SecretBuf,
        source: PromptSource,
    ) {
        match key.code {
            KeyCode::Enter => {
                // Cache the password for replay within this action.
                self.set_cached_password(buf.as_str());
                // Route the password to the appropriate destination.
                let secret = buf.into_inner();
                match source {
                    PromptSource::Askpass => {
                        let _ = self.askpass_password_tx.try_send(secret);
                        self.input = InputMode::Normal;
                    }
                    PromptSource::Sudo => {
                        if let Some(tx) = self.deploy.as_ref().and_then(|s| s.stdin_tx.as_ref()) {
                            let _ = tx.try_send(secret.to_string());
                        } else {
                            self.push_log("! no stdin channel available for sudo password", true);
                        }
                        self.input = InputMode::Normal;
                    }
                    PromptSource::SudoPre => {
                        // Cached above; now actually start the deploy.
                        // The cached password will be pulled into the
                        // DeployRequest inside `start_next_in_queue`.
                        // The typed buffer isn't needed beyond the cache.
                        drop(secret);
                        self.input = InputMode::Normal;
                        if let Some((hosts, mode, profile)) = self.pending_deploy.take() {
                            self.run_confirmed(hosts, mode, profile);
                        }
                    }
                }
            }
            KeyCode::Esc => {
                // Clear cache so next prompt asks again.
                self.clear_cached_password();
                match source {
                    PromptSource::SudoPre => {
                        // User bailed before the deploy ran.
                        self.pending_deploy = None;
                        self.push_log("• deploy cancelled — sudo password not provided", false);
                    }
                    PromptSource::Askpass => {
                        // The askpass server is blocked on this reply and
                        // handles one dialog at a time. Dismissing without
                        // answering used to wedge it permanently: the ssh
                        // child waited on the helper, the helper waited on
                        // the server, and every later prompt in the session
                        // was dead. An empty password lets ssh fail auth
                        // and the deploy report a real error.
                        let _ = self
                            .askpass_password_tx
                            .try_send(Zeroizing::new(String::new()));
                        self.push_log(
                            "• password prompt dismissed — authentication will fail for this step",
                            true,
                        );
                    }
                    PromptSource::Sudo => {
                        self.push_log(
                            "• password prompt dismissed — deploy may stall (press x to cancel)",
                            true,
                        );
                    }
                }
                self.input = InputMode::Normal;
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::PasswordPrompt {
                    prompt,
                    buf,
                    source,
                };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.input = InputMode::PasswordPrompt {
                    prompt,
                    buf,
                    source,
                };
            }
            _ => {
                self.input = InputMode::PasswordPrompt {
                    prompt,
                    buf,
                    source,
                };
            }
        }
    }

    fn search_job_log_jump(&mut self, direction: i32) {
        self.advance_match(direction);
    }

    /// First-jump variant: set the active match to the last occurrence
    /// (nearest the tail) and scroll to it. Used right after commit so
    /// the cursor lands on something visible.
    fn search_job_log_jump_initial(&mut self) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let total = self.count_all_matches(query);
        self.log_search_match_idx = total;
        self.scroll_to_match();
    }

    /// Increment (direction=+1) or decrement (direction=-1) the active
    /// match index, wrapping at the edges, then scroll so the line
    /// containing the match is visible.
    fn advance_match(&mut self, direction: i32) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let total = self.count_all_matches(query);
        if total == 0 {
            return;
        }
        // Wrap: going past total → 1, going below 1 → total.
        let cur = self.log_search_match_idx as i32 + direction;
        self.log_search_match_idx = if cur < 1 {
            total
        } else if cur > total as i32 {
            1
        } else {
            cur as usize
        };
        self.scroll_to_match();
    }

    /// Scroll the targeted pane so the line containing the current
    /// active match (by `log_search_match_idx`) is visible. Finds the
    /// Nth occurrence by walking filtered entries and counting per-line
    /// hits.
    fn scroll_to_match(&mut self) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let filtered = self.filtered_log_indices_for_job_log();
        if filtered.is_empty() {
            return;
        }
        let mut seen = 0usize;
        for (i, &idx) in filtered.iter().enumerate() {
            let hits = self.log[idx].text.matches(query).count();
            if hits > 0 && seen + hits >= self.log_search_match_idx {
                // This filtered entry contains the active match.
                // Convert filtered-entry index to scroll offset:
                // scroll == 0 ↔ tail, scroll == len-1 ↔ top.
                let scroll = filtered.len().saturating_sub(1).saturating_sub(i);
                self.job_log_scroll = scroll;
                return;
            }
            seen += hits;
        }
    }

    /// Drop the committed log search. Leaves the scroll positions
    /// alone so the user stays where they were when they pressed Esc.
    fn clear_log_search(&mut self) {
        self.log_search = None;
        self.log_search_match_idx = 0;
    }

    /// Return `(current, total)` for the committed log search.
    /// `current` is `log_search_match_idx` (1-based); `total`
    /// is the count of every individual occurrence of the query across
    /// all filtered lines (a single line with two hits counts twice).
    pub fn log_search_stats(&self) -> (usize, usize) {
        let Some(query) = self.log_search.as_ref() else {
            return (0, 0);
        };
        let total = self.count_all_matches(query);
        (self.log_search_match_idx, total)
    }

    /// Total number of individual query occurrences in the job log.
    fn count_all_matches(&self, query: &str) -> usize {
        let filtered = self.filtered_log_indices_for_job_log();
        let mut total = 0usize;
        for &idx in &filtered {
            total += self.log[idx].text.matches(query).count();
        }
        total
    }

    /// Indices into `self.log` that the job-log pane currently shows.
    /// One implementation, shared with the renderer — see
    /// [`joblog::filtered_indices`].
    pub fn filtered_log_indices_for_job_log(&self) -> Vec<usize> {
        joblog::filtered_indices(
            &self.log,
            &self.marked,
            self.selected_node().map(|n| n.name.as_str()),
        )
    }

    fn log_toggle(&mut self, name: &str, value: bool) {
        let state = if value { "on" } else { "off" };
        self.push_log(format!("• {name} = {state}").as_str(), false);
    }

    fn toggle_mark_selected(&mut self) {
        let Some(name) = self.selected_node().map(|n| n.name.clone()) else {
            return;
        };
        if let Some(idx) = self.marked.iter().position(|n| n == &name) {
            self.marked.remove(idx);
            self.push_log_tagged(
                format!("• unmarked {name}").as_str(),
                false,
                Some(name.clone()),
            );
        } else {
            self.marked.push(name.clone());
            self.push_log_tagged(
                format!("• marked {name}").as_str(),
                false,
                Some(name.clone()),
            );
        }
    }

    fn mark_all(&mut self) {
        self.marked = self.nodes.iter().map(|n| n.name.clone()).collect();
        self.push_log(
            format!("• marked all ({})", self.marked.len()).as_str(),
            false,
        );
    }

    fn clear_marks(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let n = self.marked.len();
        self.marked.clear();
        self.push_log(format!("• cleared {n} marked").as_str(), false);
    }

    fn move_selection(&mut self, delta: i32) {
        if self.nodes.is_empty() {
            return;
        }
        let len = self.nodes.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Same contract as [`scroll_log`] but for the job log pane. The
    /// job log only shows the active host set's entries, so we clamp
    /// against that filtered count.
    fn scroll_job_log(&mut self, delta: i32) {
        let cur = self.job_log_scroll as i32;
        let next = (cur + delta).max(0) as usize;
        let visible = self.filtered_log_indices_for_job_log().len();
        self.job_log_scroll = next.min(visible.saturating_sub(1));
    }

    /// Enter visual mode (`v` = char, `V` = line) at the bottom-most visible
    /// line. Clears any active search so highlight colours don't clash.
    fn enter_visual_mode(&mut self, mode: VisualMode) {
        let filtered = self.filtered_log_indices_for_job_log();
        if filtered.is_empty() {
            return;
        }
        // Start at the bottom visible line (tail minus current scroll offset).
        let line = filtered
            .len()
            .saturating_sub(1)
            .saturating_sub(self.job_log_scroll);
        self.visual_sel = Some(VisualSel {
            mode,
            anchor: (line, 0),
            cursor: (line, 0),
        });
    }

    /// Move the visual cursor by `line_delta` lines and `col_delta` columns.
    /// Updates `job_log_scroll` to keep the cursor line visible at the tail
    /// of the viewport.
    fn visual_move_cursor(&mut self, line_delta: i32, col_delta: i32) {
        let filtered = self.filtered_log_indices_for_job_log();
        if filtered.is_empty() {
            return;
        }
        let sel = match self.visual_sel.take() {
            Some(s) => s,
            None => return,
        };
        let max_line = filtered.len().saturating_sub(1);
        let new_line = (sel.cursor.0 as i32 + line_delta)
            .max(0)
            .min(max_line as i32) as usize;

        // Clamp col to actual char count of the new line.
        let new_col = if sel.mode == VisualMode::Char {
            let log_idx = filtered[new_line];
            let char_count = self.log[log_idx].text.chars().count();
            if col_delta != 0 {
                // Horizontal move — advance/retreat within line.
                (sel.cursor.1 as i32 + col_delta)
                    .max(0)
                    .min(char_count.saturating_sub(1) as i32) as usize
            } else {
                // Vertical move — preserve col but clamp to new line length.
                sel.cursor.1.min(char_count.saturating_sub(1))
            }
        } else {
            0
        };

        // Vim-style edge scrolling: the view only moves when the cursor
        // reaches the top or bottom edge of the visible area. This keeps
        // the context stable while selecting instead of chasing every move.
        //
        // Coordinate system: job_log_scroll = entries above the tail.
        // The renderer publishes the from-tail offset of the entry that
        // actually sits on the pane's top row (`job_log_top_offset`);
        // wrapped lines make that smaller than scroll + rows - 1, which
        // is why an edge check against the row count used to let the
        // cursor walk past the visible top before scrolling began.
        let cursor_from_tail = filtered.len().saturating_sub(1 + new_line);
        let top = self.job_log_top_offset.max(self.job_log_scroll);
        if cursor_from_tail > top {
            // Cursor crossed the top edge — scroll up by the overshoot;
            // the next frame re-publishes the new top.
            self.job_log_scroll += cursor_from_tail - top;
        } else if cursor_from_tail < self.job_log_scroll {
            // Cursor moved below the bottom edge — scroll down to reveal it.
            self.job_log_scroll = cursor_from_tail;
        }
        // else: cursor is within the viewport, leave scroll unchanged.

        self.visual_sel = Some(VisualSel {
            cursor: (new_line, new_col),
            ..sel
        });
    }

    /// Copy the visually-selected text to the system clipboard, then exit
    /// visual mode. Tries `wl-copy`, `xclip`, `xsel`, and `pbcopy` in order.
    fn yank_visual(&mut self) {
        let filtered = self.filtered_log_indices_for_job_log();
        let sel = match self.visual_sel.take() {
            Some(s) => s,
            None => return,
        };
        let ((start_line, start_col), (end_line, end_col)) = sel.normalized();
        let start_line = start_line.min(filtered.len().saturating_sub(1));
        let end_line = end_line.min(filtered.len().saturating_sub(1));
        let total = end_line - start_line + 1;

        let mut text = String::new();
        for (i, &log_idx) in filtered[start_line..=end_line].iter().enumerate() {
            let line_text = &self.log[log_idx].text;
            match sel.mode {
                VisualMode::Line => {
                    text.push_str(line_text);
                    text.push('\n');
                }
                VisualMode::Char => {
                    let chars: Vec<char> = line_text.chars().collect();
                    // The same bounds the renderer highlights with, so
                    // what looked selected is exactly what gets copied.
                    let (s, e) = joblog::char_selection_bounds(
                        chars.len(),
                        start_line + i,
                        start_line,
                        start_col,
                        end_line,
                        end_col,
                    );
                    text.push_str(&chars[s..e].iter().collect::<String>());
                    if i < total - 1 {
                        text.push('\n');
                    }
                }
            }
        }

        if yank_to_clipboard(&text) {
            self.push_log(
                format!(
                    "→ yanked {} line{} to clipboard",
                    total,
                    if total == 1 { "" } else { "s" }
                )
                .as_str(),
                false,
            );
        } else {
            self.push_log(
                "→ yank: no clipboard tool found (install wl-copy, xclip, or xsel)",
                false,
            );
        }
    }

    /// Vim-style "gg": jump to the top of whatever the focused pane
    /// is showing. For scroll panes "top" means the oldest line in
    /// the buffer (i.e. the maximum scroll-back offset); the renderer
    /// clamps the value against the real buffer length so over-
    /// shooting here is fine. For list panes it moves the cursor to
    /// the first entry. Every focus variant is handled explicitly so
    /// a new pane can't silently skip "g".
    fn jump_to_top(&mut self) {
        match self.focus {
            FocusPane::Hosts => {
                if !self.nodes.is_empty() {
                    self.selected = 0;
                }
            }
            FocusPane::JobLog => {
                let visible = self.filtered_log_indices_for_job_log().len();
                self.job_log_scroll = visible.saturating_sub(1);
            }
            FocusPane::Toggles => self.toggle_index = 0,
            FocusPane::Commands => self.command_index = 0,
        }
    }

    /// Snap whichever scroll pane currently has focus back to its
    /// tail (offset 0). The Details and Job Log panes both maintain
    /// their own offset; outside those panes this is a no-op.
    fn snap_to_tail(&mut self) {
        // Don't snap while the user is actively selecting — it would yank the
        // cursor to the tail and destroy their in-progress selection.
        if self.visual_sel.is_some() {
            return;
        }
        if self.focus == FocusPane::JobLog {
            self.job_log_scroll = 0;
        }
    }

    // ---------- background work ----------

    /// Spawn one task per node to probe reachability. Each task runs
    /// `ssh -G` (cheap, no connection) to resolve the node the way the
    /// real `ssh` would, then TCP-probes the resolved host:port. This
    /// makes the online badge honour `~/.ssh/config` (ProxyJump aside —
    /// we're still doing a direct TCP dial).
    fn refresh_reachability(&mut self) {
        // Piggy-back the agent badge refresh on the same gesture.
        if !self.agent.agents.is_empty() {
            self.fetch_agent_status();
        }
        // Flip every host into the "checking" state before spawning so
        // the UI shows the spinner on the very next frame instead of
        // waiting for the first TCP probe to return.
        for node in &self.nodes {
            self.status
                .entry(node.name.clone())
                .or_default()
                .checking_reachability = true;
        }
        // Snapshot the nodes before the spawn loop so the iteration
        // doesn't hold an immutable borrow of `self.nodes` while we
        // mutably reborrow `self.probe_tasks` via `track_probe`.
        let targets: Vec<Node> = self.nodes.clone();
        for node in &targets {
            self.spawn_probe(node, probe::Kind::Reachability);
        }
        // Re-discover nodes from the flake so any newly-added hosts
        // appear in the list without restarting the TUI.
        let flake_path = self.flake.clone();
        let tx = self.status_tx.clone();
        let handle = tokio::spawn(async move {
            if let Ok(nodes) = crate::flake::discover(&flake_path).await {
                let _ = tx.send(StatusUpdate::FlakeDiscover(nodes)).await;
            }
        });
        self.track_probe(handle);
        self.push_log("→ refreshing nodes and reachability", false);
    }

    /// Park a freshly-spawned probe task so it can be aborted later
    /// and prune any handles that have already finished. Called
    /// every time we start a new background probe (reachability,
    /// update, size, package diff).
    fn track_probe(&mut self, handle: JoinHandle<()>) {
        self.probe_tasks.retain(|h| !h.is_finished());
        self.probe_tasks.push(handle);
    }

    /// Snapshot the per-node context a probe needs. Everything is
    /// cloned out of `App`, so a probe in flight is unaffected by
    /// later state changes (override edits, node refreshes).
    fn probe_ctx(&self, node: &Node) -> probe::Ctx {
        probe::Ctx {
            flake: self.flake.clone(),
            node: node.clone(),
            override_: self.override_for(&node.name).clone(),
            askpass: self.askpass_env.clone(),
        }
    }

    /// Spawn one background probe against `node` and track its handle
    /// so `x` can cancel it. Progress lines and the final result come
    /// back through the status channel as [`StatusUpdate::Probe`].
    fn spawn_probe(&mut self, node: &Node, kind: probe::Kind) {
        let handle = probe::spawn(kind, self.probe_ctx(node), self.status_tx.clone());
        self.track_probe(handle);
    }

    /// Compare local-build vs remote symlink for the selected node's
    /// available profiles. Always populates the cheap-tier details
    /// (paths + activation time) as a byproduct; medium/expensive
    /// tiers live behind `U` and `p`.
    /// Resolve which hosts a per-host command (updates / sizes / pkg
    /// diff) should target. Mirrors the "marked wins over cursor"
    /// semantics that `request_deploy` uses so all per-host actions
    /// behave consistently: mark multiple and one keypress hits all
    /// of them.
    fn target_nodes(&self) -> Vec<Node> {
        if self.marked.is_empty() {
            self.selected_node().cloned().into_iter().collect()
        } else {
            self.marked
                .iter()
                .filter_map(|name| self.nodes.iter().find(|n| &n.name == name).cloned())
                .collect()
        }
    }

    fn refresh_updates_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_updates_for_node(&node);
        }
    }

    fn refresh_updates_for_node(&mut self, node: &Node) {
        // Mark every probe in flight *before* spawning so the UI flips to
        // its spinner state on the very next frame, not just after the
        // first task scheduling round-trip.
        {
            let entry = self.status.entry(node.name.clone()).or_default();
            for profile in node.profiles.keys() {
                entry.profile_mut(profile).checking = true;
            }
            entry.last_error = None;
        }

        let profiles: Vec<String> = node.profiles.keys().cloned().collect();
        for profile in profiles {
            self.spawn_probe(node, probe::Kind::Update { profile });
        }
        self.push_log_tagged(
            format!("→ checking updates for {}", node.name).as_str(),
            false,
            Some(node.name.clone()),
        );
    }

    /// Run the build-plan preflight (`Shift+P`) for every targeted node,
    /// covering whichever profiles the current profile selection would
    /// actually push.
    fn refresh_build_plan_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_build_plan_for_node(&node);
        }
    }

    /// Preflight one node. Split out from the batch entry point so the
    /// drift check can chain into it — the same way `Shift+U` chains
    /// into the package diff.
    fn refresh_build_plan_for_node(&mut self, node: &Node) {
        let site = self.build_site();
        {
            let profiles: Vec<String> = node
                .ordered_profiles()
                .into_iter()
                .filter(|p| match self.profile_sel {
                    ProfileSel::All => true,
                    ProfileSel::System => p == "system",
                    ProfileSel::Home => p == "home",
                })
                .collect();
            if profiles.is_empty() {
                self.push_log_tagged(
                    format!(
                        "! {} has no {} profile — nothing to plan",
                        node.name,
                        self.profile_sel.label()
                    )
                    .as_str(),
                    true,
                    Some(node.name.clone()),
                );
                return;
            }
            let entry = self.status.entry(node.name.clone()).or_default();
            if entry.checking_plan {
                return;
            }
            entry.checking_plan = true;
            self.push_log_tagged(
                format!(
                    "→ preflighting the {} build for {}",
                    site.label(),
                    node.name
                )
                .as_str(),
                false,
                Some(node.name.clone()),
            );

            for profile in profiles {
                self.spawn_probe(node, probe::Kind::BuildPlan { profile, site });
            }
        }
    }

    /// Build the cache-seeding plan for a node, if one applies.
    ///
    /// Seeding needs both halves: the drift check says *which* caches are
    /// new, and the build plan says *which paths* the deploy would
    /// otherwise compile. With only one of them there is nothing
    /// actionable, so we say so rather than seeding blind.
    fn seed_plan_for(&mut self, node: &str) -> Option<deploy::SeedPlan> {
        let status = self.status.get(node)?;
        let drift = status.cache_drift.as_ref()?;
        if !drift.has_drift() {
            return None;
        }
        // Only a remote build fetches through the target's store; for a
        // local build the target's cache set is irrelevant to the build.
        if drift.site != BuildSite::Remote {
            return None;
        }
        let substituters = drift.added_substituters.clone();
        let keys = drift.added_keys.clone();
        let paths: Vec<String> = status
            .build_plans()
            .flat_map(|(_, p)| p.seedable_paths())
            .collect();

        if paths.is_empty() {
            let has_plan = status.build_plans().next().is_some();
            let msg = if !has_plan {
                format!(
                    "! {node} adds {} cache(s) but no build plan is known — press Shift+P to \
resolve the paths so they can be seeded",
                    substituters.len()
                )
            } else {
                format!("• {node}: nothing left to seed — the plan needs no new paths")
            };
            let is_err = !has_plan;
            self.push_log_tagged(&msg, is_err, Some(node.to_string()));
            return None;
        }

        self.push_log_tagged(
            format!(
                "• {node}: seeding {} path(s) from {} newly-added cache(s) before the build",
                paths.len(),
                substituters.len()
            )
            .as_str(),
            false,
            Some(node.to_string()),
        );
        Some(deploy::SeedPlan {
            substituters,
            keys,
            paths,
        })
    }

    /// Where a deploy started right now would build. Toggle 4
    /// (`--remote-build`) is the only thing that moves it.
    fn build_site(&self) -> BuildSite {
        if self.toggles.remote_build {
            BuildSite::Remote
        } else {
            BuildSite::Local
        }
    }

    /// Run the substituter-drift check (`Shift+C`) for every targeted
    /// node.
    fn refresh_cache_drift_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        // Which store does the building decides whose substituter list we
        // have to compare against — see `host::BuildSite`.
        let site = self.build_site();
        for node in targets {
            let entry = self.status.entry(node.name.clone()).or_default();
            if entry.checking_cache {
                continue;
            }
            entry.checking_cache = true;
            self.push_log_tagged(
                format!(
                    "→ checking substituter drift for {} ({} build)",
                    node.name,
                    site.label()
                )
                .as_str(),
                false,
                Some(node.name.clone()),
            );

            self.spawn_probe(&node, probe::Kind::CacheDrift { site });
        }
    }

    /// Medium-tier update details: closure size delta for each of the
    /// selected host's profiles. Requires a prior `u` so we have the
    /// local/remote store paths to compare — if they're missing we
    /// log a hint and skip.
    fn refresh_sizes_for_selected(&mut self) {
        let targets = self.target_nodes();
        if targets.is_empty() {
            return;
        }
        for node in targets {
            self.refresh_sizes_for_node(&node);
        }
    }

    fn refresh_sizes_for_node(&mut self, node: &Node) {
        let mut launched = 0usize;
        let status = self.status.entry(node.name.clone()).or_default();
        let profiles: Vec<(String, Option<String>, Option<String>)> = node
            .profiles
            .keys()
            .map(|p| {
                let extra = &status.profile(p).extra;
                (
                    p.clone(),
                    extra.local_path.clone(),
                    extra.remote_path.clone(),
                )
            })
            .collect();
        for (profile, local, remote) in profiles {
            let (Some(local_path), Some(remote_path)) = (local, remote) else {
                continue;
            };
            // Flag "in flight" on the extras so the UI can spin.
            let entry = self.status.entry(node.name.clone()).or_default();
            entry.profile_mut(&profile).extra.checking_size = true;
            self.spawn_probe(
                node,
                probe::Kind::Size {
                    profile: profile.clone(),
                    local_path,
                    remote_path,
                },
            );
            launched += 1;
        }
        if launched == 0 {
            self.push_log_tagged(
                format!("! no cached paths for {} — press u first", node.name).as_str(),
                true,
                Some(node.name.clone()),
            );
        } else {
            self.push_log_tagged(
                format!("→ checking closure sizes for {}", node.name).as_str(),
                false,
                Some(node.name.clone()),
            );
        }
    }

    /// Expensive-tier update details: full package diff for a single
    /// `(node, profile)` pair. Called automatically from the size
    /// probe's Ok branch so `Shift+U` transparently chains into the
    /// package diff once the cached paths are known to be good —
    /// there's no separate key for it.
    ///
    /// `local_path`/`remote_path` are forwarded so the caller can
    /// pass the exact paths the size probe was measuring, avoiding a
    /// second lookup through the extras map (which could race with a
    /// subsequent `u`).
    fn spawn_pkg_diff_for_profile(
        &mut self,
        node: &Node,
        profile: &str,
        local_path: String,
        remote_path: String,
    ) {
        let entry = self.status.entry(node.name.clone()).or_default();
        entry.profile_mut(profile).extra.checking_pkg = true;
        self.spawn_probe(
            node,
            probe::Kind::PkgDiff {
                profile: profile.to_string(),
                local_path,
                remote_path,
            },
        );
        self.push_log_line(
            format!("→ computing package diff for {} ({profile})", node.name),
            false,
            Some(node.name.clone()),
            LogKind::Note,
        );
    }

    fn apply_status(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Probe(report) => self.apply_probe(report),
            StatusUpdate::AgentStatus(name, result) => {
                // Ignore replies from an agent the view has cycled away
                // from — only the current one may write the state.
                if self.agent.current_agent().map(|(n, _)| n == name) != Some(true) {
                    return;
                }
                self.agent.loading = false;
                match result {
                    Ok(status) => {
                        // Ambient managed-host map for badges + notice.
                        self.agent_managed.clear();
                        for w in &status.watches {
                            for h in &w.hosts {
                                let entry = self.agent_managed.entry(h.name.clone()).or_default();
                                entry.failed |= h.failed_rev.is_some();
                                entry.offline |= h.offline_rev.is_some();
                            }
                        }
                        self.agent.status = Some(status);
                        self.agent.error = None;
                        let rows = self.agent.host_rows().len();
                        if rows > 0 && self.agent.sel >= rows {
                            self.agent.sel = rows - 1;
                        }
                    }
                    Err(e) => {
                        self.agent.error = Some(e);
                    }
                }
            }
            StatusUpdate::AgentOp(result) => match result {
                Ok(msg) => {
                    self.push_log(&format!("[agent] {msg}"), false);
                    self.agent.last_op = Some(msg);
                    // The op changed daemon state; re-fetch so the view
                    // and badges catch up.
                    self.fetch_agent_status();
                }
                Err(e) => {
                    self.push_log(&format!("[agent] ! {e}"), true);
                    self.agent.last_op = Some(format!("! {e}"));
                }
            },
            StatusUpdate::AgentsDiscovered(found) => {
                self.agent.scanning = false;
                self.agent.scanned = true;
                let mut added = false;
                for (name, target) in found {
                    // Configured entries win; discovery only fills gaps
                    // (matching either the agent name or its target).
                    let dup = self
                        .agent
                        .agents
                        .iter()
                        .any(|(n, s)| *n == name || *s == target);
                    if !dup {
                        self.agent.agents.push((name, target));
                        added = true;
                    }
                }
                if added && self.agent.open && self.agent.status.is_none() {
                    self.push_log(
                        &format!(
                            "[agent] discovered {} agent(s) on deploy nodes",
                            self.agent.agents.len()
                        ),
                        false,
                    );
                    self.fetch_agent_status();
                    self.start_agent_tail();
                }
            }
            StatusUpdate::AgentTail(line) => {
                self.agent.tail.push_back(line);
                while self.agent.tail.len() > 2000 {
                    self.agent.tail.pop_front();
                }
            }
            StatusUpdate::FlakeDiscover(new_nodes) => {
                // Merge newly discovered nodes into the running list.
                // Nodes already present keep all their accumulated
                // status (reachability, update checks, extras) — we
                // only append nodes that weren't known before.
                for node in new_nodes {
                    if !self.nodes.iter().any(|n| n.name == node.name) {
                        self.push_log(&format!("→ new node discovered: {}", node.name), false);
                        self.nodes.push(node);
                    }
                }
            }
        }
    }

    /// Fold one probe report into host state. Probe *policy* lives
    /// here — which cached extras a result invalidates, and which
    /// follow-up probes it chains into — while the spawn/progress
    /// plumbing lives in [`probe::spawn`].
    fn apply_probe(&mut self, report: probe::Report) {
        match report {
            probe::Report::Reachability { node, result } => {
                let entry = self.status.entry(node).or_default();
                entry.reachability = result;
                entry.checking_reachability = false;
                // Stamp the "last seen up" time on every successful
                // probe so the details pane can show something freshly
                // anchored ("up 3s ago") rather than the stale label
                // from whatever the previous sweep found.
                if result == Reachability::Online {
                    entry.last_online = Some(std::time::SystemTime::now());
                }
            }
            probe::Report::Update {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let state = match &result {
                    Ok(c) if c.not_deployed => UpdateState::NotDeployed,
                    Ok(c) if c.up_to_date => UpdateState::UpToDate,
                    Ok(_) => UpdateState::NeedsUpdate,
                    Err(e) => {
                        entry.last_error = Some(e.clone());
                        UpdateState::Error
                    }
                };
                // Cache the cheap details (paths + activation time) on
                // the per-profile extras so the details pane can render
                // them without any extra work. An error clears the old
                // cached values so we never show stale paths alongside
                // a failed probe.
                let ps = entry.profile_mut(&profile);
                let ex = &mut ps.extra;
                match &result {
                    Ok(c) if c.not_deployed => {
                        // No remote path exists yet — clear everything
                        // so we don't show stale data from a previous probe.
                        ex.local_path = None;
                        ex.remote_path = None;
                        ex.activation_time = None;
                        ex.local_size = None;
                        ex.remote_size = None;
                        ex.pkg_diff = None;
                    }
                    Ok(c) => {
                        ex.local_path = Some(c.local_path.clone());
                        ex.remote_path = Some(c.remote_path.clone());
                        ex.activation_time = c.activation_time;
                        // A fresh `u` invalidates the medium/expensive
                        // tiers — the closure we just resolved may
                        // not be the one we sized / diffed last
                        // time. Clear them so the user re-triggers
                        // Shift+U / p against the new paths instead
                        // of reading stale numbers as current.
                        ex.local_size = None;
                        ex.remote_size = None;
                        ex.pkg_diff = None;
                    }
                    Err(_) => {
                        ex.local_path = None;
                        ex.remote_path = None;
                        ex.activation_time = None;
                        // The medium/expensive results are scoped
                        // to the paths we just invalidated — drop
                        // them so a later `U`/`p` doesn't render
                        // garbage for the wrong closure.
                        ex.local_size = None;
                        ex.remote_size = None;
                        ex.pkg_diff = None;
                    }
                }
                ps.checking = false;
                ps.update = state;
            }
            probe::Report::Size {
                node,
                profile,
                result,
            } => {
                // Snapshot the paths we'll hand to the auto-chained
                // package diff below so we don't have to re-borrow
                // `self.status` after the entry mutation. Same source
                // the size probe just measured against — guarantees
                // the diff looks at the closures whose sizes the
                // user is currently reading.
                let mut chain_paths: Option<(String, String)> = None;
                let entry = self.status.entry(node.clone()).or_default();
                let ex = &mut entry.profile_mut(&profile).extra;
                ex.checking_size = false;
                let mut probe_err = None;
                match result {
                    Ok((local, remote)) => {
                        ex.local_size = Some(local);
                        ex.remote_size = Some(remote);
                        if let (Some(lp), Some(rp)) =
                            (ex.local_path.clone(), ex.remote_path.clone())
                        {
                            chain_paths = Some((lp, rp));
                        }
                    }
                    Err(e) => {
                        ex.local_size = None;
                        ex.remote_size = None;
                        probe_err = Some(e);
                    }
                }
                if let Some(e) = probe_err {
                    entry.last_error = Some(e);
                }
                // Auto-chain the package diff after a successful size
                // probe — the old `p` keybind is gone; `Shift+U`
                // implicitly performs both tiers back to back so the
                // details pane ends up with the full picture without
                // the user having to orchestrate it.
                if let Some((local_path, remote_path)) = chain_paths {
                    if let Some(node_obj) = self.nodes.iter().find(|n| n.name == node).cloned() {
                        self.spawn_pkg_diff_for_profile(
                            &node_obj,
                            &profile,
                            local_path,
                            remote_path,
                        );
                    }
                }
            }
            probe::Report::PkgDiff {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let ex = &mut entry.profile_mut(&profile).extra;
                ex.checking_pkg = false;
                let mut probe_err = None;
                match result {
                    Ok(diff) => ex.pkg_diff = Some(diff),
                    Err(e) => {
                        ex.pkg_diff = None;
                        probe_err = Some(e);
                    }
                }
                if let Some(e) = probe_err {
                    entry.last_error = Some(e);
                }
            }
            probe::Report::BuildPlan {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node.clone()).or_default();
                entry.checking_plan = false;
                match result {
                    Ok(plan) => {
                        for (text, is_err) in describe_plan(&node, &profile, &plan) {
                            self.push_log_tagged(&text, is_err, Some(node.clone()));
                        }
                        let entry = self.status.entry(node).or_default();
                        entry.profile_mut(&profile).build_plan = Some(plan);
                    }
                    Err(e) => {
                        entry.last_error = Some(e.clone());
                        self.push_log_tagged(
                            format!("! build plan for {profile} failed: {e}").as_str(),
                            true,
                            Some(node),
                        );
                    }
                }
            }
            probe::Report::CacheDrift { node, result } => {
                let entry = self.status.entry(node.clone()).or_default();
                entry.checking_cache = false;
                match result {
                    Ok(drift) => {
                        entry.cache_drift = Some(drift.clone());
                        for (text, is_err) in describe_drift(&node, &drift) {
                            self.push_log_tagged(&text, is_err, Some(node.clone()));
                        }
                        // Chain into the preflight, same shape as
                        // `Shift+U` → package diff. Seeding needs the
                        // path list, and a user who just learned their
                        // deploy adds an unusable cache wants to know
                        // what it will cost anyway.
                        if drift.has_drift() && drift.site == BuildSite::Remote {
                            if let Some(n) = self.nodes.iter().find(|n| n.name == node).cloned() {
                                self.refresh_build_plan_for_node(&n);
                            }
                        }
                    }
                    Err(e) => {
                        entry.cache_drift = None;
                        entry.last_error = Some(e.clone());
                        self.push_log_tagged(
                            format!("! cache check failed: {e}").as_str(),
                            true,
                            Some(node),
                        );
                    }
                }
            }
            probe::Report::Progress { node, line } => {
                self.push_log_line(line.text, false, Some(node), line.kind);
            }
        }
    }

    // ---- agent view ----

    /// Does the agent auto-deploy any of these nodes? Drives the
    /// confirm-popup warning and its `p` shortcut.
    pub fn agent_manages_any(&self, hosts: &[String]) -> bool {
        hosts.iter().any(|h| self.agent_managed.contains_key(h))
    }

    /// `a` — open the full-screen agent view. Always opens: with no
    /// agents configured the view renders setup instructions (and any
    /// settings parse error) instead of being a dead key.
    fn open_agent_view(&mut self) {
        self.agent.open = true;
        self.agent.sel = 0;
        if !self.agent.agents.is_empty() {
            self.fetch_agent_status();
            self.start_agent_tail();
        } else {
            // Zero-config path: the flake already names every host —
            // ask them who runs an agent instead of demanding a
            // client-side settings file.
            self.scan_for_agents();
        }
    }

    /// Probe every deploy node for a running agent, in parallel.
    /// BatchMode + short timeouts: a host that is down or would
    /// prompt for a password is simply not discoverable this way —
    /// the settings file remains the override for those (and for
    /// agent hosts that aren't deploy nodes at all).
    fn scan_for_agents(&mut self) {
        if self.agent.scanning {
            return;
        }
        self.agent.scanning = true;
        let candidates: Vec<(String, String)> = self
            .nodes
            .iter()
            .map(|n| {
                let override_ = self.override_for(&n.name);
                (
                    n.name.clone(),
                    crate::host::build_ssh_target(n, "system", override_),
                )
            })
            .collect();
        let tx = self.status_tx.clone();
        tokio::spawn(async move {
            let probes = candidates.into_iter().map(|(name, target)| async move {
                match agentclient::probe(&target).await {
                    Ok(_) => Some((name, target)),
                    Err(_) => None,
                }
            });
            let found: Vec<(String, String)> = futures::future::join_all(probes)
                .await
                .into_iter()
                .flatten()
                .collect();
            let _ = tx.send(StatusUpdate::AgentsDiscovered(found)).await;
        });
    }

    fn close_agent_view(&mut self) {
        self.agent.open = false;
        self.stop_agent_tail();
    }

    /// Kick off a background status fetch of the current agent.
    fn fetch_agent_status(&mut self) {
        let Some((name, ssh)) = self.agent.current_agent() else {
            return;
        };
        let (name, ssh) = (name.to_string(), ssh.to_string());
        self.agent.loading = true;
        let tx = self.status_tx.clone();
        let env = self.askpass_env.clone();
        tokio::spawn(async move {
            let result = agentclient::fetch_status(&ssh, &env)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(StatusUpdate::AgentStatus(name, result)).await;
        });
    }

    /// Run a mutating agent verb in the background; the ack (or error)
    /// comes back as [`StatusUpdate::AgentOp`].
    fn agent_op(&mut self, args: Vec<String>) {
        let Some((_, ssh)) = self.agent.current_agent() else {
            return;
        };
        let ssh = ssh.to_string();
        let tx = self.status_tx.clone();
        let env = self.askpass_env.clone();
        tokio::spawn(async move {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = agentclient::op(&ssh, &env, &refs)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(StatusUpdate::AgentOp(result)).await;
        });
    }

    fn start_agent_tail(&mut self) {
        self.stop_agent_tail();
        let Some((_, ssh)) = self.agent.current_agent() else {
            return;
        };
        let tx = self.status_tx.clone();
        let handle =
            agentclient::spawn_tail(ssh.to_string(), self.askpass_env.clone(), move |line| {
                // Best-effort: a full channel drops tail lines rather than
                // blocking the reader (the history endpoint has the truth).
                let _ = tx.try_send(StatusUpdate::AgentTail(line));
            });
        self.agent.tail_task = Some(handle);
    }

    fn stop_agent_tail(&mut self) {
        if let Some(task) = self.agent.tail_task.take() {
            // kill_on_drop on the ssh child reaps it with the task.
            task.abort();
        }
    }

    /// Key handling while the agent view is open (Normal input mode).
    fn handle_key_agent(&mut self, key: KeyEvent) {
        let rows = self.agent.host_rows();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('a') => self.close_agent_view(),
            KeyCode::Char('j') | KeyCode::Down => {
                if !rows.is_empty() {
                    self.agent.sel = (self.agent.sel + 1) % rows.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !rows.is_empty() {
                    self.agent.sel = (self.agent.sel + rows.len() - 1) % rows.len();
                }
            }
            KeyCode::Char('r') => {
                if self.agent.agents.is_empty() {
                    self.scan_for_agents();
                } else {
                    self.fetch_agent_status();
                }
            }
            // `u` — ask the agent to poll now (all watches).
            KeyCode::Char('u') => self.agent_op(vec!["kick".into()]),
            // `p` — pause/resume the selected host.
            KeyCode::Char('p') => {
                if let Some((watch, host, paused)) = self.selected_agent_host(&rows) {
                    let verb = if paused { "resume" } else { "pause" };
                    self.agent_op(vec![
                        verb.into(),
                        "--host".into(),
                        host,
                        "--watch".into(),
                        watch,
                    ]);
                }
            }
            // `P` — pause/resume the whole agent.
            KeyCode::Char('P') => {
                let paused = self
                    .agent
                    .status
                    .as_ref()
                    .map(|s| s.paused)
                    .unwrap_or(false);
                let verb = if paused { "resume" } else { "pause" };
                self.agent_op(vec![verb.into()]);
            }
            // `x` — stop the run in flight (mirrors the main screen's
            // cancel key). Pause only gates future polls; this is the
            // stop button.
            KeyCode::Char('x') => self.agent_op(vec!["cancel".into()]),
            // `d` — force-deploy the selected host at the last-seen rev.
            KeyCode::Char('d') => {
                if let Some((watch, host, _)) = self.selected_agent_host(&rows) {
                    self.agent_op(vec!["deploy".into(), host, "--watch".into(), watch]);
                }
            }
            // `[` / `]` — cycle between configured agents.
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let n = self.agent.agents.len();
                if n > 1 {
                    self.agent.current = if key.code == KeyCode::Char(']') {
                        (self.agent.current + 1) % n
                    } else {
                        (self.agent.current + n - 1) % n
                    };
                    self.agent.status = None;
                    self.agent.error = None;
                    self.agent.sel = 0;
                    self.agent.tail.clear();
                    self.fetch_agent_status();
                    self.start_agent_tail();
                }
            }
            _ => {}
        }
    }

    /// `(watch name, host name, paused)` of the selected view row.
    fn selected_agent_host(&self, rows: &[(usize, usize)]) -> Option<(String, String, bool)> {
        let (wi, hi) = *rows.get(self.agent.sel)?;
        let status = self.agent.status.as_ref()?;
        let w = status.watches.get(wi)?;
        let h = w.hosts.get(hi)?;
        Some((w.name.clone(), h.name.clone(), h.paused))
    }

    /// `s` — toggle the system profile in/out of the deploy target.
    /// `profile_sel` stays the tri-state deploy-rs concept; the two
    /// keys just walk it: both on = All. Turning the last profile off
    /// is refused (a deploy must target something).
    fn toggle_profile_system(&mut self) {
        self.profile_sel = match self.profile_sel {
            ProfileSel::All => ProfileSel::Home,
            ProfileSel::Home => ProfileSel::All,
            ProfileSel::System => {
                self.push_log("! at least one profile must stay selected", true);
                return;
            }
        };
    }

    /// `h` — toggle the home profile. See [`Self::toggle_profile_system`].
    fn toggle_profile_home(&mut self) {
        self.profile_sel = match self.profile_sel {
            ProfileSel::All => ProfileSel::System,
            ProfileSel::System => ProfileSel::All,
            ProfileSel::Home => {
                self.push_log("! at least one profile must stay selected", true);
                return;
            }
        };
    }

    /// Build the candidate target list for `mode` and open the
    /// confirmation popup. Marked hosts win over the cursor selection,
    /// because that's the more deliberate action: if the user took the
    /// trouble to mark, that's what they want.
    fn request_deploy(&mut self, mode: Mode) {
        if self.deploy.is_some() {
            self.push_log("! a deploy is already running — press x to cancel", true);
            return;
        }
        // Boot is not supported by home-manager; block it when only home is targeted.
        if mode == Mode::Boot && self.profile_sel == ProfileSel::Home {
            self.push_log("! boot is not supported for the home-manager profile", true);
            return;
        }
        let hosts: Vec<String> = if self.marked.is_empty() {
            match self.selected_node().map(|n| n.name.clone()) {
                Some(name) => vec![name],
                None => {
                    self.push_log("! no host selected", true);
                    return;
                }
            }
        } else {
            self.marked.clone()
        };
        if hosts.is_empty() {
            self.push_log("! no hosts to deploy", true);
            return;
        }
        // Open the modal — actual side effects happen when the user
        // presses `y`.
        self.input = InputMode::ConfirmDeploy {
            hosts,
            mode,
            profile: self.profile_sel,
        };
    }

    /// Confirmed by the user. Stash the queue and kick off the first
    /// deploy. The remaining hosts are run sequentially as each child
    /// exits cleanly (see `handle_deploy_line`).
    ///
    /// The cached password (if any) is preserved: the SudoPre flow
    /// just populated it, and the askpass/sudo flows also rely on
    /// carrying a cache across subsequent hosts in the same batch.
    /// The cache is cleared on deploy exit, failure, and cancel.
    fn run_confirmed(&mut self, hosts: Vec<String>, mode: Mode, profile: ProfileSel) {
        self.mode = mode;
        // Fresh run wipes the previous outcome and snaps the job log to
        // auto-tail so the user sees the new output immediately.
        self.last_deploy = None;
        self.job_log_scroll = 0;
        self.visual_sel = None;
        let total = hosts.len();
        self.start_next_in_queue(hosts.into_iter().collect(), mode, profile, total, 0);
    }

    /// Pop the next host from `queue` and spawn the deploy, installing
    /// a fresh [`DeploySession`] that owns the remaining queue. Skips
    /// hosts that lack the requested profile (logs a warning) so a
    /// single bad target doesn't poison the whole batch. When every
    /// remaining host is a skip, no session is created — the batch is
    /// simply over.
    fn start_next_in_queue(
        &mut self,
        mut queue: VecDeque<String>,
        mode: Mode,
        profile_sel: ProfileSel,
        total: usize,
        mut done: usize,
    ) {
        // Drain hosts that turn out to be impossible up front so the
        // queue progress stays consistent (the user-visible total still
        // includes them — they're just counted as "done" with a skip).
        while let Some(name) = queue.pop_front() {
            let Some(node) = self.nodes.iter().find(|n| n.name == name).cloned() else {
                self.push_log_tagged(
                    format!("! unknown host {name} — skipped").as_str(),
                    true,
                    Some(name.clone()),
                );
                done = done.saturating_add(1);
                continue;
            };
            let profile = match profile_sel {
                ProfileSel::Home if !node.has_home() => {
                    self.push_log_tagged(
                        format!("! {name} has no home profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    done = done.saturating_add(1);
                    continue;
                }
                ProfileSel::System if !node.has_system() => {
                    self.push_log_tagged(
                        format!("! {name} has no system profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    done = done.saturating_add(1);
                    continue;
                }
                other => other,
            };
            // Profiles in deploy order, so `DeployRequest::plan` can tell
            // whether an ssh-user override fits each one and split the run
            // when it doesn't.
            let profiles = node
                .ordered_profiles()
                .into_iter()
                .filter_map(|name| {
                    node.profiles.get(&name).map(|p| deploy::ProfileInfo {
                        name,
                        user: p.user.clone(),
                    })
                })
                .collect();
            let req = DeployRequest {
                flake: self.flake.clone(),
                node: node.name.clone(),
                profile,
                mode,
                toggles: self.toggles,
                ssh_override: self.override_for(&node.name).clone(),
                askpass: self.askpass_env.clone(),
                profiles,
                extra_build_args: self.extra_build_args.clone(),
                seed: self.seed_plan_for(&node.name),
                node_info: Some(node.clone()),
            };
            if !self.extra_build_args.is_empty() {
                self.push_log_tagged(
                    format!("• extra build args: {}", self.extra_build_args.join(" ")).as_str(),
                    false,
                    Some(node.name.clone()),
                );
                if self.toggles.remote_build {
                    let inert = inert_remote_build_args(&self.extra_build_args);
                    if !inert.is_empty() {
                        self.push_log_tagged(
                            format!(
                                "! {} is inert with --remote-build: it configures the local nix \
client, but the target's daemon does the fetching. Use Shift+C so deptui seeds the \
target's store instead.",
                                inert.join(", ")
                            )
                            .as_str(),
                            true,
                            Some(node.name.clone()),
                        );
                    }
                }
            }
            self.push_log_tagged(
                format!(
                    "→ deploy [{}/{}] {} ({}, {})",
                    done + 1,
                    total,
                    node.name,
                    mode.label(),
                    profile.label(),
                )
                .as_str(),
                false,
                Some(node.name.clone()),
            );
            // When interactive_sudo is on, pass the cached password so
            // `deploy::run` can pre-feed it into the PTY that backs the
            // child's controlling tty. Clone so our cache survives for
            // replay on subsequent hosts in the queue.
            let sudo_pw = if self.toggles.interactive_sudo {
                self.cached_password
                    .as_deref()
                    .map(|s| Zeroizing::new(s.clone()))
            } else {
                None
            };
            let handle = deploy::run(req, sudo_pw);
            self.busy_label = if total > 1 {
                Some(format!("deploying [{}/{}] {}", done + 1, total, node.name))
            } else {
                Some(format!("deploying {}", node.name))
            };
            self.deploy = Some(DeploySession {
                rx: handle.rx,
                task: handle.task,
                cancel: Some(handle.cancel),
                stdin_tx: handle.stdin_tx,
                current: node.name,
                queue,
                mode,
                profile: profile_sel,
                total,
                done,
            });
            return;
        }
        // Queue drained without spawning anything (every host was a
        // skip) — there is no session, so there is nothing to reset.
    }

    fn cancel_deploy(&mut self) {
        // First: cancel any in-flight probe tasks. This is what makes
        // `x` actually stop a long-running package check (the most
        // common reason the user reaches for cancel when no deploy is
        // running). The Commands inside `host.rs` set
        // `kill_on_drop(true)`, so aborting the awaiting future also
        // reaps the underlying nix-store / ssh children instead of
        // orphaning them.
        let probes_aborted = self.cancel_probes();

        if let Some(mut session) = self.deploy.take() {
            // Signal first, then *detach* rather than abort. The deploy
            // task needs to stay alive long enough to SIGTERM its whole
            // process group, wait out the grace period, and SIGKILL
            // whatever is left; aborting here would drop the child
            // mid-sequence and `kill_on_drop` would only reach the group
            // leader, orphaning the `nix` builders underneath it.
            if let Some(c) = session.cancel.take() {
                c.cancel();
            } else {
                // No canceller (shouldn't happen) — fall back to the old
                // behaviour rather than leaving the task running.
                session.task.abort();
            }
            self.clear_cached_password();

            self.busy_label = None;
            // Cancelling kills the queue too — otherwise pressing `x`
            // mid-batch would surprise-deploy the next host. The queue
            // dies with the taken session; we only report its size.
            let drained = session.queue.len();
            if drained > 0 {
                self.push_log_tagged(
                    format!("! deploy cancelled — dropped {drained} queued host(s)").as_str(),
                    true,
                    Some(session.current.clone()),
                );
            } else {
                self.push_log_tagged("! deploy cancelled", true, Some(session.current.clone()));
            }
            let entry = LastDeploy {
                node: session.current.clone(),
                mode: session.mode,
                profile: session.profile,
                exit_code: -1,
                ok: false,
            };
            self.last_deploys
                .insert(session.current.clone(), entry.clone());
            self.last_deploy = Some(entry);
        } else if probes_aborted > 0 {
            // No deploy was running but probes were — surface that so
            // the user gets feedback for their `x` press.
            self.push_log(
                format!("! cancelled {probes_aborted} in-flight check(s)").as_str(),
                true,
            );
        }
    }

    /// Abort every tracked probe task and clear the per-host
    /// `checking_*` flags so spinners stop spinning. Returns the
    /// number of probes that were actually still in flight (i.e.
    /// hadn't already finished naturally) so the caller can decide
    /// whether to push a user-visible message.
    fn cancel_probes(&mut self) -> usize {
        let mut aborted = 0usize;
        for h in self.probe_tasks.drain(..) {
            if !h.is_finished() {
                aborted += 1;
                h.abort();
            }
        }
        // Clear every in-flight indicator. The aborted tasks will
        // never publish their final StatusUpdate, so without this
        // sweep the spinners would spin forever.
        for s in self.status.values_mut() {
            s.checking_reachability = false;
            for p in s.profiles.values_mut() {
                p.checking = false;
                p.extra.checking_size = false;
                p.extra.checking_pkg = false;
            }
            s.checking_cache = false;
            s.checking_plan = false;
        }
        aborted
    }

    fn handle_deploy_line(&mut self, line: LogLine) {
        match line {
            LogLine::Stdout(s) => {
                let host = self.deploy.as_ref().map(|d| d.current.clone());
                self.push_log_tagged(&s, false, host);
            }
            LogLine::Stderr(s) => {
                let host = self.deploy.as_ref().map(|d| d.current.clone());
                self.push_log_tagged(&s, true, host);
            }
            LogLine::SudoPrompt(prompt) => {
                if let Some(ref pw) = self.cached_password {
                    if let Some(tx) = self.deploy.as_ref().and_then(|s| s.stdin_tx.as_ref()) {
                        let _ = tx.try_send(pw.to_string());
                    }
                } else {
                    self.input = InputMode::PasswordPrompt {
                        prompt,
                        buf: SecretBuf::new(),
                        source: PromptSource::Sudo,
                    };
                }
            }
            LogLine::Exit(code) => {
                let ok = code == 0;
                let banner = if ok {
                    format!("← deploy succeeded (exit {code})")
                } else {
                    format!("← deploy failed (exit {code}) — magic-rollback may have reverted")
                };
                // Take the whole session: the child is gone, and every
                // piece of in-flight state goes with it. The follow-ups
                // (host tag, last-deploy entry, batch continuation) read
                // what they need off the taken value — one owner, no
                // field to forget. The post-failure "batch stopped"
                // notice in particular has to be host-tagged, otherwise
                // the job-log pane filters it out.
                let Some(mut session) = self.deploy.take() else {
                    self.push_log_tagged(&banner, !ok, None);
                    return;
                };
                let exit_host = session.current.clone();
                self.push_log_tagged(&banner, !ok, Some(exit_host.clone()));

                if matches!(self.input, InputMode::PasswordPrompt { .. }) {
                    self.input = InputMode::Normal;
                }
                self.busy_label = None;
                let entry = LastDeploy {
                    node: exit_host.clone(),
                    mode: session.mode,
                    profile: session.profile,
                    exit_code: code,
                    ok,
                };
                self.last_deploys.insert(exit_host.clone(), entry.clone());
                self.last_deploy = Some(entry);
                if ok {
                    // Stale-update marks: a successful push
                    // invalidates the previously-cached probe.
                    // Wipe the per-profile extras too — their
                    // paths, sizes, and package diff were scoped
                    // to the *previous* closure and would
                    // otherwise linger in the details pane until
                    // the user re-ran `u`/`U`/`p`.
                    if let Some(s) = self.status.get_mut(&exit_host) {
                        for p in s.profiles.values_mut() {
                            p.update = UpdateState::Unknown;
                            p.extra = ProfileExtra::default();
                            // The plan described the closure we
                            // just pushed; keeping it would tell
                            // the user a deploy still has work to
                            // do when it doesn't.
                            p.build_plan = None;
                        }
                        // Drift, on the other hand, is now stale in
                        // the opposite direction: the new nix.conf
                        // is live, so whatever it reported has been
                        // resolved by this very deploy.
                        s.cache_drift = None;
                    }
                }
                session.done = session.done.saturating_add(1);
                if ok {
                    if !session.queue.is_empty() {
                        // Continue the batch: the next session inherits
                        // the queue and progress of the finished one.
                        self.start_next_in_queue(
                            std::mem::take(&mut session.queue),
                            session.mode,
                            session.profile,
                            session.total,
                            session.done,
                        );
                    } else {
                        self.clear_cached_password();
                    }
                } else {
                    self.clear_cached_password();
                    // Stop the batch on failure — safer than blindly
                    // continuing to push to more hosts after one breaks.
                    let dropped = session.queue.len();
                    if dropped > 0 {
                        self.push_log_tagged(
                            format!("! batch stopped after failure — {dropped} host(s) skipped")
                                .as_str(),
                            true,
                            Some(exit_host),
                        );
                    }
                }
            }
            LogLine::Error(e) => {
                // Same take-the-session pattern as Exit: the banner,
                // the per-host last-deploy entry, and the batch-stopped
                // notice all read off the taken value.
                let Some(session) = self.deploy.take() else {
                    self.push_log_tagged(
                        format!("! deploy spawn failed: {e}").as_str(),
                        true,
                        None,
                    );
                    return;
                };
                let err_host = session.current.clone();
                self.push_log_tagged(
                    format!("! deploy spawn failed: {e}").as_str(),
                    true,
                    Some(err_host.clone()),
                );
                self.clear_cached_password();

                if matches!(self.input, InputMode::PasswordPrompt { .. }) {
                    self.input = InputMode::Normal;
                }
                self.busy_label = None;
                let entry = LastDeploy {
                    node: err_host.clone(),
                    mode: session.mode,
                    profile: session.profile,
                    exit_code: -1,
                    ok: false,
                };
                self.last_deploys.insert(err_host.clone(), entry.clone());
                self.last_deploy = Some(entry);
                let dropped = session.queue.len();
                if dropped > 0 {
                    self.push_log_tagged(
                        format!("! batch stopped — {dropped} host(s) skipped").as_str(),
                        true,
                        Some(err_host),
                    );
                }
            }
        }
    }

    fn push_log(&mut self, text: &str, is_err: bool) {
        self.push_log_tagged(text, is_err, None);
    }

    /// Push a log line that belongs to a specific host's deploy. Used
    /// by the deploy event handler so the batch log pane can colourise
    /// per host. `host = None` is equivalent to `push_log`.
    fn push_log_tagged(&mut self, text: &str, is_err: bool, host: Option<String>) {
        self.push_log_line(text.to_string(), is_err, host, LogKind::Plain);
    }

    /// Push a line with an explicit [`LogKind`], so the renderer can
    /// style it from data instead of parsing the text.
    fn push_log_line(&mut self, text: String, is_err: bool, host: Option<String>, kind: LogKind) {
        self.log.push(LogEntry {
            text,
            is_err,
            host,
            kind,
        });
        // Cap so we don't grow forever during long sessions.
        const MAX: usize = 2000;
        if self.log.len() > MAX {
            let drop = self.log.len() - MAX;
            self.log.drain(0..drop);
        }
    }
}

/// `--build-arg` values that configure substitution and therefore do
/// nothing for a remote build.
///
/// `--remote-build` runs a *local* nix client against a remote store
/// (`--store ssh-ng://…`), so these options configure the local client
/// while the fetching happens on the target, out of the target's own
/// nix.conf. nix reports nothing; the build just proceeds as if they
/// weren't there. Naming them explicitly is the only way the user finds
/// out.
const SUBSTITUTION_BUILD_ARGS: &[&str] = &[
    "substituters",
    "extra-substituters",
    "trusted-substituters",
    "trusted-public-keys",
    "extra-trusted-public-keys",
];

/// Which of `args` are substitution options that a remote build ignores.
pub fn inert_remote_build_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| {
            let key = a.trim_start_matches('-');
            SUBSTITUTION_BUILD_ARGS.contains(&key)
        })
        .cloned()
        .collect()
}

/// Render a build plan as job-log lines, as `(text, is_err)` pairs.
///
/// The build list is what the user is actually after — "this deploy will
/// compile ollama" is the fact that has to arrive *before* the deploy,
/// not forty minutes into it — so it is listed by name and marked as a
/// warning. Fetching is routine and stays neutral.
pub fn describe_plan(node: &str, profile: &str, plan: &BuildPlan) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if plan.nothing_to_do {
        out.push((
            format!("[plan] {node}.{profile}: already realised — nothing to build or fetch"),
            false,
        ));
        return out;
    }

    if plan.builds_anything() {
        out.push((
            format!(
                "[plan] {node}.{profile}: {} derivation(s) will be COMPILED",
                plan.to_build.len()
            ),
            true,
        ));
        // Cap the list: a from-scratch system is hundreds of entries and
        // would bury everything else in the log.
        const MAX_LISTED: usize = 12;
        for name in plan.build_labels().iter().take(MAX_LISTED) {
            out.push((format!("[plan]   ⚒ {name}"), true));
        }
        if plan.to_build.len() > MAX_LISTED {
            out.push((
                format!("[plan]   … +{} more", plan.to_build.len() - MAX_LISTED),
                true,
            ));
        }
    } else {
        out.push((
            format!("[plan] {node}.{profile}: nothing to compile"),
            false,
        ));
    }

    if !plan.to_fetch.is_empty() {
        let size = plan
            .download_bytes
            .map(|b| format!(" ({} download)", ui::humanise_bytes(b)))
            .unwrap_or_default();
        out.push((
            format!(
                "[plan] {node}.{profile}: {} path(s) will be fetched{size}",
                plan.to_fetch.len()
            ),
            false,
        ));
    }
    out
}

/// Turn a drift result into the lines the job log shows, as
/// `(text, is_err)` pairs.
///
/// The wording is deliberately specific about *why* the cache can't be
/// used, because the symptom (a deploy that compiles for 40 minutes) is
/// nowhere near the cause (a substituter that only goes live once
/// `nix-daemon` restarts, which happens after the build).
pub fn describe_drift(node: &str, drift: &SubstituterDrift) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if !drift.has_drift() {
        out.push((
            format!(
                "[cache] {node}: no new substituters — the {} store can already fetch \
everything this closure declares",
                drift.site.label()
            ),
            false,
        ));
    } else {
        let n = drift.added_substituters.len();
        if n > 0 {
            let where_ = match drift.site {
                BuildSite::Remote => "the target's nix-daemon",
                BuildSite::Local => "this machine's nix",
            };
            out.push((
                format!(
                    "[cache] {node}: this deploy adds {n} cache(s) that {where_} does not have \
yet — the build runs before the new nix.conf is active, so it cannot use them"
                ),
                true,
            ));
            for url in &drift.added_substituters {
                out.push((format!("[cache]   + {url}"), true));
            }
        }
        if !drift.added_keys.is_empty() {
            out.push((
                format!(
                    "[cache] {node}: {} new trusted-public-key(s) — a cache without its key is \
still unusable",
                    drift.added_keys.len()
                ),
                true,
            ));
            for key in &drift.added_keys {
                // Keys are long; the name before `:` is the identifying part.
                let short = key.split_once(':').map(|(n, _)| n).unwrap_or(key);
                out.push((format!("[cache]   + {short}:…"), true));
            }
        }
        out.push((
            match drift.site {
                BuildSite::Remote => format!(
                    "[cache] {node}: remote builds fetch from the *target's* nix.conf, so \
`-- --option extra-substituters …` will not help here"
                ),
                BuildSite::Local => format!(
                    "[cache] {node}: local build — pass the cache to this deploy with \
`-- --option extra-substituters <url> --option extra-trusted-public-keys <key>`"
                ),
            },
            false,
        ));
    }
    if !drift.removed_substituters.is_empty() {
        out.push((
            format!(
                "[cache] {node}: {} cache(s) present now but absent from the new config",
                drift.removed_substituters.len()
            ),
            false,
        ));
    }
    if drift.ssh_user_trusted == Some(false) {
        let user = drift.ssh_user.as_deref().unwrap_or("the ssh user");
        out.push((
            format!(
                "[cache] {node}: `{user}` is not in the target's `trusted-users` — substituter \
overrides sent to that host are silently ignored, with no warning from nix"
            ),
            true,
        ));
    }
    out
}

/// Upper bound on how many already-queued deploy lines one loop
/// iteration absorbs before painting. Matches the channel capacity in
/// `deploy::run`, so a full backlog clears in a single pass.
const MAX_LOG_DRAIN: usize = 256;

/// Floor on the interval between deploy-output repaints (~30fps). Only
/// applies while a deploy is running, where the tick timer guarantees a
/// follow-up frame.
const MIN_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// Receive from an `Option<Receiver<T>>`. Returns `None` (i.e. the branch
/// stays pending) when the option is empty, so `select!` can ignore it.
/// Await the running deploy's next log line, or pend forever when no
/// deploy is running — so the `select!` arm simply stays quiet.
async fn recv_deploy(session: &mut Option<DeploySession>) -> Option<LogLine> {
    match session {
        Some(s) => s.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Walk `~/.ssh` and return the paths that look like private keys. We
/// keep the filter conservative — anything that isn't a public key
/// (`*.pub`) or one of the well-known non-key files. The user can still
/// type a custom path in the picker, so missing a key here only costs a
/// keystroke, not correctness.
fn scan_ssh_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join(".ssh");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let blacklist = [
        "config",
        "known_hosts",
        "known_hosts.old",
        "authorized_keys",
        "authorized_keys2",
        "environment",
        "rc",
    ];
    let mut out: Vec<PathBuf> = read
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let ft = entry.file_type().ok()?;
            if !ft.is_file() {
                return None;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".pub") || blacklist.iter().any(|b| name == *b) {
                return None;
            }
            Some(path)
        })
        .collect();
    out.sort();
    out
}

/// Try to write `text` to the system clipboard. Attempts `wl-copy` (Wayland),
/// then `xclip`, then `xsel` (X11), then `pbcopy` (macOS). Returns `true` if
/// any tool succeeded.
/// Push `text` to the system clipboard. Returns `false` only when no
/// clipboard helper could be started at all.
///
/// Deliberately never blocks indefinitely on the child. `xclip` (and
/// `xsel --nodetach`) stay in the foreground *owning* the X selection
/// until another client claims it — `Child::wait()` on them never
/// returns, and since this runs on the UI thread that used to wedge the
/// whole TUI: no keys, no redraws, no way out but SIGKILL. We hand over
/// the text, give the helper a short grace period to fail fast on a bad
/// invocation, and otherwise leave it running detached (dropping a
/// `std::process::Child` does not kill it).
fn yank_to_clipboard(text: &str) -> bool {
    use std::io::Write as _;

    // Skipping helpers whose display server isn't present avoids
    // spawning processes that can only fail, and keeps the grace period
    // below from being spent on them.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let candidates: &[(&str, &[&str], bool)] = &[
        ("wl-copy", &[], wayland),
        ("xclip", &["-selection", "clipboard"], x11),
        ("xsel", &["--clipboard", "--input"], x11),
        ("pbcopy", &[], cfg!(target_os = "macos")),
    ];

    /// How long to wait for an immediate failure (helper not usable,
    /// wrong args) before assuming the helper is alive and holding the
    /// selection on purpose.
    const GRACE: std::time::Duration = std::time::Duration::from_millis(120);

    for &(cmd, args, usable) in candidates {
        if !usable {
            continue;
        }
        let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Drop stdin so the helper sees EOF and can take ownership.
        match child.stdin.take() {
            Some(mut stdin) => {
                if stdin.write_all(text.as_bytes()).is_err() {
                    let _ = child.kill();
                    continue;
                }
            }
            None => {
                let _ = child.kill();
                continue;
            }
        }

        let deadline = std::time::Instant::now() + GRACE;
        loop {
            match child.try_wait() {
                // Exited cleanly (wl-copy, pbcopy, xsel forking off).
                Ok(Some(status)) if status.success() => return true,
                // Exited non-zero — try the next helper.
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        // Still alive: it owns the selection now. Leave
                        // it detached rather than waiting on it.
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flake::{Node, Profile};
    use std::collections::BTreeMap;

    fn sample_nodes() -> Vec<Node> {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "system".into(),
            Profile {
                user: None,
                ssh_user: None,
            },
        );
        profiles.insert(
            "home".into(),
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
                profiles: {
                    let mut p = BTreeMap::new();
                    p.insert(
                        "system".into(),
                        Profile {
                            user: None,
                            ssh_user: None,
                        },
                    );
                    p
                },
                profiles_order: None,
            },
            Node {
                name: "gamma".into(),
                hostname: "gamma.lan".into(),
                ssh_user: None,
                profiles: BTreeMap::new(),
                profiles_order: None,
            },
        ]
    }

    fn drift(site: BuildSite, added: &[&str], keys: &[&str]) -> SubstituterDrift {
        SubstituterDrift {
            site,
            added_substituters: added.iter().map(|s| s.to_string()).collect(),
            added_keys: keys.iter().map(|s| s.to_string()).collect(),
            removed_substituters: Vec::new(),
            ssh_user: None,
            ssh_user_trusted: None,
        }
    }

    /// `build`/`fetch` are package names; the helper turns them into
    /// plausible store paths so `build_labels()` round-trips.
    fn plan(build: &[&str], fetch: &[&str], download: Option<u64>) -> BuildPlan {
        let path = |n: &str, drv: bool| {
            format!(
                "/nix/store/00000000000000000000000000000000-{n}{}",
                if drv { ".drv" } else { "" }
            )
        };
        BuildPlan {
            to_build: build.iter().map(|s| path(s, true)).collect(),
            to_fetch: fetch.iter().map(|s| path(s, false)).collect(),
            build_outputs: build.iter().map(|s| path(s, false)).collect(),
            download_bytes: download,
            unpacked_bytes: None,
            nothing_to_do: build.is_empty() && fetch.is_empty() && download.is_none(),
        }
    }

    #[test]
    fn flags_substitution_args_as_inert_for_remote_builds() {
        let args: Vec<String> = [
            "--option",
            "extra-substituters",
            "https://cuda",
            "--option",
            "extra-trusted-public-keys",
            "k:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let inert = inert_remote_build_args(&args);
        assert_eq!(
            inert,
            vec!["extra-substituters", "extra-trusted-public-keys"]
        );
    }

    #[test]
    fn ordinary_build_args_are_not_flagged() {
        let args: Vec<String> = ["--option", "max-jobs", "4", "--cores", "8"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(inert_remote_build_args(&args).is_empty());
    }

    #[test]
    fn describe_plan_names_what_will_be_compiled() {
        let p = plan(
            &["ollama-0.5.4", "cuda-merged-12.4"],
            &["hello-2.12"],
            Some(1024 * 1024),
        );
        let lines = describe_plan("gpu", "system", &p);
        let joined = lines
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("ollama-0.5.4"), "{joined}");
        assert!(
            joined.contains("2 derivation(s) will be COMPILED"),
            "{joined}"
        );
        assert!(joined.contains("1.0 MiB download"), "{joined}");
        // Compiling is the part that has to stand out.
        assert!(lines
            .iter()
            .any(|(t, is_err)| *is_err && t.contains("ollama")));
    }

    #[test]
    fn describe_plan_is_quiet_when_nothing_compiles() {
        let p = plan(&[], &["hello-2.12"], Some(2048));
        let lines = describe_plan("host", "system", &p);
        assert!(
            lines.iter().all(|(_, is_err)| !*is_err),
            "a fetch-only plan should raise no warnings: {lines:?}",
        );
    }

    #[test]
    fn describe_plan_reports_a_no_op_deploy() {
        let p = plan(&[], &[], None);
        let lines = describe_plan("host", "system", &p);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].0.contains("nothing to build or fetch"),
            "{lines:?}"
        );
    }

    #[test]
    fn describe_plan_caps_a_huge_build_list() {
        let names: Vec<String> = (0..40).map(|i| format!("pkg-{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let lines = describe_plan("host", "system", &plan(&refs, &[], None));
        assert!(
            lines.len() < 20,
            "a 40-entry build list must not flood the log: {} lines",
            lines.len(),
        );
        assert!(
            lines.iter().any(|(t, _)| t.contains("+28 more")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn shift_p_starts_a_build_plan_check() {
        let mut app = App::new(".".into(), sample_nodes());
        app.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert!(app.status_for("alpha").checking_plan);
        // `p` (unshifted) must still be the job-log pane jump.
        let mut app2 = App::new(".".into(), sample_nodes());
        app2.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app2.focus, FocusPane::JobLog);
        assert!(!app2.status_for("alpha").checking_plan);
    }

    #[test]
    fn describe_drift_names_the_cache_it_cannot_use() {
        let d = drift(BuildSite::Remote, &["https://cache.nixos-cuda.org"], &[]);
        let lines = describe_drift("gpu", &d);
        let joined = lines
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("https://cache.nixos-cuda.org"), "{joined}");
        assert!(joined.contains("adds 1 cache"), "{joined}");
        // The remote case must not suggest the local remedy, which is
        // inert across the ssh-ng store boundary.
        assert!(
            !joined.contains("pass the cache to this deploy"),
            "remote drift should not suggest extra build args: {joined}",
        );
        assert!(lines.iter().any(|(_, is_err)| *is_err));
    }

    #[test]
    fn describe_drift_suggests_build_args_for_local_builds() {
        let d = drift(BuildSite::Local, &["https://extra"], &[]);
        let joined = describe_drift("host", &d)
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("extra-substituters"), "{joined}");
    }

    #[test]
    fn describe_drift_is_quiet_when_nothing_was_added() {
        let d = drift(BuildSite::Remote, &[], &[]);
        let lines = describe_drift("host", &d);
        assert!(
            lines.iter().all(|(_, is_err)| !*is_err),
            "no drift should produce no error lines: {lines:?}",
        );
    }

    #[test]
    fn describe_drift_flags_an_untrusted_ssh_user() {
        let mut d = drift(BuildSite::Remote, &[], &[]);
        d.ssh_user = Some("deploy".into());
        d.ssh_user_trusted = Some(false);
        let lines = describe_drift("host", &d);
        assert!(
            lines
                .iter()
                .any(|(t, is_err)| *is_err && t.contains("trusted-users")),
            "{lines:?}",
        );
    }

    // Spawns the probe task, so it needs a reactor.
    #[tokio::test]
    async fn shift_c_starts_a_cache_check_for_the_selected_host() {
        let mut app = App::new(".".into(), sample_nodes());
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT));
        assert!(
            app.status_for("alpha").checking_cache,
            "Shift+C should mark the selected host as checking",
        );
        // `c` (unshifted) must still be the commands-pane jump.
        let mut app2 = App::new(".".into(), sample_nodes());
        app2.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app2.focus, FocusPane::Commands);
        assert!(!app2.status_for("alpha").checking_cache);
    }

    #[test]
    fn v_enters_visual_mode_from_the_hosts_pane() {
        // Regression: this used to require the job log to already have
        // focus, so the gesture right after a deploy did nothing.
        let mut app = App::new(".".into(), sample_nodes());
        app.focus = FocusPane::Hosts;
        app.push_log_tagged("some output", false, Some("alpha".into()));
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::JobLog);
        assert!(app.visual_sel.is_some());
    }

    #[test]
    fn esc_unwinds_selection_before_search() {
        let mut app = App::new(".".into(), sample_nodes());
        app.push_log_tagged("hello", false, Some("alpha".into()));
        app.log_search = Some("hello".into());
        app.focus = FocusPane::JobLog;
        app.enter_visual_mode(VisualMode::Line);
        assert!(app.visual_sel.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.visual_sel.is_none());
        assert!(
            app.log_search.is_some(),
            "search must survive the first Esc"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.log_search.is_none());
    }

    #[test]
    fn new_app_initialises_status_for_every_node() {
        let nodes = sample_nodes();
        let app = App::new(".".into(), nodes.clone());
        assert_eq!(app.status.len(), nodes.len());
        for n in &nodes {
            assert!(app.status.contains_key(&n.name));
        }
    }

    #[test]
    fn new_app_defaults() {
        let app = App::new(".".into(), sample_nodes());
        assert_eq!(app.selected, 0);
        assert_eq!(app.mode, Mode::Switch);
        assert_eq!(app.profile_sel, ProfileSel::All);
        assert!(app.marked.is_empty());
        assert_eq!(app.focus, FocusPane::Hosts);
        assert!(!app.show_help);
        assert!(app.log.is_empty());
        assert!(app.deploy.is_none());
        assert!(app.last_deploy.is_none());
    }

    #[test]
    fn selected_node_returns_correct_node() {
        let app = App::new(".".into(), sample_nodes());
        assert_eq!(app.selected_node().unwrap().name, "alpha");
    }

    #[test]
    fn selected_node_none_for_empty() {
        let app = App::new(".".into(), Vec::new());
        assert!(app.selected_node().is_none());
    }

    #[test]
    fn is_marked_works() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(!app.is_marked("alpha"));
        app.marked.push("alpha".into());
        assert!(app.is_marked("alpha"));
        assert!(!app.is_marked("beta"));
    }

    #[test]
    fn status_for_returns_default_for_unknown() {
        let app = App::new(".".into(), sample_nodes());
        let st = app.status_for("nonexistent");
        assert_eq!(st.reachability, Reachability::Unknown);
        assert_eq!(st.profile("system").update, UpdateState::Unknown);
    }

    #[test]
    fn override_for_returns_empty_by_default() {
        let app = App::new(".".into(), sample_nodes());
        let o = app.override_for("alpha");
        assert!(!o.is_active());
    }

    #[test]
    fn override_mut_creates_entry() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(!app.overrides.contains_key("alpha"));
        app.override_mut("alpha").hostname = Some("10.0.0.1".into());
        assert!(app.overrides.contains_key("alpha"));
        assert!(app.override_for("alpha").is_active());
    }

    #[test]
    fn push_log_caps_at_2000() {
        let mut app = App::new(".".into(), sample_nodes());
        for i in 0..2100 {
            app.push_log(&format!("line {i}"), false);
        }
        assert_eq!(app.log.len(), 2000);
        // Most recent line should still be present.
        assert_eq!(app.log.last().unwrap().text, "line 2099");
        // Oldest lines should have been drained.
        assert_eq!(app.log.first().unwrap().text, "line 100");
    }

    #[test]
    fn push_log_tagged_sets_host() {
        let mut app = App::new(".".into(), sample_nodes());
        app.push_log_tagged("deploying", false, Some("alpha".into()));
        assert_eq!(app.log[0].host.as_deref(), Some("alpha"));
    }

    #[test]
    fn toggles_start_at_deploy_rs_defaults() {
        let app = App::new(".".into(), sample_nodes());
        assert!(!app.toggles.skip_checks);
        assert!(app.toggles.magic_rollback);
        assert!(app.toggles.auto_rollback);
        assert!(!app.toggles.remote_build);
        assert!(!app.toggles.interactive_sudo);
    }

    #[test]
    fn describe_mode_labels() {
        assert_eq!(Mode::Switch.label(), "switch");
        assert_eq!(Mode::Boot.label(), "boot");
        assert_eq!(Mode::DryRun.label(), "dry-run");
    }

    #[test]
    fn describe_profile_labels() {
        assert_eq!(ProfileSel::All.label(), "all");
        assert_eq!(ProfileSel::System.label(), "system");
        assert_eq!(ProfileSel::Home.label(), "home");
    }

    #[test]
    fn focus_pane_rows() {
        assert_eq!(FocusPane::Toggles.row(), 0);
        assert_eq!(FocusPane::Hosts.row(), 1);
        assert_eq!(FocusPane::JobLog.row(), 1);
        assert_eq!(FocusPane::Commands.row(), 2);
    }

    #[test]
    fn command_pane_entries() {
        // Smoke test: at least verify the pane has the expected commands
        // and that indices match expectations for the nav cursor.
        assert!(COMMANDS.len() >= 11);
        assert_eq!(COMMANDS[0].0, Command::Refresh);
        assert_eq!(COMMANDS[0].1, "r");
        assert!(COMMANDS
            .iter()
            .any(|(c, k, _)| *c == Command::MarkAll && *k == "A"));
    }

    #[test]
    fn shift_a_marks_all_and_shift_x_clears_marks() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(app.marked.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(
            app.marked.len(),
            app.nodes.len(),
            "Shift+A marks every host"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert!(app.marked.is_empty(), "Shift+X clears all marks");
    }

    #[test]
    fn mark_all_command_button_follows_the_mark_state() {
        let mut app = App::new(".".into(), sample_nodes());
        let idx = COMMANDS
            .iter()
            .position(|(c, _, _)| *c == Command::MarkAll)
            .expect("mark-all button present");
        app.activate_command(idx);
        assert_eq!(app.marked.len(), app.nodes.len(), "button marks all");
        app.activate_command(idx);
        assert!(app.marked.is_empty(), "button clears marks when any exist");
    }

    #[test]
    fn handle_key_ctrl_c_shows_confirm_then_quits() {
        let mut app = App::new(".".into(), sample_nodes());
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_key(key);
        assert!(!app.should_quit);
        assert!(matches!(app.input, InputMode::ConfirmQuit { .. }));
        // Second Ctrl-C confirms immediately.
        app.handle_key(key);
        assert!(app.should_quit);
    }

    #[test]
    fn handle_key_q_shows_confirm_then_quits() {
        let mut app = App::new(".".into(), sample_nodes());
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        app.handle_key(key);
        assert!(!app.should_quit);
        assert!(matches!(app.input, InputMode::ConfirmQuit { .. }));
        // Pressing 'y' confirms.
        let confirm = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.handle_key(confirm);
        assert!(app.should_quit);
    }

    #[test]
    fn handle_key_question_mark_toggles_help() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(!app.show_help);
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_key(key);
        assert!(app.show_help);
    }

    #[test]
    fn handle_key_j_k_moves_selection() {
        let mut app = App::new(".".into(), sample_nodes());
        assert_eq!(app.selected, 0);

        // j moves down.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 1);

        // k moves up.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected, 0);

        // k at top wraps to bottom.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected, 2);
    }

    // Needs a runtime: opening the view spawns the status fetch task
    // (which fails fast against the fake ssh target and is dropped).
    #[tokio::test]
    async fn agent_view_opens_only_when_configured() {
        // No agents configured: `a` still opens the view — it renders
        // setup instructions instead of being a dead key.
        let mut app = App::new(".".into(), sample_nodes());
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.agent.open);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(!app.agent.open, "a toggles the view closed again");

        // Configured: the view opens, q closes it, `a` toggles too.
        let settings: crate::settings::Settings =
            toml::from_str("[agents.box]\nssh = \"me@box\"\n").unwrap();
        let mut app = App::with_settings(".".into(), sample_nodes(), settings);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.agent.open);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.agent.open, "q closes the agent view, not the app");
        assert!(
            !matches!(app.input, InputMode::ConfirmQuit { .. }),
            "q inside the view must not raise the quit popup"
        );
    }

    #[test]
    fn agent_view_selection_moves_over_host_rows() {
        let settings: crate::settings::Settings =
            toml::from_str("[agents.box]\nssh = \"me@box\"\n").unwrap();
        let mut app = App::with_settings(".".into(), sample_nodes(), settings);
        app.agent.open = true;
        app.agent.status = Some(agentwire::AgentStatus {
            version: "0".into(),
            paused: false,
            watches: vec![agentwire::WatchStatus {
                name: "w".into(),
                repo: "r".into(),
                ref_label: "branch main".into(),
                paused: false,
                last_seen: None,
                next_poll: None,
                running: None,
                hosts: vec![
                    agentwire::HostStatus {
                        name: "a".into(),
                        paused: false,
                        deployed_rev: None,
                        deployed_time: None,
                        failed_rev: None,
                        failed_time: None,
                        failed_message: None,
                        unreachable: None,
                        offline_rev: None,
                        offline_time: None,
                    },
                    agentwire::HostStatus {
                        name: "b".into(),
                        paused: true,
                        deployed_rev: None,
                        deployed_time: None,
                        failed_rev: None,
                        failed_time: None,
                        failed_message: None,
                        unreachable: None,
                        offline_rev: None,
                        offline_time: None,
                    },
                ],
            }],
        });
        assert_eq!(app.agent.host_rows().len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.agent.sel, 1);
        // Wraps.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.agent.sel, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.agent.sel, 1);
        // The selected row resolves to (watch, host, paused).
        let rows = app.agent.host_rows();
        let (w, h, paused) = app.selected_agent_host(&rows).unwrap();
        assert_eq!((w.as_str(), h.as_str(), paused), ("w", "b", true));
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn linear_hit_resolves_wrapped_positions() {
        let inner = Rect {
            x: 2,
            y: 1,
            width: 10,
            height: 2,
        };
        let items = vec![(1, 6, 0), (8, 14, 1)];
        // First row.
        assert_eq!(linear_hit(&items, inner, 3, 1), Some(0));
        assert_eq!(linear_hit(&items, inner, 8, 1), None); // gap
                                                           // Item 1 wraps onto row two: linear pos 12 = (1*10)+2.
        assert_eq!(linear_hit(&items, inner, 4, 2), Some(1));
        // Outside the rect entirely.
        assert_eq!(linear_hit(&items, inner, 3, 5), None);
    }

    #[test]
    fn mouse_clicks_select_hosts_flip_toggles_press_buttons() {
        let mut app = App::new(".".into(), sample_nodes());
        app.mouse.hosts = Some(Rect {
            x: 1,
            y: 5,
            width: 20,
            height: 5,
        });
        app.mouse.toggles = Some(Rect {
            x: 1,
            y: 1,
            width: 60,
            height: 1,
        });
        app.mouse.toggle_items = vec![(0, 10, 0)];
        app.mouse.commands = Some(Rect {
            x: 1,
            y: 20,
            width: 60,
            height: 1,
        });
        // Button 4 = the ProfileSystem command in COMMANDS.
        let sys_idx = COMMANDS
            .iter()
            .position(|(c, _, _)| *c == Command::ProfileSystem)
            .unwrap();
        app.mouse.command_items = vec![(0, 10, sys_idx)];

        // Click the second host row.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 6));
        assert_eq!(app.selected, 1);
        assert_eq!(app.focus, FocusPane::Hosts);

        // Click toggle 0 (skip-checks) — flips it and moves focus.
        assert!(!app.toggles.skip_checks);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1));
        assert!(app.toggles.skip_checks);
        assert_eq!(app.focus, FocusPane::Toggles);

        // Click the sys profile button: system leaves the selection.
        assert_eq!(app.profile_sel, ProfileSel::All);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 20));
        assert_eq!(app.profile_sel, ProfileSel::Home);

        // Wheel over the hosts pane moves the selection.
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 3, 6));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn mouse_is_inert_during_modal_input() {
        let mut app = App::new(".".into(), sample_nodes());
        app.mouse.hosts = Some(Rect {
            x: 1,
            y: 5,
            width: 20,
            height: 5,
        });
        app.input = InputMode::ConfirmQuit {
            deploy_running: false,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 6));
        assert_eq!(app.selected, 0, "clicks must not act under a modal popup");
    }

    #[test]
    fn discovered_agents_merge_without_clobbering_config() {
        let settings: crate::settings::Settings =
            toml::from_str("[agents.pinned]\nssh = \"me@box\"\n").unwrap();
        let mut app = App::with_settings(".".into(), sample_nodes(), settings);
        app.agent.scanning = true;
        app.apply_status(StatusUpdate::AgentsDiscovered(vec![
            ("alpha".into(), "root@alpha.lan".into()),
            // Same target as the pinned entry: not duplicated.
            ("boxy".into(), "me@box".into()),
        ]));
        assert!(!app.agent.scanning);
        assert!(app.agent.scanned);
        let names: Vec<&str> = app.agent.agents.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["pinned", "alpha"]);

        // A second scan finding the same node adds nothing.
        app.apply_status(StatusUpdate::AgentsDiscovered(vec![(
            "alpha".into(),
            "root@alpha.lan".into(),
        )]));
        assert_eq!(app.agent.agents.len(), 2);
    }

    #[test]
    fn handle_key_profile_toggles() {
        let mut app = App::new(".".into(), sample_nodes());
        // Both profiles start selected (All). `h` off → system only.
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::System);

        // Toggling the last selected profile off is refused.
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::System);

        // `h` back on → All again; `s` off → home only.
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::All);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::Home);
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::Home, "refused: last profile");
    }

    #[test]
    fn handle_key_toggle_skip_checks() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(!app.toggles.skip_checks);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(app.toggles.skip_checks);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(!app.toggles.skip_checks);
    }

    #[test]
    fn handle_key_toggle_magic_rollback() {
        let mut app = App::new(".".into(), sample_nodes());
        assert!(app.toggles.magic_rollback);
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(!app.toggles.magic_rollback);
    }

    #[test]
    fn handle_key_tab_cycles_focus() {
        let mut app = App::new(".".into(), sample_nodes());
        assert_eq!(app.focus, FocusPane::Hosts);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        // Tab should move focus (exact target depends on layout logic,
        // but it should NOT stay on Hosts).
        assert_ne!(app.focus, FocusPane::Hosts);
    }

    #[test]
    fn input_mode_starts_normal() {
        let app = App::new(".".into(), sample_nodes());
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn quit_confirm_n_cancels() {
        let mut app = App::new(".".into(), sample_nodes());
        // q opens the dialog.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::ConfirmQuit { .. }));
        // n cancels.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn quit_confirm_esc_cancels() {
        let mut app = App::new(".".into(), sample_nodes());
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::ConfirmQuit { .. }));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn search_n_works_from_hosts_pane() {
        let mut app = App::new(".".into(), sample_nodes());
        app.focus = FocusPane::Hosts;
        // Add a log entry with the search term so advance_match has
        // something to work with.
        app.push_log_tagged("hello test world", false, Some("alpha".to_string()));
        app.push_log_tagged("another test line", false, Some("alpha".to_string()));
        app.log_search = Some("test".to_string());
        app.log_search_match_idx = 1;

        // n from the Hosts pane should advance the match index.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.log_search_match_idx, 2);
        // Focus should remain on Hosts — the key was handled globally,
        // not by the Hosts pane's own key handler.
        assert_eq!(app.focus, FocusPane::Hosts);
    }

    #[test]
    fn esc_clears_search_from_any_pane() {
        let mut app = App::new(".".into(), sample_nodes());
        app.focus = FocusPane::Hosts;
        app.log_search = Some("needle".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.log_search.is_none());
    }

    // ---- PasswordPrompt input mode ----

    #[test]
    fn secret_buf_redacts_its_debug_output() {
        // `InputMode` derives Debug; a stray `{:?}` must not put the
        // plaintext into --log-file.
        let buf = SecretBuf::from("hunter2");
        let rendered = format!("{buf:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");

        let mode = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf,
            source: PromptSource::Askpass,
        };
        assert!(!format!("{mode:?}").contains("hunter2"));
    }

    #[test]
    fn secret_buf_grows_past_its_reserved_capacity() {
        let mut buf = SecretBuf::new();
        let long = "x".repeat(SECRET_BUF_CAPACITY * 2 + 7);
        for c in long.chars() {
            buf.push(c);
        }
        assert_eq!(buf.as_str(), long);
        assert_eq!(buf.char_count(), long.len());
    }

    #[test]
    fn secret_buf_handles_multibyte_input() {
        let mut buf = SecretBuf::new();
        for c in "pässwörd±é".chars() {
            buf.push(c);
        }
        assert_eq!(buf.as_str(), "pässwörd±é");
        assert_eq!(buf.char_count(), 10);
        buf.pop();
        assert_eq!(buf.as_str(), "pässwörd±");
    }

    /// The askpass server handles one dialog at a time and blocks on the
    /// reply. Dismissing without answering used to leave it blocked
    /// forever, killing askpass for the rest of the session.
    #[tokio::test]
    async fn dismissing_an_askpass_prompt_still_answers_the_server() {
        let mut app = App::new(".".into(), sample_nodes());
        let (tx, mut rx) = mpsc::channel::<Zeroizing<String>>(4);
        app.askpass_password_tx = tx;
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: SecretBuf::from("partial"),
            source: PromptSource::Askpass,
        };
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(matches!(app.input, InputMode::Normal));
        let reply = rx
            .try_recv()
            .expect("server must receive a reply on dismiss");
        assert!(reply.is_empty(), "dismissal must not send the typed prefix");
        // And the half-typed password must not have been cached.
        assert!(app.cached_password.is_none());
    }

    #[tokio::test]
    async fn submitting_an_askpass_prompt_sends_the_password() {
        let mut app = App::new(".".into(), sample_nodes());
        let (tx, mut rx) = mpsc::channel::<Zeroizing<String>>(4);
        app.askpass_password_tx = tx;
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: SecretBuf::from("hunter2"),
            source: PromptSource::Askpass,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(rx.try_recv().unwrap().as_str(), "hunter2");
    }

    #[test]
    fn password_prompt_esc_returns_to_normal() {
        let mut app = App::new(".".into(), sample_nodes());
        app.input = InputMode::PasswordPrompt {
            prompt: "[sudo] password for root: ".into(),
            buf: "secret".into(),
            source: PromptSource::Sudo,
        };
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn password_prompt_typing_appends_to_buf() {
        let mut app = App::new(".".into(), sample_nodes());
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: SecretBuf::new(),
            source: PromptSource::Askpass,
        };
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::PasswordPrompt { ref buf, .. } if buf == "abc"));
    }

    #[test]
    fn password_prompt_backspace_removes_char() {
        let mut app = App::new(".".into(), sample_nodes());
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: "xy".into(),
            source: PromptSource::Sudo,
        };
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::PasswordPrompt { ref buf, .. } if buf == "x"));
    }

    #[test]
    fn password_prompt_enter_returns_to_normal() {
        let mut app = App::new(".".into(), sample_nodes());
        // No stdin_tx set — Enter should still return to Normal (with an error log).
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: "hunter2".into(),
            source: PromptSource::Sudo,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn password_prompt_password_not_in_log_after_enter() {
        let mut app = App::new(".".into(), sample_nodes());
        app.input = InputMode::PasswordPrompt {
            prompt: "Password:".into(),
            buf: "supersecret".into(),
            source: PromptSource::Sudo,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for entry in &app.log {
            assert!(
                !entry.text.contains("supersecret"),
                "password leaked into log: {:?}",
                entry.text,
            );
        }
    }

    #[test]
    fn askpass_prompt_enter_returns_to_normal() {
        let mut app = App::new(".".into(), sample_nodes());
        // No askpass_tx set — Enter should still return to Normal.
        app.input = InputMode::PasswordPrompt {
            prompt: "Enter passphrase for key: ".into(),
            buf: "mypass".into(),
            source: PromptSource::Askpass,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::Normal));
    }

    // ---- probe report policy (apply_probe) ----
    //
    // The probe seam makes the result-handling policy testable without
    // spawning a single process: construct a `probe::Report`, feed it
    // in, and assert on the state transitions and chained probes.

    #[test]
    fn progress_report_lands_in_the_host_log() {
        let mut app = App::new(".".into(), sample_nodes());
        app.apply_probe(probe::Report::Progress {
            node: "alpha".into(),
            line: crate::host::ProgressLine::note("[size] measuring local closure …"),
        });
        let entry = app.log.last().expect("progress line pushed");
        assert_eq!(entry.host.as_deref(), Some("alpha"));
        assert!(entry.text.contains("[size]"));
        assert!(!entry.is_err);
    }

    #[tokio::test]
    async fn size_probe_ok_chains_into_pkg_diff() {
        let mut app = App::new(".".into(), sample_nodes());
        // The cheap tier populated the paths the chain re-uses.
        {
            let entry = app.status.entry("alpha".into()).or_default();
            let ex = &mut entry.profile_mut("system").extra;
            ex.local_path = Some("/nix/store/aaa-x".into());
            ex.remote_path = Some("/nix/store/bbb-x".into());
            ex.checking_size = true;
        }
        app.apply_probe(probe::Report::Size {
            node: "alpha".into(),
            profile: "system".into(),
            result: Ok((10, 20)),
        });
        let st = app.status_for("alpha");
        let ex = &st.profile("system").extra;
        assert_eq!(ex.local_size, Some(10));
        assert_eq!(ex.remote_size, Some(20));
        assert!(!ex.checking_size);
        assert!(
            ex.checking_pkg,
            "a successful size probe must chain into the package diff"
        );
    }

    #[tokio::test]
    async fn remote_cache_drift_chains_into_build_plan() {
        let mut app = App::new(".".into(), sample_nodes());
        let drift = crate::host::SubstituterDrift {
            site: BuildSite::Remote,
            added_substituters: vec!["https://cache.example.org".into()],
            added_keys: vec![],
            removed_substituters: vec![],
            ssh_user: Some("root".into()),
            ssh_user_trusted: Some(true),
        };
        app.apply_probe(probe::Report::CacheDrift {
            node: "alpha".into(),
            result: Ok(drift),
        });
        let st = app.status_for("alpha");
        assert!(st.cache_drift.is_some());
        assert!(
            st.checking_plan,
            "remote drift must chain into the build-plan preflight"
        );
    }

    #[test]
    fn update_probe_error_clears_cached_extras() {
        let mut app = App::new(".".into(), sample_nodes());
        {
            let entry = app.status.entry("alpha".into()).or_default();
            let ps = entry.profile_mut("system");
            ps.extra.local_path = Some("/nix/store/aaa-x".into());
            ps.extra.local_size = Some(1);
            ps.extra.pkg_diff = Some(crate::host::PkgDiff::default());
            ps.checking = true;
        }
        app.apply_probe(probe::Report::Update {
            node: "alpha".into(),
            profile: "system".into(),
            result: Err("host unreachable".into()),
        });
        let st = app.status_for("alpha");
        let ps = st.profile("system");
        assert_eq!(ps.update, UpdateState::Error);
        assert!(!ps.checking);
        assert!(ps.extra.local_path.is_none());
        assert!(ps.extra.local_size.is_none());
        assert!(ps.extra.pkg_diff.is_none());
        assert_eq!(st.last_error.as_deref(), Some("host unreachable"));
    }

    #[test]
    fn visual_edge_scroll_starts_at_the_rendered_top_not_the_row_count() {
        let mut app = App::new(".".into(), sample_nodes());
        for i in 0..10 {
            app.push_log_tagged(&format!("line {i}"), false, Some("alpha".into()));
        }
        app.focus = FocusPane::JobLog;
        // The renderer saw wrapped lines: only the last 4 entries fit,
        // so the entry on the pane's top row is 3 entries from the tail
        // even though the pane itself is taller in rows.
        app.job_log_top_offset = 3;
        app.job_log_scroll = 0;
        app.enter_visual_mode(VisualMode::Line);

        // Moving up to the visible top must not scroll.
        for _ in 0..3 {
            app.visual_move_cursor(-1, 0);
        }
        assert_eq!(app.job_log_scroll, 0);

        // The very next step crosses the top — scrolling starts
        // immediately, not a few rows later.
        app.visual_move_cursor(-1, 0);
        assert_eq!(app.job_log_scroll, 1);
    }
}
