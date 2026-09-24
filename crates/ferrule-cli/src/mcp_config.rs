//! M17: `[[mcp.servers]]` entries written and removed through `toml_edit`,
//! so the rest of the owner's file keeps its comments and order. Used by
//! `ferrule mcp add`/`remove` and the setup wizard's MCP step.

use crate::config;
use crate::setup::{table, Target};
use anyhow::{anyhow, bail, Result};
use ferrule_mcp::McpServerConfig;
use std::collections::HashMap;
use std::path::PathBuf;
use toml_edit::{Array, ArrayOfTables, InlineTable, Item, Table, TableLike, Value};

/// A server's entry as it's written: the keys in a fixed, readable order,
/// defaults left out.
pub fn server_table(cfg: &McpServerConfig) -> Table {
    let mut t = Table::new();
    t.insert("name", toml_edit::value(&cfg.name));
    match &cfg.url {
        Some(url) => {
            t.insert("url", toml_edit::value(url));
        }
        None => {
            t.insert("command", toml_edit::value(&cfg.command));
        }
    }
    let strings = |v: &[String]| Item::Value(Array::from_iter(v.iter().map(String::as_str)).into());
    if !cfg.args.is_empty() {
        t.insert("args", strings(&cfg.args));
    }
    let map = |m: &HashMap<String, String>| {
        let mut keys: Vec<_> = m.keys().collect();
        keys.sort();
        let mut i = InlineTable::new();
        for k in keys {
            i.insert(k, m[k].as_str().into());
        }
        Item::Value(i.into())
    };
    if !cfg.env.is_empty() {
        t.insert("env", map(&cfg.env));
    }
    if !cfg.headers.is_empty() {
        t.insert("headers", map(&cfg.headers));
    }
    if let Some(secs) = cfg.timeout_secs {
        t.insert("timeout_secs", toml_edit::value(secs as i64));
    }
    if !cfg.sandbox {
        t.insert("sandbox", toml_edit::value(false));
    }
    if !cfg.writable_roots.is_empty() {
        let roots: Vec<String> = cfg
            .writable_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        t.insert("writable_roots", strings(&roots));
    }
    if !cfg.env_remove.is_empty() {
        t.insert("env_remove", strings(&cfg.env_remove));
    }
    if !cfg.hide_args.is_empty() {
        t.insert("hide_args", strings(&cfg.hide_args));
    }
    if !cfg.enabled_tools.is_empty() {
        t.insert("enabled_tools", strings(&cfg.enabled_tools));
    }
    if let Some(n) = cfg.max_output_chars {
        t.insert("max_output_chars", toml_edit::value(n as i64));
    }
    if !cfg.output_caps.is_empty() {
        let mut keys: Vec<_> = cfg.output_caps.keys().collect();
        keys.sort();
        let mut i = InlineTable::new();
        for k in keys {
            i.insert(k, (cfg.output_caps[k] as i64).into());
        }
        t.insert("output_caps", Item::Value(i.into()));
    }
    t
}

/// The `mcp.servers` array, created where missing. An inline
/// `servers = [{…}]` is edited as it is.
enum Servers<'a> {
    Tables(&'a mut ArrayOfTables),
    Inline(&'a mut Array),
}

fn servers(root: &mut dyn TableLike) -> Result<Servers<'_>> {
    let mcp = table(root, &["mcp"])?;
    let item = mcp
        .entry("servers")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    match item {
        Item::ArrayOfTables(a) => Ok(Servers::Tables(a)),
        Item::Value(Value::Array(a)) => Ok(Servers::Inline(a)),
        _ => Err(anyhow!(
            "`mcp.servers` in the config isn't a list of tables"
        )),
    }
}

fn name_of(t: &dyn TableLike) -> Option<&str> {
    t.get("name").and_then(Item::as_str)
}

/// Add `cfg` after the last `[[mcp.servers]]` entry, or, with `replace`, put
/// it where the entry of that name is (keeping that entry's comments).
pub fn add_server(root: &mut dyn TableLike, cfg: &McpServerConfig, replace: bool) -> Result<()> {
    let new = server_table(cfg);
    match servers(root)? {
        Servers::Tables(list) => {
            let at = list
                .iter()
                .position(|t| name_of(t) == Some(cfg.name.as_str()));
            match at {
                Some(_) if !replace => bail!("`{}` is already in [[mcp.servers]]", cfg.name),
                Some(i) => {
                    let old = list.get_mut(i).expect("found above");
                    let mut new = new;
                    *new.decor_mut() = old.decor().clone();
                    if let Some(pos) = old.position() {
                        new.set_position(pos);
                    }
                    *old = new;
                }
                None => list.push(new),
            }
        }
        Servers::Inline(list) => {
            let at = list.iter().position(|v| {
                v.as_inline_table()
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
                    == Some(cfg.name.as_str())
            });
            let new = Value::InlineTable(new.into_inline_table());
            match at {
                Some(_) if !replace => bail!("`{}` is already in mcp.servers", cfg.name),
                Some(i) => {
                    list.replace(i, new);
                }
                None => list.push_formatted(new),
            }
        }
    }
    Ok(())
}

/// Drop the entry named `name`. `false`: there was none.
pub fn remove_server(root: &mut dyn TableLike, name: &str) -> Result<bool> {
    let Some(mcp) = root.get_mut("mcp").and_then(Item::as_table_like_mut) else {
        return Ok(false);
    };
    let Some(item) = mcp.get_mut("servers") else {
        return Ok(false);
    };
    let found = match item {
        Item::ArrayOfTables(list) => {
            let at = list.iter().position(|t| name_of(t) == Some(name));
            if let Some(i) = at {
                list.remove(i);
            }
            at.is_some()
        }
        Item::Value(Value::Array(list)) => {
            let at = list.iter().position(|v| {
                v.as_inline_table()
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            });
            if let Some(i) = at {
                list.remove(i);
            }
            at.is_some()
        }
        _ => bail!("`mcp.servers` in the config isn't a list of tables"),
    };
    Ok(found)
}

/// The file `ferrule mcp add` edits: the one `Config::load` reads, else a
/// new global config.
pub fn config_file() -> Result<PathBuf> {
    match config::config_path()? {
        Some(path) => Ok(path),
        None => config::global_config_path(),
    }
}

/// The `${VAR}`s a server's headers send: the `[secrets]` it uses by name.
/// (A stdio server gets every placeholder in its env, so it names none.)
pub fn secret_refs(cfg: &McpServerConfig) -> Vec<String> {
    let mut refs = Vec::new();
    for value in cfg.headers.values() {
        let mut rest = value.as_str();
        while let Some(start) = rest.find("${") {
            rest = &rest[start + 2..];
            let Some(end) = rest.find('}') else { break };
            let name = &rest[..end];
            if !refs.iter().any(|r| r == name) {
                refs.push(name.to_string());
            }
            rest = &rest[end + 1..];
        }
    }
    refs.sort();
    refs
}

/// What removing a configured server left behind on purpose.
pub struct Removed {
    pub path: PathBuf,
    /// `[secrets]` its headers used that no other server's headers do; they
    /// stay, since commands may use them too.
    pub secrets_kept: Vec<String>,
}

/// Remove the configured server `name` from `path`, and with `purge` its
/// state dir. `None`: the config has no such server.
pub fn remove_configured(path: PathBuf, name: &str, purge: bool) -> Result<Option<Removed>> {
    let mut t = Target::load(path)?;
    let cfg = t.config()?;
    let Some(gone) = cfg.mcp.servers.iter().find(|s| s.name == name) else {
        return Ok(None);
    };
    let others: Vec<String> = cfg
        .mcp
        .servers
        .iter()
        .filter(|s| s.name != name)
        .flat_map(secret_refs)
        .collect();
    let secrets_kept = secret_refs(gone)
        .into_iter()
        .filter(|r| cfg.secrets.contains_key(r) && !others.contains(r))
        .collect();
    remove_server(t.root(), name)?;
    t.save()?;
    if purge {
        let dir = config::data_dir()?
            .join("mcp")
            .join(ferrule_extensions::layout::mcp_dir_name(name));
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(anyhow!("removing {}: {e}", dir.display()))
            }
            _ => {}
        }
    }
    Ok(Some(Removed {
        path: t.path,
        secrets_kept,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::write_secret_hosts;

    const MINE: &str = "\
# my config
default_provider = \"a\"   # keep

[providers.a]
base_url = \"http://x\"
api_key_env = \"A_KEY\"
model = \"m\"

# the filesystem server
[[mcp.servers]]
name = \"fs\"   # first
command = \"npx\"
args = [\"-y\", \"fs\"]

[[mcp.servers]]
name = \"old\"
command = \"old-server\"

# comes last
[sandbox]
mode = \"read-only\"
";

    fn target(dir: &std::path::Path, text: &str) -> Target {
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        Target::load(path).unwrap()
    }

    fn github() -> McpServerConfig {
        McpServerConfig {
            name: "github".into(),
            url: Some("https://api.githubcopilot.com/mcp/".into()),
            headers: [(
                "Authorization".to_string(),
                "Bearer ${GITHUB_TOKEN}".to_string(),
            )]
            .into(),
            enabled_tools: vec!["search_*".into(), "get_issue".into()],
            max_output_chars: Some(8000),
            output_caps: [("get_issue".to_string(), 2000)].into(),
            ..Default::default()
        }
    }

    #[test]
    fn adding_and_removing_a_server_keeps_comments_order_and_other_tables() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), MINE);
        add_server(t.root(), &github(), false).unwrap();
        write_secret_hosts(t.root(), "GITHUB_TOKEN", &["api.githubcopilot.com".into()]).unwrap();
        t.save().unwrap();

        let text = std::fs::read_to_string(&t.path).unwrap();
        // The new entry goes after the last server, everything else as it was.
        let (servers, rest) = MINE.split_at(MINE.find("# comes last").unwrap());
        let new = "[[mcp.servers]]\nname = \"github\"\n";
        assert!(text.starts_with(&format!("{servers}{new}")), "{text}");
        assert!(text.contains(rest), "{text}");
        let cfg = t.config().unwrap();
        let names: Vec<_> = cfg.mcp.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["fs", "old", "github"]);
        assert_eq!(cfg.mcp.servers[2], github(), "it reads back as written");
        assert!(cfg.secrets.contains_key("GITHUB_TOKEN"));

        // A second add of the name is refused; --replace edits it in place.
        assert!(add_server(t.root(), &github(), false).is_err());
        let fs = McpServerConfig {
            name: "fs".into(),
            command: "fs-server".into(),
            enabled_tools: vec!["read".into()],
            sandbox: false,
            ..Default::default()
        };
        add_server(t.root(), &fs, true).unwrap();
        t.save().unwrap();
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(
            text.contains(
                "# the filesystem server\n[[mcp.servers]]\nname = \"fs\"\ncommand = \"fs-server\""
            ),
            "{text}"
        );
        let cfg = t.config().unwrap();
        assert_eq!(cfg.mcp.servers[0], fs);
        assert_eq!(cfg.mcp.servers.len(), 3);

        assert!(remove_server(t.root(), "old").unwrap());
        assert!(!remove_server(t.root(), "old").unwrap());
        t.save().unwrap();
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(!text.contains("old-server"), "{text}");
        assert!(text.starts_with("# my config\ndefault_provider = \"a\"   # keep\n"));
        assert!(
            text.contains("# comes last\n[sandbox]\nmode = \"read-only\"\n"),
            "{text}"
        );
        let order: Vec<_> = [
            "[providers.a]",
            "name = \"fs\"",
            "name = \"github\"",
            "[sandbox]",
        ]
        .iter()
        .map(|s| text.find(s).unwrap_or_else(|| panic!("{s} in {text}")))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    }

    #[test]
    fn a_config_without_servers_gets_them_and_inline_lists_are_edited_inline() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = target(dir.path(), "# empty\n");
        add_server(t.root(), &github(), false).unwrap();
        t.save().unwrap();
        assert_eq!(t.config().unwrap().mcp.servers.len(), 1);
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(
            text.contains("[[mcp.servers]]\nname = \"github\""),
            "{text}"
        );

        let mut t = target(
            dir.path(),
            "mcp = { servers = [{ name = \"a\", command = \"x\" }] }  # inline\n",
        );
        add_server(t.root(), &github(), false).unwrap();
        t.save().unwrap();
        let names: Vec<_> = t
            .config()
            .unwrap()
            .mcp
            .servers
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, ["a", "github"]);
        assert!(remove_server(t.root(), "a").unwrap());
        t.save().unwrap();
        let text = std::fs::read_to_string(&t.path).unwrap();
        assert!(text.contains("# inline"), "{text}");
        assert_eq!(t.config().unwrap().mcp.servers.len(), 1);

        let mut doc: toml_edit::DocumentMut = "[mcp]\nservers = \"x\"\n".parse().unwrap();
        assert!(add_server(doc.as_table_mut(), &github(), false).is_err());
    }

    #[test]
    fn removal_reports_the_secrets_only_that_server_used() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[[mcp.servers]]\nname = \"a\"\nurl = \"https://a.dev/mcp\"\n\
                    headers = { Authorization = \"Bearer ${A_TOKEN}\", X-Org = \"${ORG}-${A_TOKEN}\" }\n\n\
                    [[mcp.servers]]\nname = \"b\"\nurl = \"https://b.dev/mcp\"\nheaders = { X-Org = \"${ORG}\" }\n\n\
                    [secrets]\nA_TOKEN = [\"a.dev\"]\nORG = [\"a.dev\", \"b.dev\"]\n";
        let t = target(dir.path(), text);
        let cfg = t.config().unwrap();
        assert_eq!(secret_refs(&cfg.mcp.servers[0]), ["A_TOKEN", "ORG"]);

        let removed = remove_configured(t.path.clone(), "a", false)
            .unwrap()
            .unwrap();
        assert_eq!(removed.secrets_kept, ["A_TOKEN"]);
        let t = Target::load(t.path).unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.secrets.len(), 2, "[secrets] stay");
        assert!(remove_configured(t.path, "a", false).unwrap().is_none());
    }
}
