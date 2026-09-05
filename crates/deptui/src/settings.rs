//! deptui's client-side settings — the TUI's first persistent file.
//!
//! `~/.config/deptui/config.toml` (`$XDG_CONFIG_HOME` honoured) holds
//! named agent connections. A missing file is a normal state, not an
//! error: the agent screen simply reports that nothing is configured.
//!
//! ```toml
//! default_agent = "homelab"
//!
//! [agents.homelab]
//! ssh = "me@deploy-box"
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Which agent the view opens on when several are configured.
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentEntry>,
    /// Why the file failed to load, when it existed but didn't parse.
    /// Carried into the agent view so a typo'd config produces an
    /// on-screen explanation instead of a silently empty agent list.
    #[serde(skip)]
    pub load_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEntry {
    /// SSH destination reaching the agent host, e.g. `me@deploy-box`.
    /// The TUI runs `ssh <this> deptui-agent <verb> --json`.
    pub ssh: String,
}

impl Settings {
    pub fn config_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".config")
            });
        base.join("deptui").join("config.toml")
    }

    /// Load the settings file. Missing → defaults; unreadable/invalid →
    /// defaults plus a tracing warning (the TUI must still start).
    pub fn load() -> Self {
        let path = Self::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                return Self {
                    load_error: Some(format!("{}: {e}", path.display())),
                    ..Self::default()
                };
            }
        };
        match toml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("parsing {}: {e}", path.display());
                Self {
                    load_error: Some(format!("{}: {e}", path.display())),
                    ..Self::default()
                }
            }
        }
    }

    /// `(name, ssh)` pairs, the default agent first, the rest in name
    /// order — the order the agent view's `[`/`]` cycling walks.
    pub fn agent_list(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(def) = &self.default_agent {
            if let Some(e) = self.agents.get(def) {
                out.push((def.clone(), e.ssh.clone()));
            }
        }
        for (name, e) in &self.agents {
            if Some(name) != self.default_agent.as_ref() {
                out.push((name.clone(), e.ssh.clone()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_sorts_first() {
        let s: Settings = toml::from_str(
            r#"
default_agent = "zeta"
[agents.alpha]
ssh = "a@a"
[agents.zeta]
ssh = "z@z"
"#,
        )
        .unwrap();
        let list = s.agent_list();
        assert_eq!(list[0].0, "zeta");
        assert_eq!(list[1].0, "alpha");
    }

    #[test]
    fn parse_error_is_carried_not_swallowed() {
        // load() itself needs a file; the parse path is what matters.
        let s: Result<Settings, _> = toml::from_str("agents = 5");
        assert!(s.is_err());
    }

    #[test]
    fn missing_default_is_fine() {
        let s: Settings = toml::from_str("[agents.only]\nssh = \"o@o\"\n").unwrap();
        assert_eq!(s.agent_list(), vec![("only".into(), "o@o".into())]);
        assert!(Settings::default().agent_list().is_empty());
    }
}
