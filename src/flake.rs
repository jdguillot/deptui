//! Flake discovery — talks to `nix eval` to enumerate `deploy.nodes`.
//!
//! We deliberately stay shallow: we read only `hostname`, `sshUser`,
//! `profilesOrder`, and each profile's `user`/`sshUser`. Touching `path` would
//! force evaluation of the full NixOS / home-manager configurations, which is
//! slow and not needed to draw the host list.

use std::collections::BTreeMap;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::process::Command;

/// One profile (e.g. `system`, `home`) attached to a node.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    /// The user the profile is *activated* as. deploy-rs sudos to this
    /// from the ssh login user when the two differ.
    #[serde(default)]
    pub user: Option<String>,
    /// Per-profile ssh login user, overriding the node-level one.
    #[serde(default, rename = "sshUser")]
    pub ssh_user: Option<String>,
}

/// One entry in `deploy.nodes`.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    /// The attribute name in `deploy.nodes`.
    #[serde(skip)]
    pub name: String,
    /// The hostname deploy-rs will SSH to.
    pub hostname: String,
    /// SSH user at the node level (profiles can override).
    #[serde(default, rename = "sshUser")]
    pub ssh_user: Option<String>,
    /// Profile attrs keyed by name (`system`, `home`, …).
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// deploy-rs's `profilesOrder`, when the node sets one. `profiles` is a
    /// `BTreeMap`, so without this the order would be alphabetical --
    /// "home" before "system", the reverse of what deploy-rs does.
    #[serde(default, rename = "profilesOrder")]
    pub profiles_order: Option<Vec<String>>,
}

impl Node {
    /// True if a `system` profile is present (NixOS / "host config").
    pub fn has_system(&self) -> bool {
        self.profiles.contains_key("system")
    }

    /// True if a `home` profile is present (home-manager).
    pub fn has_home(&self) -> bool {
        self.profiles.contains_key("home")
    }

    /// Profile names in the order deploy-rs will push them: the node's
    /// `profilesOrder` first (minus anything it names that doesn't exist),
    /// then any remaining profiles. Without an explicit order we put
    /// `system` first, matching deploy-rs's own convention of bringing the
    /// host up before touching a user's home.
    pub fn ordered_profiles(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(order) = &self.profiles_order {
            for name in order {
                if self.profiles.contains_key(name) && !out.contains(name) {
                    out.push(name.clone());
                }
            }
        } else if self.profiles.contains_key("system") {
            out.push("system".to_string());
        }
        for name in self.profiles.keys() {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_system_true() {
        let mut profiles = BTreeMap::new();
        profiles.insert("system".into(), Profile { user: None, ssh_user: None });
        let node = Node {
            name: "host".into(),
            hostname: "host".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        };
        assert!(node.has_system());
        assert!(!node.has_home());
    }

    #[test]
    fn has_home_true() {
        let mut profiles = BTreeMap::new();
        profiles.insert("home".into(), Profile { user: Some("jd".into()), ssh_user: None });
        let node = Node {
            name: "host".into(),
            hostname: "host".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        };
        assert!(!node.has_system());
        assert!(node.has_home());
    }

    #[test]
    fn has_both() {
        let mut profiles = BTreeMap::new();
        profiles.insert("system".into(), Profile { user: None, ssh_user: None });
        profiles.insert("home".into(), Profile { user: Some("jd".into()), ssh_user: None });
        let node = Node {
            name: "host".into(),
            hostname: "host".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        };
        assert!(node.has_system());
        assert!(node.has_home());
    }

    #[test]
    fn ordered_profiles_defaults_system_first() {
        // BTreeMap would yield home, system — deploy-rs does the reverse.
        let mut profiles = BTreeMap::new();
        profiles.insert("system".into(), Profile { user: None, ssh_user: None });
        profiles.insert("home".into(), Profile { user: Some("jd".into()), ssh_user: None });
        let node = Node {
            name: "host".into(),
            hostname: "host".into(),
            ssh_user: None,
            profiles,
            profiles_order: None,
        };
        assert_eq!(node.ordered_profiles(), vec!["system", "home"]);
    }

    #[test]
    fn ordered_profiles_honours_explicit_order() {
        let mut profiles = BTreeMap::new();
        profiles.insert("system".into(), Profile { user: None, ssh_user: None });
        profiles.insert("home".into(), Profile { user: Some("jd".into()), ssh_user: None });
        profiles.insert("extra".into(), Profile { user: None, ssh_user: None });
        let node = Node {
            name: "host".into(),
            hostname: "host".into(),
            ssh_user: None,
            // Names a profile that doesn't exist; it should be skipped, and
            // the undeclared `extra` appended rather than dropped.
            profiles_order: Some(vec!["home".into(), "nope".into(), "system".into()]),
            profiles,
        };
        assert_eq!(node.ordered_profiles(), vec!["home", "system", "extra"]);
    }

    #[test]
    fn deserialize_node_json() {
        let json = r#"{
            "hostname": "myhost.example.com",
            "sshUser": "root",
            "profilesOrder": ["system", "home"],
            "profiles": {
                "system": { "user": null },
                "home": { "user": "jd", "sshUser": "jd" }
            }
        }"#;
        let node: Node = serde_json::from_str(json).unwrap();
        assert_eq!(node.hostname, "myhost.example.com");
        assert_eq!(node.ssh_user.as_deref(), Some("root"));
        assert!(node.has_system());
        assert!(node.has_home());
        assert_eq!(
            node.profiles.get("home").unwrap().user.as_deref(),
            Some("jd")
        );
        assert_eq!(
            node.profiles.get("home").unwrap().ssh_user.as_deref(),
            Some("jd")
        );
        assert_eq!(node.ordered_profiles(), vec!["system", "home"]);
        // name is skip(deserializing) so it stays empty.
        assert!(node.name.is_empty());
    }

    #[test]
    fn deserialize_minimal_node() {
        let json = r#"{ "hostname": "h" }"#;
        let node: Node = serde_json::from_str(json).unwrap();
        assert_eq!(node.hostname, "h");
        assert_eq!(node.ssh_user, None);
        assert!(node.profiles.is_empty());
    }

    #[test]
    fn deserialize_nodes_map() {
        let json = r#"{
            "alpha": { "hostname": "alpha.lan" },
            "beta":  { "hostname": "beta.lan", "sshUser": "root", "profiles": { "system": {} } }
        }"#;
        let raw: BTreeMap<String, Node> = serde_json::from_str(json).unwrap();
        let nodes: Vec<Node> = raw
            .into_iter()
            .map(|(name, mut node)| { node.name = name; node })
            .collect();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "alpha");
        assert_eq!(nodes[1].name, "beta");
        assert!(nodes[1].has_system());
    }
}

/// Run `nix eval --json` on the flake and parse the resulting attrset.
pub async fn discover(flake: &str) -> Result<Vec<Node>> {
    // Apply function strips the heavy `path` derivations and keeps only the
    // metadata we render. Doing this in Nix avoids forcing evaluation of
    // the per-host modules.
    let apply = r#"nodes: builtins.mapAttrs (n: v: {
      hostname = v.hostname;
      sshUser = v.sshUser or null;
      profilesOrder = v.profilesOrder or null;
      profiles = builtins.mapAttrs (pn: pv: {
        user = pv.user or null;
        sshUser = pv.sshUser or null;
      }) v.profiles;
    }) nodes"#;

    let target = format!("{flake}#deploy.nodes");
    let output = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-warn-dirty",
            &target,
            "--apply",
            apply,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .context("spawning `nix eval`")?;

    if !output.status.success() {
        return Err(anyhow!(
            "`nix eval {target}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let raw: BTreeMap<String, Node> =
        serde_json::from_slice(&output.stdout).context("parsing `nix eval` JSON output")?;

    Ok(raw
        .into_iter()
        .map(|(name, mut node)| {
            node.name = name;
            node
        })
        .collect())
}
