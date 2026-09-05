# Domain glossary

Names the concepts deptui is built around, so code, docs, and reviews
use one vocabulary. Each entry notes the module that owns the concept.

- **Node** (`flake.rs`): one host declared under `deploy.nodes` in the
  target flake, with its hostname, ssh user, and profiles.
- **Profile** (`flake.rs` / `host.rs`): one deployable unit of a node
  (`system`, `home`, or anything else the flake declares). Per-profile
  runtime state lives in a `ProfileStatus` entry keyed by profile name —
  never in per-name field pairs.
- **Probe** (`probe.rs`): one background check against a host —
  reachability, update, closure size, package diff, build plan, or
  substituter drift. Spawned through the single `probe::spawn`
  interface; reports progress lines then exactly one typed `Report`.
- **Report** (`probe.rs`): the one message shape every probe sends back
  over the status channel. `apply_probe` in `app.rs` folds reports into
  state and owns probe *policy* (invalidation + chaining).
- **Progress line** (`host.rs::ProgressLine`): a typed job-log line a
  probe emits while running. Its constructors format the canonical text
  and the `LogKind` from the same values, so text and type can't drift.
- **LogKind** (`host.rs`): the producer-set classification of a job-log
  line (`Plain`, `Note`, `SizeLocal`, `SizeRemote`, `Pkg`, `PkgDone`).
  The renderer styles from it; it never re-parses prose.
- **Package diff / PkgChange** (`host.rs`): the typed result of
  comparing two closures by parsed `<name, version>`; the
  `ContentOnly` variants cover "same versions, different store paths".
- **Job-log window** (`joblog.rs`): the filtered, scrolled view of the
  log the right-hand pane shows. The host filter and the char-selection
  column bounds live here once, shared by key handling and rendering.
- **Deploy session** (`app.rs::DeploySession`): the state of a running
  deploy batch — child plumbing, current host, and the remaining queue
  with its confirmed mode/profile and progress. Exists exactly while a
  deploy runs; teardown takes the whole session.
- **Build plan** (`host.rs::BuildPlan`): what a deploy would compile and
  fetch, from `nix build --dry-run` mirrored against the store that
  will actually build (local or remote).
- **Substituter drift** (`host.rs::SubstituterDrift`): caches the new
  closure declares that the *building* store can't use yet; the trigger
  for cache seeding.
- **Seeding** (`host.rs::seed_substituters`): additively copying store
  paths into the target before a build, instead of touching its
  `nix.conf`.
- **SSH override** (`ssh.rs::SshOverride`): per-host connection
  overrides (hostname, user, identity, extra opts) applied identically
  to probes and deploys.
- **Toggle** (`deploy.rs::TOGGLES`): a user-flippable deploy-rs flag;
  the table is the single source for name, strip label, help text, and
  accessors.
