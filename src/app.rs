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
use zeroize::Zeroizing;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::askpass::{AskpassEnv, AskpassServer};
use crate::deploy::{self, DeployRequest, LogLine, Mode, ProfileSel, Toggles};
use crate::event::{spawn as spawn_events, AppEvent};
use crate::flake::Node;
use crate::host::{self, HostStatus, ProfileCheck, ProfileExtra, Reachability, UpdateState};
use crate::ssh::SshOverride;
use crate::ui::{self, Tui};

/// Focusable regions of the UI. Each one has its own keyboard
/// affordance when focused: Hosts moves the selection, Details scrolls
/// the log, Toggles lets you flip the deploy-rs flags without hitting
/// 1–5, and Commands exposes every keybind action as a navigable button
/// row. Tab/Shift-Tab cycles forward/back; Shift+H/L also crosses
/// sub-nav boundaries inside Toggles and Commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Toggles,
    Hosts,
    Details,
    JobLog,
    Commands,
}

impl FocusPane {
    /// Row in the grid layout. 0 = toggles (top), 1 = middle (hosts /
    /// details / job log), 2 = commands (bottom). Used by the vertical
    /// pane-move keys to decide what "up" and "down" mean.
    pub fn row(self) -> usize {
        match self {
            FocusPane::Toggles => 0,
            FocusPane::Hosts | FocusPane::Details | FocusPane::JobLog => 1,
            FocusPane::Commands => 2,
        }
    }
}

/// Number of toggle cells, kept in one place so the nav bounds check
/// stays consistent with the rendering code.
pub const TOGGLE_COUNT: usize = 5;

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
    ProfileAll,
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
    (Command::Refresh, "r", "refresh"),
    (Command::Updates, "u", "updates"),
    (Command::ProfileAll, "a", "all"),
    (Command::ProfileSystem, "s", "sys"),
    (Command::ProfileHome, "h", "home"),
    (Command::Switch, "S", "switch"),
    (Command::Boot, "B", "boot"),
    (Command::DryRun, "D", "dry"),
    (Command::Cancel, "x", "cancel"),
    (Command::Override, "o", "override"),
];

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub text: String,
    pub is_err: bool,
    /// Which host's deploy produced this line, if any. `None` is for
    /// app-level status messages (reachability sweeps, toggle flips,
    /// banner strings, etc.). Used by the batch-log pane to colour-tag
    /// each line with its origin host.
    pub host: Option<String>,
}

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

/// Which log pane an in-progress `/` search is targeted at. The two
/// log panes (details + job log) maintain independent scroll positions
/// and content filters, so a single global search would land on the
/// wrong line. We pin the target at the moment the user presses `/` and
/// keep using it for `n` / `Shift+N` until they start a new search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    /// The right-column job log (host-tagged deploy output only).
    JobLog,
}

/// Whether the visual selection in the job log is character-level or line-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualMode {
    /// `v` — cursor tracks (line, col); partial-line selection is possible.
    Char,
    /// `V` — whole lines only; col component is ignored.
    Line,
}

/// Active visual selection in the job log pane. Indices are in terms of the
/// *filtered* log (same index space as `filtered_log_indices_for_job_log`).
#[derive(Debug, Clone)]
pub struct VisualSel {
    pub mode: VisualMode,
    /// The end the user *started* the selection from. Fixed until selection ends.
    pub anchor: (usize, usize), // (filtered_line_idx, char_col)
    /// The end the user is currently moving. Drives `j`/`k`/`h`/`l`.
    pub cursor: (usize, usize),
}

impl VisualSel {
    /// Returns the normalised range `(start, end)` where start ≤ end.
    /// Both elements are `(filtered_line_idx, char_col)`.
    pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        let (al, ac) = self.anchor;
        let (cl, cc) = self.cursor;
        if al < cl || (al == cl && ac <= cc) {
            ((al, ac), (cl, cc))
        } else {
            ((cl, cc), (al, ac))
        }
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
        target: SearchTarget,
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
        /// Password being typed. Rendered as `•` characters.
        buf: String,
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

/// Background work updates we receive over the status channel.
#[derive(Debug)]
enum StatusUpdate {
    Reachability(String, Reachability),
    /// Re-discovered flake nodes from the last `r` refresh. Merges new
    /// nodes into the running list without disturbing existing state.
    FlakeDiscover(Vec<Node>),
    UpdateProbe {
        node: String,
        profile: String,
        result: Result<ProfileCheck, String>,
    },
    /// Closure-size probe result: `(local_bytes, remote_bytes)`. Owned
    /// by the medium-tier update details (`U`).
    SizeProbe {
        node: String,
        profile: String,
        result: Result<(u64, u64), String>,
    },
    /// `nix store diff-closures` output for the expensive-tier check
    /// (`p`). Empty string = closures identical.
    PkgDiffProbe {
        node: String,
        profile: String,
        result: Result<String, String>,
    },
    /// Free-form progress line from a long-running probe (currently
    /// only the package diff). Forwarded into the host-tagged log so
    /// the user sees activity instead of a silent spinner.
    LogLine {
        node: String,
        text: String,
        is_err: bool,
    },
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
    /// Which pane the committed `log_search` belongs to. The two log
    /// panes share `App.log_search` storage but only the targeted one
    /// renders highlights and responds to `n`/`Shift+N`.
    pub log_search_target: Option<SearchTarget>,
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
    /// Lines from the bottom of the details/status log the user has
    /// scrolled up. `0` means "auto-tail" (always show the latest line).
    pub log_scroll: usize,
    /// Same contract as `log_scroll` but for the job log pane, which
    /// has its own independent scroll state so the user can focus it
    /// and scroll without disturbing the details log position.
    pub job_log_scroll: usize,
    /// Last known rendered height (in rows) of the job log viewport.
    /// Set by `draw_job_log` each frame; used by `visual_move_cursor`
    /// to implement vim-style edge scrolling (view only moves when the
    /// cursor reaches the top or bottom edge of the visible area).
    pub job_log_viewport_height: usize,
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

    /// In-flight deploy. We hold both the receiver (for log lines) and the
    /// task handle so we can cancel.
    deploy_rx: Option<mpsc::Receiver<LogLine>>,
    deploy_task: Option<JoinHandle<()>>,
    /// When the in-flight deploy was started with `--interactive-sudo`, this
    /// sender lets the TUI write the sudo password to the child's piped
    /// stdin. `None` otherwise. Dropped on cancel or deploy completion.
    deploy_stdin_tx: Option<mpsc::Sender<String>>,

    /// App-level askpass environment: script and socket paths, cloned
    /// into every task that spawns SSH.
    askpass_env: AskpassEnv,
    /// Send passwords to the askpass server (which relays them to the
    /// SSH_ASKPASS helper over the Unix socket).
    askpass_password_tx: mpsc::Sender<String>,
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
    /// Pending hosts to deploy after the current one finishes. Populated
    /// when the user kicks off a multi-host deploy. The currently
    /// running host is NOT in this queue (it lives in `current_target`).
    deploy_queue: VecDeque<String>,
    /// Sticky parameters for the in-flight queue so each subsequent
    /// host is deployed with the same mode/profile/toggles the user
    /// originally confirmed.
    queue_mode: Mode,
    queue_profile: ProfileSel,
    /// Total hosts in the run that produced the current queue. Stays
    /// fixed while the queue drains so progress is `done/total`. Reset
    /// to 0 when the queue is empty.
    pub queue_total: usize,
    pub queue_done: usize,
    /// The host currently being deployed (if any). Separate from the
    /// queue so the running host can be displayed independently.
    pub current_target: Option<String>,

    /// True once we receive a quit request.
    should_quit: bool,
}

impl App {
    pub fn new(flake: String, nodes: Vec<Node>) -> Self {
        let (status_tx, status_rx) = mpsc::channel(64);
        let mut status = HashMap::new();
        for n in &nodes {
            status.insert(n.name.clone(), HostStatus::default());
        }

        // Askpass channels are created now (cheap); the actual server
        // is started in `run()` which has a tokio runtime. Until then
        // `askpass_env` holds a dummy value — it's overwritten before
        // any SSH commands are spawned.
        let (askpass_password_tx, _placeholder_rx) = mpsc::channel::<String>(4);
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
            log_search_target: None,
            log_search_match_idx: 0,
            help_search: None,
            last_deploy: None,
            last_deploys: HashMap::new(),
            log_scroll: 0,
            job_log_scroll: 0,
            job_log_viewport_height: 0,
            visual_sel: None,
            show_help: false,
            help_scroll: 0,
            input: InputMode::Normal,
            tick_counter: 0,
            status_tx,
            status_rx,
            deploy_rx: None,
            deploy_task: None,
            deploy_stdin_tx: None,
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
            deploy_queue: VecDeque::new(),
            queue_mode: Mode::Switch,
            queue_profile: ProfileSel::All,
            queue_total: 0,
            queue_done: 0,
            current_target: None,
            should_quit: false,
        }
    }

    /// Cache a password in memory, locking its pages to prevent swapping.
    fn set_cached_password(&mut self, password: String) {
        self.clear_cached_password();
        let pw = Zeroizing::new(password);
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
        if self.deploy_task.is_some() {
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
        let (askpass_password_tx, askpass_password_rx) = mpsc::channel::<String>(4);
        self.askpass_password_tx = askpass_password_tx;
        self.askpass_prompt_rx = askpass_prompt_rx;
        self._askpass_task = Some(tokio::spawn(async move {
            askpass_server
                .serve(askpass_prompt_tx, askpass_password_rx)
                .await;
        }));

        let mut events = spawn_events();

        // Kick off an initial reachability sweep so the first frame isn't
        // all "unknown".
        self.refresh_reachability();

        terminal.draw(|f| ui::draw(f, self))?;

        while !self.should_quit {
            let needs_redraw;
            tokio::select! {
                biased;

                Some(ev) = events.recv() => {
                    // Ticks only need a redraw when something is animating
                    // (spinners). Otherwise skip the expensive draw pass.
                    needs_redraw = !matches!(ev, AppEvent::Tick) || self.has_inflight_work();
                    self.handle_event(ev);
                }

                Some(update) = self.status_rx.recv() => {
                    needs_redraw = true;
                    self.apply_status(update);
                }

                Some(line) = recv_optional(&mut self.deploy_rx) => {
                    needs_redraw = true;
                    self.handle_deploy_line(line);
                }

                Some(prompt) = self.askpass_prompt_rx.recv() => {
                    if let Some(ref pw) = self.cached_password {
                        let _ = self.askpass_password_tx.try_send(pw.to_string());
                        needs_redraw = false;
                    } else {
                        needs_redraw = true;
                        self.input = InputMode::PasswordPrompt {
                            prompt,
                            buf: String::new(),
                            source: PromptSource::Askpass,
                        };
                    }
                }
            }

            if needs_redraw {
                terminal.draw(|f| ui::draw(f, self))?;
            }
        }

        // Cancel any running deploy when we exit. The child will be reaped
        // by tokio when its handles drop.
        if let Some(t) = self.deploy_task.take() {
            t.abort();
        }

        Ok(())
    }

    // ---------- event handling ----------

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => self.tick_counter = self.tick_counter.wrapping_add(1),
            AppEvent::Term(CtEvent::Key(key)) => self.handle_key(key),
            AppEvent::Term(_) => {}
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
                    deploy_running: self.deploy_task.is_some(),
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
            InputMode::SearchLog { target, buf } => {
                self.handle_key_search_log(key, target, buf);
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
                    deploy_running: self.deploy_task.is_some(),
                };
                return;
            }
            // Esc was an accidental quit before — now it just no-ops
            // in Normal mode so a stray escape doesn't kill the TUI.
            // Modal handlers (override / confirm / identity picker)
            // still consume Esc to back out themselves. If visual
            // selection is active, Esc clears it first.
            KeyCode::Esc => {
                if self.log_search.is_some() {
                    self.clear_log_search();
                }
                self.visual_sel = None;
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
            // btop-style direct pane jumps. Picked letters that don't
            // collide with anything else: `f` = focus hosts (the
            // obvious `h` is taken by the home-profile shortcut and
            // `n` is taken by search-next), `i` = inspect details,
            // `v` = view job log, `t` = toggles, `c` = commands.
            KeyCode::Char('f') => {
                self.focus = FocusPane::Hosts;
                return;
            }
            KeyCode::Char('i') => {
                self.focus = FocusPane::Details;
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
            self.input = InputMode::SearchLog {
                target: SearchTarget::JobLog,
                buf: String::new(),
            };
            return;
        }

        // `n`/`N` jump between search matches from any pane — the search
        // is global so navigating results should be too.
        if self.log_search.is_some()
            && matches!(self.log_search_target, Some(SearchTarget::JobLog))
        {
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
            FocusPane::Details => {
                // Details pane no longer has a scrollable log — key
                // events fall through to the global action keys below.
            }
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

            // Profile selection. `s` = sys (mnemonic: System), `h` = home, `a` = all.
            KeyCode::Char('a') => self.profile_sel = ProfileSel::All,
            KeyCode::Char('s') => self.profile_sel = ProfileSel::System,
            KeyCode::Char('h') => self.profile_sel = ProfileSel::Home,

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

    /// Advance focus in reading order: Toggles → Hosts → Details →
    /// JobLog → Commands → Toggles. Tab uses this; Shift+Tab uses
    /// [`focus_prev`].
    fn focus_next(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Hosts,
            FocusPane::Hosts => FocusPane::Details,
            FocusPane::Details => FocusPane::JobLog,
            FocusPane::JobLog => FocusPane::Commands,
            FocusPane::Commands => FocusPane::Toggles,
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            FocusPane::Toggles => FocusPane::Commands,
            FocusPane::Hosts => FocusPane::Toggles,
            FocusPane::Details => FocusPane::Hosts,
            FocusPane::JobLog => FocusPane::Details,
            FocusPane::Commands => FocusPane::JobLog,
        };
    }

    /// Horizontal pane move. With the new two-column layout:
    ///   Left column:  Hosts (top) | Details (bottom)
    ///   Right column: JobLog (full height)
    ///
    /// Shift+L from Hosts or Details moves to JobLog.
    /// Shift+H from JobLog moves back to Hosts.
    /// Clamped at both ends (no wrap) so stray Shift+L at the right
    /// edge doesn't teleport back to the host list.
    fn pane_move_horizontal(&mut self, delta: i32) {
        self.focus = match (self.focus, delta) {
            (FocusPane::Hosts, 1) | (FocusPane::Details, 1) => FocusPane::JobLog,
            (FocusPane::JobLog, -1) => FocusPane::Hosts,
            _ => self.focus, // already at edge or not in the middle row
        };
    }

    /// Vertical pane move. With the new layout Hosts and Details stack
    /// vertically in the left column, so Shift+J/K navigate between
    /// them in addition to crossing the row boundary.
    ///
    ///   Shift+J:  Toggles → Hosts → Details → Commands
    ///             JobLog  → Commands
    ///   Shift+K:  Commands → JobLog
    ///             Details  → Hosts → Toggles
    ///             Hosts    → Toggles
    ///
    /// Clamped at the top and bottom edges.
    fn pane_move_vertical(&mut self, delta: i32) {
        self.focus = match (self.focus, delta) {
            // Down
            (FocusPane::Toggles, 1) => FocusPane::Hosts,
            (FocusPane::Hosts, 1) => FocusPane::Details,
            (FocusPane::Details, 1) | (FocusPane::JobLog, 1) => FocusPane::Commands,
            // Up
            (FocusPane::Commands, -1) => FocusPane::JobLog,
            (FocusPane::Details, -1) => FocusPane::Hosts,
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
        match idx {
            0 => {
                self.toggles.skip_checks = !self.toggles.skip_checks;
                self.log_toggle("skip-checks", self.toggles.skip_checks);
            }
            1 => {
                self.toggles.magic_rollback = !self.toggles.magic_rollback;
                self.log_toggle("magic-rollback", self.toggles.magic_rollback);
            }
            2 => {
                self.toggles.auto_rollback = !self.toggles.auto_rollback;
                self.log_toggle("auto-rollback", self.toggles.auto_rollback);
            }
            3 => {
                self.toggles.remote_build = !self.toggles.remote_build;
                self.log_toggle("remote-build", self.toggles.remote_build);
            }
            4 => {
                self.toggles.interactive_sudo = !self.toggles.interactive_sudo;
                self.log_toggle("interactive-sudo", self.toggles.interactive_sudo);
                if self.toggles.interactive_sudo {
                    self.push_log(
                        "  interactive-sudo: TUI will prompt securely when sudo asks for a password",
                        false,
                    );
                }
            }
            _ => {}
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
            Command::ProfileAll => self.profile_sel = ProfileSel::All,
            Command::ProfileSystem => self.profile_sel = ProfileSel::System,
            Command::ProfileHome => self.profile_sel = ProfileSel::Home,
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
                    let first_host = hosts
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "host".into());
                    self.pending_deploy = Some((hosts, mode, profile));
                    self.input = InputMode::PasswordPrompt {
                        prompt: format!("sudo password for {first_host}: "),
                        buf: String::new(),
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
    fn handle_key_search_log(&mut self, key: KeyEvent, target: SearchTarget, mut buf: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // Cancel: drop the buffer AND any previously-committed
                // query so the highlights vanish. Most-explicit way
                // for the user to "turn search off entirely".
                self.input = InputMode::Normal;
                self.log_search = None;
                self.log_search_target = None;
            }
            KeyCode::Enter => {
                self.input = InputMode::Normal;
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    // Committing empty == clearing.
                    self.log_search = None;
                    self.log_search_target = None;
                    return;
                }
                self.log_search = Some(trimmed);
                self.log_search_target = Some(target);
                // Jump to the first match nearest the tail (newest).
                match target {
                    SearchTarget::JobLog => {
                        self.job_log_scroll = 0;
                        self.search_job_log_jump_initial();
                    }
                }
            }
            KeyCode::Backspace => {
                buf.pop();
                self.input = InputMode::SearchLog { target, buf };
            }
            KeyCode::Char(c) if !ctrl => {
                buf.push(c);
                self.input = InputMode::SearchLog { target, buf };
            }
            _ => {
                self.input = InputMode::SearchLog { target, buf };
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
        mut buf: String,
        source: PromptSource,
    ) {
        match key.code {
            KeyCode::Enter => {
                // Cache the password for replay within this action.
                self.set_cached_password(buf.clone());
                // Route the password to the appropriate destination.
                match source {
                    PromptSource::Askpass => {
                        let _ = self.askpass_password_tx.try_send(buf);
                        self.input = InputMode::Normal;
                    }
                    PromptSource::Sudo => {
                        if let Some(tx) = &self.deploy_stdin_tx {
                            let _ = tx.try_send(buf);
                        } else {
                            self.push_log(
                                "! no stdin channel available for sudo password",
                                true,
                            );
                        }
                        self.input = InputMode::Normal;
                    }
                    PromptSource::SudoPre => {
                        // Cached above; now actually start the deploy.
                        // The cached password will be pulled into the
                        // DeployRequest inside `start_next_in_queue`.
                        // `buf` isn't needed beyond the cache — drop it.
                        drop(buf);
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
                    _ => {
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
        self.advance_match(SearchTarget::JobLog, direction);
    }

    /// First-jump variant: set the active match to the last occurrence
    /// (nearest the tail) and scroll to it. Used right after commit so
    /// the cursor lands on something visible.
    fn search_job_log_jump_initial(&mut self) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let total = self.count_all_matches(SearchTarget::JobLog, query);
        self.log_search_match_idx = total;
        self.scroll_to_match(SearchTarget::JobLog);
    }

    /// Increment (direction=+1) or decrement (direction=-1) the active
    /// match index, wrapping at the edges, then scroll so the line
    /// containing the match is visible.
    fn advance_match(&mut self, target: SearchTarget, direction: i32) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let total = self.count_all_matches(target, query);
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
        self.scroll_to_match(target);
    }

    /// Scroll the targeted pane so the line containing the current
    /// active match (by `log_search_match_idx`) is visible. Finds the
    /// Nth occurrence by walking filtered entries and counting per-line
    /// hits.
    fn scroll_to_match(&mut self, target: SearchTarget) {
        let Some(query) = self.log_search.as_ref() else {
            return;
        };
        let filtered = match target {
            SearchTarget::JobLog => self.filtered_log_indices_for_job_log(),
        };
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
                match target {
                    SearchTarget::JobLog => self.job_log_scroll = scroll,
                }
                return;
            }
            seen += hits;
        }
    }

    /// Drop the committed log search. Leaves the scroll positions
    /// alone so the user stays where they were when they pressed Esc.
    fn clear_log_search(&mut self) {
        self.log_search = None;
        self.log_search_target = None;
        self.log_search_match_idx = 0;
    }

    /// Return `(current, total)` for the committed log search on
    /// `target`. `current` is `log_search_match_idx` (1-based); `total`
    /// is the count of every individual occurrence of the query across
    /// all filtered lines (a single line with two hits counts twice).
    pub fn log_search_stats(&self, target: SearchTarget) -> (usize, usize) {
        let Some(query) = self.log_search.as_ref() else {
            return (0, 0);
        };
        if self.log_search_target != Some(target) {
            return (0, 0);
        }
        let total = self.count_all_matches(target, query);
        (self.log_search_match_idx, total)
    }

    /// Total number of individual query occurrences in the targeted pane.
    fn count_all_matches(&self, target: SearchTarget, query: &str) -> usize {
        let filtered = match target {
            SearchTarget::JobLog => self.filtered_log_indices_for_job_log(),
        };
        let mut total = 0usize;
        for &idx in &filtered {
            total += self.log[idx].text.matches(query).count();
        }
        total
    }

    /// Indices into `self.log` that the job-log pane currently shows.
    /// Mirrors the filter inside `draw_job_log` in `ui.rs`: shows only
    /// entries for the marked hosts (or the selected host when no marks
    /// are set).
    fn filtered_log_indices_for_job_log(&self) -> Vec<usize> {
        let active: std::collections::HashSet<&str> = if self.marked.is_empty() {
            self.selected_node()
                .map(|n| n.name.as_str())
                .into_iter()
                .collect()
        } else {
            self.marked.iter().map(|s| s.as_str()).collect()
        };
        self.log
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                e.host
                    .as_deref()
                    .filter(|h| active.contains(*h))
                    .map(|_| i)
            })
            .collect()
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
        // Coordinate system: job_log_scroll = lines above the tail.
        // The viewport shows cursor_from_tail values in [scroll, scroll+vh-1].
        let cursor_from_tail = filtered.len().saturating_sub(1 + new_line);
        let vh = self.job_log_viewport_height.max(1);
        if cursor_from_tail > self.job_log_scroll + vh.saturating_sub(1) {
            // Cursor moved above the top edge — scroll up to reveal it.
            self.job_log_scroll = cursor_from_tail.saturating_sub(vh.saturating_sub(1));
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
                    let (s, e) = if total == 1 {
                        (start_col.min(chars.len()), (end_col + 1).min(chars.len()))
                    } else if i == 0 {
                        (start_col.min(chars.len()), chars.len())
                    } else if i == total - 1 {
                        (0, (end_col + 1).min(chars.len()))
                    } else {
                        (0, chars.len())
                    };
                    text.push_str(&chars[s..e].iter().collect::<String>());
                    if i < total - 1 {
                        text.push('\n');
                    }
                }
            }
        }

        if yank_to_clipboard(&text) {
            self.push_log(
                format!("→ yanked {} line{} to clipboard", total, if total == 1 { "" } else { "s" })
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
            FocusPane::Details => {
                // Details no longer has a scrollable log; treat as no-op.
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
        match self.focus {
            FocusPane::JobLog => self.job_log_scroll = 0,
            _ => {}
        }
    }

    // ---------- background work ----------

    /// Spawn one task per node to probe reachability. Each task runs
    /// `ssh -G` (cheap, no connection) to resolve the node the way the
    /// real `ssh` would, then TCP-probes the resolved host:port. This
    /// makes the online badge honour `~/.ssh/config` (ProxyJump aside —
    /// we're still doing a direct TCP dial).
    fn refresh_reachability(&mut self) {
        // Flip every host into the "checking" state before spawning so
        // the UI shows the spinner on the very next frame instead of
        // waiting for the first TCP probe to return.
        for node in &self.nodes {
            self.status
                .entry(node.name.clone())
                .or_default()
                .checking_reachability = true;
        }
        // Snapshot the (name, hostname) pairs before the spawn loop
        // so the closure-capturing iteration doesn't hold an
        // immutable borrow of `self.nodes` while we mutably reborrow
        // `self.probe_tasks` via `track_probe` inside the body.
        let targets: Vec<(String, String)> = self
            .nodes
            .iter()
            .map(|n| (n.name.clone(), n.hostname.clone()))
            .collect();
        for (name, host) in targets {
            // Pass the full override so `ssh -G` sees any `-i`/`-o`
            // args the user set, not just the raw hostname.
            let override_ = self.override_for(&name).clone();
            let tx = self.status_tx.clone();
            let task_name = name.clone();
            let handle = tokio::spawn(async move {
                let r = host::check_online(&host, &override_).await;
                let _ = tx.send(StatusUpdate::Reachability(task_name, r)).await;
            });
            self.track_probe(handle);
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
                match profile.as_str() {
                    "system" => entry.checking_system = true,
                    "home" => entry.checking_home = true,
                    _ => {}
                }
            }
            entry.last_error = None;
        }

        let flake = self.flake.clone();
        let override_ = self.override_for(&node.name).clone();
        let askpass = self.askpass_env.clone();
        for profile in node.profiles.keys() {
            let profile = profile.clone();
            let node = node.clone();
            let flake = flake.clone();
            let override_ = override_.clone();
            let askpass = askpass.clone();
            let tx = self.status_tx.clone();
            let handle = tokio::spawn(async move {
                let result =
                    host::check_profile_up_to_date(&flake, &node, &profile, &override_, &askpass)
                        .await
                        .map_err(|e| format!("{e:#}"));
                let _ = tx
                    .send(StatusUpdate::UpdateProbe {
                        node: node.name.clone(),
                        profile,
                        result,
                    })
                    .await;
            });
            self.track_probe(handle);
        }
        self.push_log_tagged(
            format!("→ checking updates for {}", node.name).as_str(),
            false,
            Some(node.name.clone()),
        );
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
                let extra = match p.as_str() {
                    "system" => &status.system_extra,
                    "home" => &status.home_extra,
                    _ => return (p.clone(), None, None),
                };
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
            match profile.as_str() {
                "system" => entry.system_extra.checking_size = true,
                "home" => entry.home_extra.checking_size = true,
                _ => {}
            }
            let node_cloned = node.clone();
            let override_ = self.override_for(&node.name).clone();
            let askpass = self.askpass_env.clone();
            let tx = self.status_tx.clone();
            let profile_cloned = profile.clone();
            let flake_cloned = self.flake.clone();
            let handle = tokio::spawn(async move {
                let (prog_tx, mut prog_rx) = mpsc::channel::<String>(64);
                let forwarder_tx = tx.clone();
                let forwarder_node = node_cloned.name.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(line) = prog_rx.recv().await {
                        let _ = forwarder_tx
                            .send(StatusUpdate::LogLine {
                                node: forwarder_node.clone(),
                                text: line,
                                is_err: false,
                            })
                            .await;
                    }
                });
                let result = host::check_closure_sizes(
                    &flake_cloned,
                    &node_cloned,
                    &profile_cloned,
                    &local_path,
                    &remote_path,
                    &override_,
                    &askpass,
                    prog_tx,
                )
                .await
                .map_err(|e| format!("{e:#}"));
                // Drain the forwarder before publishing the final probe
                // result so the closing "[size] remote: …" line lands
                // before the inline sizes snap into place.
                let _ = forwarder.await;
                let _ = tx
                    .send(StatusUpdate::SizeProbe {
                        node: node_cloned.name.clone(),
                        profile: profile_cloned,
                        result,
                    })
                    .await;
            });
            self.track_probe(handle);
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
        match profile {
            "system" => entry.system_extra.checking_pkg = true,
            "home" => entry.home_extra.checking_pkg = true,
            _ => return,
        }
        let tx = self.status_tx.clone();
        let node_cloned = node.clone();
        let profile_cloned = profile.to_string();
        let override_ = self.override_for(&node.name).clone();
        let askpass = self.askpass_env.clone();
        let flake_cloned = self.flake.clone();
        let handle = tokio::spawn(async move {
            let (prog_tx, mut prog_rx) = mpsc::channel::<String>(64);
            let forwarder_tx = tx.clone();
            let forwarder_node = node_cloned.name.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(line) = prog_rx.recv().await {
                    let _ = forwarder_tx
                        .send(StatusUpdate::LogLine {
                            node: forwarder_node.clone(),
                            text: line,
                            is_err: false,
                        })
                        .await;
                }
            });

            let result = host::check_package_diff(
                &flake_cloned,
                &node_cloned,
                &profile_cloned,
                &local_path,
                &remote_path,
                &override_,
                &askpass,
                prog_tx,
            )
            .await
            .map_err(|e| format!("{e:#}"));
            // Drain the forwarder before publishing the final probe
            // result so the closing "[pkg] done" line lands before
            // the inline diff snaps into place.
            let _ = forwarder.await;
            let _ = tx
                .send(StatusUpdate::PkgDiffProbe {
                    node: node_cloned.name.clone(),
                    profile: profile_cloned,
                    result,
                })
                .await;
        });
        self.track_probe(handle);
        self.push_log_tagged(
            format!("→ computing package diff for {} ({profile})", node.name).as_str(),
            false,
            Some(node.name.clone()),
        );
    }

    fn apply_status(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Reachability(name, r) => {
                let entry = self.status.entry(name).or_default();
                entry.reachability = r;
                entry.checking_reachability = false;
                // Stamp the "last seen up" time on every successful
                // probe so the details pane can show something freshly
                // anchored ("up 3s ago") rather than the stale label
                // from whatever the previous sweep found.
                if r == Reachability::Online {
                    entry.last_online = Some(std::time::SystemTime::now());
                }
            }
            StatusUpdate::FlakeDiscover(new_nodes) => {
                // Merge newly discovered nodes into the running list.
                // Nodes already present keep all their accumulated
                // status (reachability, update checks, extras) — we
                // only append nodes that weren't known before.
                for node in new_nodes {
                    if !self.nodes.iter().any(|n| n.name == node.name) {
                        self.push_log(
                            &format!("→ new node discovered: {}", node.name),
                            false,
                        );
                        self.nodes.push(node);
                    }
                }
            }
            StatusUpdate::UpdateProbe {
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
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
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
                }
                match profile.as_str() {
                    "system" => {
                        entry.checking_system = false;
                        entry.system_update = state;
                    }
                    "home" => {
                        entry.checking_home = false;
                        entry.home_update = state;
                    }
                    _ => {}
                }
            }
            StatusUpdate::SizeProbe {
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
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
                    ex.checking_size = false;
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
                            entry.last_error = Some(e);
                        }
                    }
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
            StatusUpdate::PkgDiffProbe {
                node,
                profile,
                result,
            } => {
                let entry = self.status.entry(node).or_default();
                let extra = match profile.as_str() {
                    "system" => Some(&mut entry.system_extra),
                    "home" => Some(&mut entry.home_extra),
                    _ => None,
                };
                if let Some(ex) = extra {
                    ex.checking_pkg = false;
                    match result {
                        Ok(diff) => ex.pkg_diff = Some(diff),
                        Err(e) => {
                            ex.pkg_diff = None;
                            entry.last_error = Some(e);
                        }
                    }
                }
            }
            StatusUpdate::LogLine { node, text, is_err } => {
                self.push_log_tagged(&text, is_err, Some(node));
            }
        }
    }

    /// Build the candidate target list for `mode` and open the
    /// confirmation popup. Marked hosts win over the cursor selection,
    /// because that's the more deliberate action: if the user took the
    /// trouble to mark, that's what they want.
    fn request_deploy(&mut self, mode: Mode) {
        if self.deploy_task.is_some() {
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
        self.queue_mode = mode;
        self.queue_profile = profile;
        self.queue_total = hosts.len();
        self.queue_done = 0;
        self.deploy_queue = hosts.into_iter().collect();
        // Fresh run wipes the previous outcome and snaps both logs to
        // auto-tail so the user sees the new output in the details
        // pane and the job log pane simultaneously.
        self.last_deploy = None;
        self.log_scroll = 0;
        self.job_log_scroll = 0;
        self.visual_sel = None;
        self.start_next_in_queue();
    }

    /// Pop the next host from `deploy_queue` and spawn the deploy. Skips
    /// hosts that lack the requested profile (logs a warning) so a single
    /// bad target doesn't poison the whole batch.
    fn start_next_in_queue(&mut self) {
        // Drain hosts that turn out to be impossible up front so the
        // queue progress stays consistent (the user-visible total still
        // includes them — they're just counted as "done" with a skip).
        while let Some(name) = self.deploy_queue.pop_front() {
            let Some(node) = self.nodes.iter().find(|n| n.name == name).cloned() else {
                self.push_log_tagged(
                    format!("! unknown host {name} — skipped").as_str(),
                    true,
                    Some(name.clone()),
                );
                self.queue_done = self.queue_done.saturating_add(1);
                continue;
            };
            let profile = match self.queue_profile {
                ProfileSel::Home if !node.has_home() => {
                    self.push_log_tagged(
                        format!("! {name} has no home profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    self.queue_done = self.queue_done.saturating_add(1);
                    continue;
                }
                ProfileSel::System if !node.has_system() => {
                    self.push_log_tagged(
                        format!("! {name} has no system profile — skipped").as_str(),
                        true,
                        Some(name.clone()),
                    );
                    self.queue_done = self.queue_done.saturating_add(1);
                    continue;
                }
                other => other,
            };
            let req = DeployRequest {
                flake: self.flake.clone(),
                node: node.name.clone(),
                profile,
                mode: self.queue_mode,
                toggles: self.toggles,
                ssh_override: self.override_for(&node.name).clone(),
                askpass: self.askpass_env.clone(),
            };
            self.push_log_tagged(
                format!(
                    "→ deploy [{}/{}] {} ({}, {})",
                    self.queue_done + 1,
                    self.queue_total,
                    node.name,
                    describe_mode(self.queue_mode),
                    describe_profile(profile),
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
                self.cached_password.as_deref().map(|s| Zeroizing::new(s.clone()))
            } else {
                None
            };
            let handle = deploy::run(req, sudo_pw);
            self.deploy_rx = Some(handle.rx);
            self.deploy_task = Some(handle.task);
            self.deploy_stdin_tx = handle.stdin_tx;
            self.busy_label = if self.queue_total > 1 {
                Some(format!(
                    "deploying [{}/{}] {}",
                    self.queue_done + 1,
                    self.queue_total,
                    node.name
                ))
            } else {
                Some(format!("deploying {}", node.name))
            };
            self.current_target = Some(node.name);
            return;
        }
        // Queue drained without spawning anything (every host was a skip).
        self.queue_total = 0;
        self.queue_done = 0;
        self.current_target = None;
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

        if let Some(t) = self.deploy_task.take() {
            t.abort();
            self.deploy_rx = None;
            self.deploy_stdin_tx = None;
            self.clear_cached_password();

            self.busy_label = None;
            // Cancelling kills the queue too — otherwise pressing `x`
            // mid-batch would surprise-deploy the next host.
            let drained = self.deploy_queue.len();
            self.deploy_queue.clear();
            let target = self.current_target.clone();
            if drained > 0 {
                self.push_log_tagged(
                    format!("! deploy cancelled — dropped {drained} queued host(s)").as_str(),
                    true,
                    target.clone(),
                );
            } else {
                self.push_log_tagged("! deploy cancelled", true, target);
            }
            if let Some(node_name) = self.current_target.take() {
                let entry = LastDeploy {
                    node: node_name.clone(),
                    mode: self.queue_mode,
                    profile: self.queue_profile,
                    exit_code: -1,
                    ok: false,
                };
                self.last_deploys.insert(node_name, entry.clone());
                self.last_deploy = Some(entry);
            }
            self.queue_total = 0;
            self.queue_done = 0;
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
            s.checking_system = false;
            s.checking_home = false;
            s.system_extra.checking_size = false;
            s.home_extra.checking_size = false;
            s.system_extra.checking_pkg = false;
            s.home_extra.checking_pkg = false;
        }
        aborted
    }

    fn handle_deploy_line(&mut self, line: LogLine) {
        match line {
            LogLine::Stdout(s) => {
                let host = self.current_target.clone();
                self.push_log_tagged(&s, false, host);
            }
            LogLine::Stderr(s) => {
                let host = self.current_target.clone();
                self.push_log_tagged(&s, true, host);
            }
            LogLine::SudoPrompt(prompt) => {
                if let Some(ref pw) = self.cached_password {
                    if let Some(tx) = &self.deploy_stdin_tx {
                        let _ = tx.try_send(pw.to_string());
                    }
                } else {
                    self.input = InputMode::PasswordPrompt {
                        prompt,
                        buf: String::new(),
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
                // Snapshot the host before clearing `current_target` so
                // every follow-up log line and the per-host last-deploy
                // entry below can be tagged with it. The post-failure
                // "batch stopped" notice in particular has to be tagged
                // — otherwise the job-log pane filters it out and the
                // last visible line lags behind the details pane.
                let exit_host = self.current_target.take();
                self.push_log_tagged(&banner, !ok, exit_host.clone());
                self.deploy_task = None;
                self.deploy_rx = None;
                self.deploy_stdin_tx = None;
    
                if matches!(self.input, InputMode::PasswordPrompt { .. }) {
                    self.input = InputMode::Normal;
                }
                self.busy_label = None;
                if let Some(name) = exit_host.clone() {
                    let entry = LastDeploy {
                        node: name.clone(),
                        mode: self.queue_mode,
                        profile: self.queue_profile,
                        exit_code: code,
                        ok,
                    };
                    self.last_deploys.insert(name.clone(), entry.clone());
                    self.last_deploy = Some(entry);
                    if ok {
                        // Stale-update marks: a successful push
                        // invalidates the previously-cached probe.
                        // Wipe the per-profile extras too — their
                        // paths, sizes, and package diff were scoped
                        // to the *previous* closure and would
                        // otherwise linger in the details pane until
                        // the user re-ran `u`/`U`/`p`.
                        if let Some(s) = self.status.get_mut(&name) {
                            s.system_update = UpdateState::Unknown;
                            s.home_update = UpdateState::Unknown;
                            s.system_extra = ProfileExtra::default();
                            s.home_extra = ProfileExtra::default();
                        }
                    }
                }
                self.queue_done = self.queue_done.saturating_add(1);
                if ok {
                    // Drain the next host. If the queue is empty,
                    // start_next_in_queue resets the queue counters.
                    if !self.deploy_queue.is_empty() {
                        self.start_next_in_queue();
                    } else {
                        self.clear_cached_password();
                        self.queue_total = 0;
                        self.queue_done = 0;
                    }
                } else {
                    self.clear_cached_password();
                    // Stop the batch on failure — safer than blindly
                    // continuing to push to more hosts after one breaks.
                    let dropped = self.deploy_queue.len();
                    if dropped > 0 {
                        self.deploy_queue.clear();
                        self.push_log_tagged(
                            format!("! batch stopped after failure — {dropped} host(s) skipped")
                                .as_str(),
                            true,
                            exit_host,
                        );
                    }
                    self.queue_total = 0;
                    self.queue_done = 0;
                }
            }
            LogLine::Error(e) => {
                // Same snapshot-before-take pattern as Exit: we need
                // the host name for the spawn-failure banner, the
                // per-host last-deploy entry, and the post-failure
                // batch-stopped notice. All three want the same string.
                let err_host = self.current_target.take();
                self.push_log_tagged(
                    format!("! deploy spawn failed: {e}").as_str(),
                    true,
                    err_host.clone(),
                );
                self.deploy_task = None;
                self.deploy_rx = None;
                self.deploy_stdin_tx = None;
                self.clear_cached_password();

                if matches!(self.input, InputMode::PasswordPrompt { .. }) {
                    self.input = InputMode::Normal;
                }
                self.busy_label = None;
                if let Some(name) = err_host.clone() {
                    let entry = LastDeploy {
                        node: name.clone(),
                        mode: self.queue_mode,
                        profile: self.queue_profile,
                        exit_code: -1,
                        ok: false,
                    };
                    self.last_deploys.insert(name, entry.clone());
                    self.last_deploy = Some(entry);
                }
                let dropped = self.deploy_queue.len();
                self.deploy_queue.clear();
                if dropped > 0 {
                    self.push_log_tagged(
                        format!("! batch stopped — {dropped} host(s) skipped").as_str(),
                        true,
                        err_host,
                    );
                }
                self.queue_total = 0;
                self.queue_done = 0;
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
        self.log.push(LogEntry {
            text: text.to_string(),
            is_err,
            host,
        });
        // Cap so we don't grow forever during long sessions.
        const MAX: usize = 2000;
        if self.log.len() > MAX {
            let drop = self.log.len() - MAX;
            self.log.drain(0..drop);
        }
    }
}

/// Receive from an `Option<Receiver<T>>`. Returns `None` (i.e. the branch
/// stays pending) when the option is empty, so `select!` can ignore it.
async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
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

fn describe_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Switch => "switch",
        Mode::Boot => "boot",
        Mode::DryRun => "dry-run",
    }
}

fn describe_profile(p: ProfileSel) -> &'static str {
    match p {
        ProfileSel::All => "all",
        ProfileSel::System => "system",
        ProfileSel::Home => "home",
    }
}

/// Try to write `text` to the system clipboard. Attempts `wl-copy` (Wayland),
/// then `xclip`, then `xsel` (X11), then `pbcopy` (macOS). Returns `true` if
/// any tool succeeded.
fn yank_to_clipboard(text: &str) -> bool {
    use std::io::Write as _;
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for &(cmd, args) in candidates {
        let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::flake::{Node, Profile};

    fn sample_nodes() -> Vec<Node> {
        let mut profiles = BTreeMap::new();
        profiles.insert("system".into(), Profile { user: None });
        profiles.insert("home".into(), Profile { user: Some("jd".into()) });
        vec![
            Node {
                name: "alpha".into(),
                hostname: "alpha.lan".into(),
                ssh_user: Some("root".into()),
                profiles: profiles.clone(),
            },
            Node {
                name: "beta".into(),
                hostname: "beta.lan".into(),
                ssh_user: None,
                profiles: {
                    let mut p = BTreeMap::new();
                    p.insert("system".into(), Profile { user: None });
                    p
                },
            },
            Node {
                name: "gamma".into(),
                hostname: "gamma.lan".into(),
                ssh_user: None,
                profiles: BTreeMap::new(),
            },
        ]
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
        assert!(app.deploy_rx.is_none());
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
        assert_eq!(st.system_update, UpdateState::Unknown);
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
        assert_eq!(describe_mode(Mode::Switch), "switch");
        assert_eq!(describe_mode(Mode::Boot), "boot");
        assert_eq!(describe_mode(Mode::DryRun), "dry-run");
    }

    #[test]
    fn describe_profile_labels() {
        assert_eq!(describe_profile(ProfileSel::All), "all");
        assert_eq!(describe_profile(ProfileSel::System), "system");
        assert_eq!(describe_profile(ProfileSel::Home), "home");
    }

    #[test]
    fn focus_pane_rows() {
        assert_eq!(FocusPane::Toggles.row(), 0);
        assert_eq!(FocusPane::Hosts.row(), 1);
        assert_eq!(FocusPane::Details.row(), 1);
        assert_eq!(FocusPane::JobLog.row(), 1);
        assert_eq!(FocusPane::Commands.row(), 2);
    }

    #[test]
    fn command_pane_entries() {
        // Smoke test: at least verify the pane has the expected commands
        // and that indices match expectations for the nav cursor.
        assert!(COMMANDS.len() >= 10);
        assert_eq!(COMMANDS[0].0, Command::Refresh);
        assert_eq!(COMMANDS[0].1, "r");
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

    #[test]
    fn handle_key_mode_selection() {
        let mut app = App::new(".".into(), sample_nodes());
        // Profile selection keys.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::All);

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::System);

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.profile_sel, ProfileSel::Home);
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
        app.log_search_target = Some(SearchTarget::JobLog);
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
        app.log_search_target = Some(SearchTarget::JobLog);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.log_search.is_none());
    }

    // ---- PasswordPrompt input mode ----

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
            buf: String::new(),
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
}
