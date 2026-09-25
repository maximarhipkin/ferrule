//! M17: a running process follows its config file, so a server added by
//! `ferrule mcp add` (or by hand) starts without a restart. Every
//! `SYNC_EVERY` the file's modification time and length are checked; when
//! they change it is parsed again, new `[secrets]` are bound into the
//! credential proxy, and the manager is handed the new `[[mcp.servers]]`.
//! A poll, not a file watch: it behaves the same on every OS and around
//! editors' atomic renames. Design: `docs/m17-mcp-add.md` §2.

use crate::config::Config;
use ferrule_extensions::manager::SYNC_EVERY;
use ferrule_extensions::ExtensionManager;
use ferrule_proxy::{Broker, SecretRule};
use ferrule_sandbox::Egress;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::SystemTime;

type Lookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What the process runs with: a secret's rule and its value, if it has one.
type Bound = BTreeMap<String, (SecretRule, Option<String>)>;

pub struct Follower {
    manager: Weak<ExtensionManager>,
    path: PathBuf,
    /// Only a trusted file (`--config`/`$FERRULE_CONFIG`, the global one)
    /// may start servers; a `./ferrule.toml` may be the agent's to write.
    trusted: bool,
    seen: Option<(SystemTime, u64)>,
    secrets: Bound,
    broker: Option<&'static Broker>,
    lookup: Lookup,
    warned_untrusted: bool,
}

/// Start following `path`, whose contents `cfg` are what the process
/// started with. The task ends with the manager.
pub fn spawn(manager: &Arc<ExtensionManager>, cfg: &Config, path: &Path, trusted: bool) {
    let broker = match crate::shared_broker(cfg) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("config follower: no credential proxy: {e:#}");
            None
        }
    };
    let mut f = Follower::new(manager, cfg, path, trusted, broker, Box::new(secret_value));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            if !f.tick().await {
                return;
            }
        }
    });
}

/// A secret's value as a newly started process would see it: the
/// environment, then the secrets file, read directly (`set_var` isn't
/// thread-safe, so the file can't be loaded into the env again).
fn secret_value(name: &str) -> Option<String> {
    if let Some(v) = std::env::var(name).ok().filter(|v| !v.is_empty()) {
        return Some(v);
    }
    let path = crate::secrets::path().ok()?;
    crate::secrets::read(&path)
        .ok()?
        .into_iter()
        .find(|(n, v)| n == name && !v.is_empty())
        .map(|(_, v)| v)
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl Follower {
    pub fn new(
        manager: &Arc<ExtensionManager>,
        cfg: &Config,
        path: &Path,
        trusted: bool,
        broker: Option<&'static Broker>,
        lookup: Lookup,
    ) -> Self {
        let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut f = Self {
            manager: Arc::downgrade(manager),
            seen: stamp(&path),
            path,
            trusted,
            secrets: Bound::new(),
            broker,
            lookup,
            warned_untrusted: false,
        };
        f.secrets = f.wanted(cfg);
        f
    }

    fn wanted(&self, cfg: &Config) -> Bound {
        cfg.secrets
            .iter()
            .map(|(name, spec)| (name.clone(), (spec.into(), (self.lookup)(name))))
            .collect()
    }

    /// One look at the file. `false` once there is nothing left to follow.
    pub async fn tick(&mut self) -> bool {
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        let now = stamp(&self.path);
        if now.is_none() || now == self.seen {
            return true;
        }
        self.seen = now;
        if !self.trusted {
            if !self.warned_untrusted {
                tracing::warn!(
                    "{} changed; MCP servers and secrets from a config in the working directory \
                     take a restart (only --config, $FERRULE_CONFIG or the global config are followed live)",
                    self.path.display()
                );
                self.warned_untrusted = true;
            }
            return true;
        }
        let cfg = match std::fs::read_to_string(&self.path)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(toml::from_str::<Config>(&t)?))
        {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    "{} changed but doesn't parse, keeping the running setup: {e:#}",
                    self.path.display()
                );
                return true;
            }
        };
        self.apply_secrets(&manager, &cfg);
        for change in manager.set_configured(crate::mcp_servers(&cfg)).await {
            tracing::info!("config changed: mcp server {change}");
        }
        true
    }

    /// Bind the secrets that are new, or whose rule or value changed, and
    /// give servers started from now on the proxy's env. Removing one takes
    /// a restart.
    fn apply_secrets(&mut self, manager: &ExtensionManager, cfg: &Config) {
        let wanted = self.wanted(cfg);
        let fresh: Vec<(&String, &SecretRule, &String)> = wanted
            .iter()
            .filter(|(name, v)| self.secrets.get(*name) != Some(*v))
            .filter_map(|(name, (rule, value))| value.as_ref().map(|v| (name, rule, v)))
            .collect();
        if fresh.is_empty() {
            self.secrets = wanted;
            return;
        }
        match self.broker {
            Some(broker) => {
                for (name, rule, value) in &fresh {
                    match broker.bind(name, rule, value) {
                        Ok(_) => tracing::info!("config changed: secret `{name}` bound"),
                        Err(e) => {
                            tracing::warn!("config changed: secret `{name}` not bound: {e:#}")
                        }
                    }
                }
            }
            None => {
                let started = crate::broker_config(cfg).and_then(|c| {
                    Broker::start(c, |name| wanted.get(name).and_then(|(_, v)| v.clone()))
                });
                match started {
                    // One per process at most, kept for its lifetime like the first.
                    Ok(Some(b)) => {
                        tracing::info!("config changed: credential proxy started for [secrets]");
                        self.broker = Some(Box::leak(Box::new(b)));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("config changed: no credential proxy: {e:#}"),
                }
            }
        }
        self.secrets = wanted;
        if let Some(broker) = self.broker {
            match std::fs::read_to_string(broker.ca_cert_path()) {
                Ok(ca_cert_pem) => {
                    let sandbox = manager
                        .sandbox()
                        .as_ref()
                        .clone()
                        .with_env(broker.child_env())
                        .with_egress(Some(Egress {
                            proxy_url: broker.proxy_url(),
                            ca_cert_pem,
                        }));
                    manager.set_sandbox(Arc::new(sandbox));
                }
                Err(e) => tracing::warn!(
                    "reading {}: {e}; new servers won't have the new secrets",
                    broker.ca_cert_path().display()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_extensions::{AllowList, Layout, ManagerConfig};
    use ferrule_sandbox::Sandbox;

    const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    if msg.get("method") == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "0"}}
    elif msg.get("method") == "tools/list":
        r = {"tools": [{"name": n, "description": "A tool.", "inputSchema": {"type": "object"}} for n in ("echo", "other")]}
    elif mid is None:
        continue
    else:
        r = {"content": [{"type": "text", "text": "ok"}]}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}) + "\n")
    sys.stdout.flush()
"#;

    fn manager(dir: &Path) -> Arc<ExtensionManager> {
        ExtensionManager::new(ManagerConfig {
            allow: AllowList::new::<String>(&[]),
            layout: Layout::new(dir.join("data")),
            sandbox: Arc::new(Sandbox::off()),
            workspace: dir.to_path_buf(),
            skills: None,
        })
    }

    fn names(m: &ExtensionManager) -> Vec<String> {
        use ferrule_core::ToolSource;
        let mut n: Vec<_> = m.tools().iter().map(|t| t.definition().name).collect();
        n.sort();
        n
    }

    /// Rewrite the config so the stamp surely moves (same-second writes
    /// can share an mtime; the length changes with the padding).
    fn write(path: &Path, text: &str, pad: usize) {
        std::fs::write(path, format!("{text}\n{}\n", "#".repeat(pad))).unwrap();
    }

    fn server(script: &Path, name: &str, extra: &str) -> String {
        format!(
            "[[mcp.servers]]\nname = \"{name}\"\ncommand = \"python3\"\nargs = [{:?}]\nsandbox = false\n{extra}\n",
            script.display().to_string()
        )
    }

    #[tokio::test]
    async fn a_trusted_config_is_followed_and_an_untrusted_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(dir.path()).unwrap();
        let script = dir.join("s.py");
        std::fs::write(&script, SERVER).unwrap();
        let path = dir.join("config.toml");
        write(&path, "# mine\n", 0);
        let cfg: Config = toml::from_str("").unwrap();

        let m = manager(&dir);
        m.start(vec![]).await;
        let mut f = Follower::new(&m, &cfg, &path, true, None, Box::new(|_| None));
        assert!(f.tick().await);
        assert!(names(&m).is_empty(), "unchanged file, nothing to do");

        write(&path, &server(&script, "late", ""), 1);
        f.tick().await;
        assert_eq!(names(&m), ["mcp__late__echo", "mcp__late__other"]);

        write(&path, "this is not toml [", 2);
        f.tick().await;
        assert_eq!(names(&m).len(), 2, "a broken file keeps what runs");

        write(
            &path,
            &server(&script, "late", "enabled_tools = [\"echo\"]"),
            3,
        );
        f.tick().await;
        assert_eq!(names(&m), ["mcp__late__echo"]);

        write(&path, "# gone\n", 4);
        f.tick().await;
        assert!(names(&m).is_empty());

        let u = manager(&dir.join("u"));
        u.start(vec![]).await;
        let mut f = Follower::new(&u, &cfg, &path, false, None, Box::new(|_| None));
        write(&path, &server(&script, "late", ""), 5);
        assert!(f.tick().await);
        assert!(
            names(&u).is_empty(),
            "./ferrule.toml can't start servers live"
        );

        drop(u);
        assert!(!f.tick().await, "the follower ends with its manager");
        m.shutdown_all().await;
    }
}
