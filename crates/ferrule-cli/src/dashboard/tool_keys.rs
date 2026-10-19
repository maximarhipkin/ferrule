//! M50 (docs/m50-connect.md, C): keys for tools from the page. A new
//! `[secrets]` entry: a name, its value and the hosts it may be sent to.
//! Commands get a same-shaped placeholder; the credential proxy swaps the
//! real value in only on requests to those hosts.
//!
//! Only new names, so nothing that exists moves: not a name in the secrets
//! file or in `[secrets]`, one the config names anywhere (a `*_env` key, a
//! `${NAME}` in a header), one of ferrule's own, or one set in this
//! process's environment, which beats the secrets file (a name already set
//! there would send that value to the new hosts). Write-only: the value
//! goes to the secrets file, and only names and hosts come back.

use super::api::{arg, bad, config, confirmed, missing, need, ok, setup_place, Answer};
use super::Ctx;
use serde_json::{json, Value};

/// `GET secrets`: each `[secrets]` name with its hosts, and whether this
/// page may remove it.
pub fn list(ctx: &Ctx) -> Answer {
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let place = need!(setup_place(ctx));
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    let keys: Vec<Value> = cfg
        .secrets
        .iter()
        .map(|(name, spec)| {
            json!({
                "name": name,
                "hosts": ferrule_proxy::SecretRule::from(spec).hosts,
                "removable": stored.iter().any(|(n, _)| n == name) && !named_in(&text, name),
            })
        })
        .collect();
    ok(json!({ "keys": keys, "here": ctx.live.is_some(), "locked": locked() }))
}

/// `POST secrets/add {name, value, hosts}`.
pub fn add(ctx: &Ctx, body: &Value) -> Answer {
    if let Some(why) = locked() {
        return bad(403, why);
    }
    // A page served without the gateway can't see the gateway's
    // environment, so it can't tell whether a name is free there.
    if ctx.live.is_none() {
        return bad(
            409,
            "Open this page from your running bot (send it /dashboard): only the bot can tell whether a name is free.",
        );
    }
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let place = need!(setup_place(ctx));
    let name = need!(arg(body, "name")).trim();
    let value = body
        .get("value")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let hosts = match hosts(body.get("hosts").and_then(Value::as_str).unwrap_or("")) {
        Ok(h) => h,
        Err(why) => return bad(400, why),
    };
    if let Err(why) = shape(name) {
        return bad(400, why);
    }
    if value.is_empty() {
        return bad(400, "paste the key first");
    }
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return bad(
            400,
            "the key has line breaks or is too long; paste it as one line. Nothing was saved.",
        );
    }
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    if cfg.secrets.contains_key(name)
        || stored.iter().any(|(n, _)| n == name)
        || named_in(&text, name)
        || std::env::var_os(name).is_some()
    {
        return bad(
            409,
            format!("{name} is in use already, so this page won't change it. Pick a new name."),
        );
    }
    let written = (|| -> anyhow::Result<()> {
        crate::secrets::set(&place.secrets, name, value)?;
        let mut t = crate::setup::Target::load(place.config.clone())?;
        crate::setup::write_secret_hosts(t.root(), name, &hosts)?;
        t.save()
    })();
    if let Err(e) = written {
        return bad(500, format!("the key couldn't be saved: {e:#}"));
    }
    audit(
        ctx,
        "dashboard.secret.add",
        json!({ "name": name, "hosts": hosts }),
    );
    ok(json!({
        "ok": true,
        "name": name,
        "said": format!(
            "Saved {name}. Commands see a stand-in; the real key goes only to {}.",
            hosts.join(", ")
        ),
    }))
}

/// `POST secrets/remove {name}`: only a page-shaped entry (its value in the
/// secrets file, named nowhere else in the config).
pub fn remove(ctx: &Ctx, body: &Value) -> Answer {
    if let Some(why) = locked() {
        return bad(403, why);
    }
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let place = need!(setup_place(ctx));
    let name = need!(arg(body, "name"));
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    if !cfg.secrets.contains_key(name)
        || !stored.iter().any(|(n, _)| n == name)
        || named_in(&text, name)
    {
        return bad(
            409,
            format!("{name} is used or kept elsewhere, so it's changed with `ferrule setup` on the machine."),
        );
    }
    need!(confirmed(
        body,
        format!("Remove {name}? Commands that use it stop getting it.")
    ));
    let done = (|| -> anyhow::Result<()> {
        let mut t = crate::setup::Target::load(place.config.clone())?;
        crate::setup::table(t.root(), &["secrets"])?.remove(name);
        t.save()?;
        crate::secrets::remove(&place.secrets, name)
    })();
    if let Err(e) = done {
        return bad(500, format!("{name} couldn't be removed: {e:#}"));
    }
    audit(ctx, "dashboard.secret.remove", json!({ "name": name }));
    ok(json!({ "ok": true, "said": format!("Removed {name}.") }))
}

/// Managed mode: keys for tools are for commands, which `shell` covers.
fn locked() -> Option<String> {
    crate::managed::policy()
        .filter(|p| !p.shell)
        .map(|p| format!("Keys for tools are locked on this bot: {}.", p.why()))
}

fn shape(name: &str) -> Result<(), String> {
    let ok = (2..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(
            "a name is capital letters, digits and _, starting with a letter: WEATHER_API_KEY"
                .into(),
        );
    }
    if name.starts_with("FERRULE_") {
        return Err("names starting with FERRULE_ are ferrule's own".into());
    }
    // Every command gets the name with a stand-in value: never one that
    // the loader, a shell or a runtime acts on by itself.
    let acts = [
        "LD_", "DYLD_", "GIT_", "SSH_", "NODE_", "PYTHON", "BASH_", "NPM_",
    ]
    .iter()
    .any(|p| name.starts_with(p))
        || matches!(
            name,
            "PATH"
                | "HOME"
                | "SHELL"
                | "IFS"
                | "ENV"
                | "PERL5OPT"
                | "RUBYOPT"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "TMPDIR"
                | "LANG"
        );
    if acts {
        return Err(format!(
            "{name} changes how programs run, so it can't hold a key"
        ));
    }
    Ok(())
}

/// The hosts, comma- or space-separated: plain DNS names, or `*.` and one.
/// Never everything, an address, or this machine.
fn hosts(text: &str) -> Result<Vec<String>, String> {
    let list: Vec<String> = text
        .split([',', ' ', '\n'])
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    if list.is_empty() {
        return Err("name the site it may be sent to: api.example.com".into());
    }
    if list.len() > 10 {
        return Err("ten sites at most".into());
    }
    for h in &list {
        let bare = h.strip_prefix("*.").unwrap_or(h);
        let labels: Vec<&str> = bare.split('.').collect();
        let fine = labels.len() >= 2
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && !l.starts_with('-')
                    && !l.ends_with('-')
            })
            // An all-digit name is an address, not a site.
            && !labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit()))
            && !matches!(bare, "localhost" | "local") && !bare.ends_with(".localhost");
        if !fine {
            return Err(format!(
                "{h} isn't a site's name: write it like api.example.com or *.example.com"
            ));
        }
    }
    Ok(list)
}

/// Whether the config names `name` anywhere outside `[secrets]`: as a
/// `*_env` key's value, in a `secrets` list, or as `${NAME}` in a string.
fn named_in(config_text: &str, name: &str) -> bool {
    let Ok(mut doc) = config_text.parse::<toml::Table>() else {
        // A config that doesn't read: assume the worst.
        return true;
    };
    doc.remove("secrets");
    let wanted = format!("${{{name}}}");
    fn walk(v: &toml::Value, key: &str, name: &str, wanted: &str) -> bool {
        match v {
            toml::Value::String(s) => {
                s.contains(wanted)
                    || (key == "secrets" && s == name)
                    // `client_env = "X"` names X_ID and X_SECRET too.
                    || (key.ends_with("_env")
                        && (s == name || name.strip_prefix(s.as_str()).is_some_and(|r| r.starts_with('_'))))
            }
            toml::Value::Array(a) => a.iter().any(|x| walk(x, key, name, wanted)),
            toml::Value::Table(t) => t.iter().any(|(k, x)| walk(x, k, name, wanted)),
            _ => false,
        }
    }
    doc.iter().any(|(k, v)| walk(v, k, name, &wanted))
}

fn audit(ctx: &Ctx, event: &str, detail: Value) {
    if let Some(hub) = &ctx.hub {
        hub.audit()
            .record(chrono::Utc::now(), event, None, None, detail);
    }
}
