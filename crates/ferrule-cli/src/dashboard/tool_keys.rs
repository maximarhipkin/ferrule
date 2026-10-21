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

/// `GET secrets`: each `[secrets]` name with its hosts, whether this
/// page may remove it or edit its hosts, whether any kept key (from any
/// table) may be rotated, and what uses it — never a value.
pub fn list(ctx: &Ctx) -> Answer {
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let place = need!(setup_place(ctx));
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    let mut keys: Vec<Value> = cfg
        .secrets
        .iter()
        .map(|(name, spec)| {
            let page_shaped = stored.iter().any(|(n, _)| n == name) && !named_in(&text, name);
            json!({
                "name": name,
                "hosts": ferrule_proxy::SecretRule::from(spec).hosts,
                "removable": page_shaped,
                "hosts_editable": page_shaped,
                "rotatable": true,
                "used_by": used_by(&text, name),
            })
        })
        .collect();
    // Kept keys the config names outside [secrets] — a provider's
    // api_key_env, a channel token — can't be added or removed here,
    // but their value can be rotated (the name and its hosts don't move).
    for (name, _) in &stored {
        if !cfg.secrets.contains_key(name) {
            keys.push(json!({
                "name": name,
                "hosts": [],
                "removable": false,
                "hosts_editable": false,
                "rotatable": true,
                "used_by": used_by(&text, name),
            }));
        }
    }
    keys.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    ok(json!({ "keys": keys, "here": ctx.live.is_some(), "locked": locked() }))
}

/// `POST secrets/add {name, value, hosts}`.
pub fn add(ctx: &Ctx, body: &Value) -> Answer {
    let name = need!(arg(body, "name"));
    let hosts = match check_new(
        ctx,
        name,
        body.get("hosts").and_then(Value::as_str).unwrap_or(""),
    ) {
        Ok(h) => h,
        Err((status, why)) => return bad(status, why),
    };
    let place = need!(setup_place(ctx));
    let value = body
        .get("value")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if value.is_empty() {
        return bad(400, "paste the key first");
    }
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return bad(
            400,
            "the key has line breaks or is too long; paste it as one line. Nothing was saved.",
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
    // A chat that asked for this key carries on (M54 §6.3).
    crate::self_service::keys::saved(name, &hosts);
    ok(json!({
        "ok": true,
        "name": name,
        "said": format!(
            "Saved {name}. Commands see a stand-in; the real key goes only to {}.",
            hosts.join(", ")
        ),
    }))
}

/// What `secrets/add` checks before the value: keys may be added here,
/// `name` is a new name of the right shape, and `hosts` (comma- or
/// space-separated) are sites. `Ok`: the hosts, tidied. The chat's
/// `secret_request` makes the same checks, so its refusals are the page's
/// words (M54 §6.3).
pub(crate) fn check_new(ctx: &Ctx, name: &str, hosts: &str) -> Result<Vec<String>, (u16, String)> {
    if let Some(why) = locked() {
        return Err((403, why));
    }
    // A page served without the gateway can't see the gateway's
    // environment, so it can't tell whether a name is free there.
    if ctx.live.is_none() {
        return Err((
            409,
            "Open this page from your running bot (send it /dashboard): only the bot can tell whether a name is free.".into(),
        ));
    }
    let (Some(cfg), Ok(place)) = (config(ctx), setup_place(ctx)) else {
        return Err((
            503,
            "the config file isn't available in this process".into(),
        ));
    };
    let malformed = |why: String| -> (u16, String) { (400, why) };
    let hosts = self::hosts(hosts).map_err(malformed)?;
    shape(name).map_err(malformed)?;
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    if cfg.secrets.contains_key(name)
        || stored.iter().any(|(n, _)| n == name)
        || named_in(&text, name)
        || std::env::var_os(name).is_some()
    {
        return Err((
            409,
            format!("{name} is in use already, so this page won't change it. Pick a new name."),
        ));
    }
    Ok(hosts)
}

/// Whether the secrets file keeps `name`: what `rotate` can replace.
pub(crate) fn kept(ctx: &Ctx, name: &str) -> bool {
    setup_place(ctx).is_ok_and(|place| {
        crate::secrets::read(&place.secrets)
            .unwrap_or_default()
            .iter()
            .any(|(n, _)| n == name)
    })
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

/// `POST secrets/rotate {name, value}`: replace a kept key's value
/// (M52). The one change that's safe for a key the config names — a
/// provider's `api_key_env`, a channel token — because the name and its
/// hosts don't move. Write-then-swap: a failed write leaves the old
/// value in place.
pub fn rotate(ctx: &Ctx, body: &Value) -> Answer {
    if let Some(why) = locked() {
        return bad(403, why);
    }
    if ctx.live.is_none() {
        return bad(
            409,
            "Open this page from your running bot (send it /dashboard): only the bot holds the keys it uses.",
        );
    }
    let place = need!(setup_place(ctx));
    let name = need!(arg(body, "name")).trim();
    let value = body
        .get("value")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    if !stored.iter().any(|(n, _)| n == name) {
        return bad(
            404,
            format!("no key named {name} is kept here, so there's nothing to replace."),
        );
    }
    if value.is_empty() {
        return bad(400, "paste the new key first");
    }
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return bad(
            400,
            "the key has line breaks or is too long; paste it as one line. Nothing was saved.",
        );
    }
    need!(confirmed(
        body,
        format!(
            "Replace the key for {name}? Whatever uses it stops working if the new key is wrong — you fix it here the same way. The old key is never shown."
        )
    ));
    if let Err(e) = crate::secrets::set(&place.secrets, name, value) {
        return bad(
            500,
            format!("the key couldn't be replaced: {e:#}. The old one is still in place."),
        );
    }
    // Providers and connections read their key_env per call through the
    // held keys (`secrets::var`, M54.2): put the new value there too, so
    // the change works without a restart.
    ferrule_sandbox::secret_env::set(name, value);
    // M52.2: a tool key the proxy swaps in takes the new value now; the
    // old placeholder keeps working for commands started before.
    // Only a `[secrets]` name: the proxy isn't started for anything else.
    if let Some(cfg) = config(ctx) {
        if let Some(spec) = cfg.secrets.get(name) {
            if let Ok(Some(broker)) = crate::shared_broker(&cfg) {
                if let Err(e) = broker.bind(name, &ferrule_proxy::SecretRule::from(spec), value) {
                    tracing::warn!("{name} rotated, but the proxy kept the old value: {e:#}");
                }
            }
        }
    }
    audit(ctx, "dashboard.secret.rotate", json!({ "name": name }));
    ok(json!({
        "ok": true,
        "said": format!("Replaced {name} — in use from the next call, no restart needed. The old key is gone from this machine."),
    }))
}

/// `POST secrets/hosts {name, hosts}`: where a tool key may be sent
/// (M52). Only a page-shaped entry (the same test as remove); the
/// config page's undo covers the change.
pub fn hosts_edit(ctx: &Ctx, body: &Value) -> Answer {
    if let Some(why) = locked() {
        return bad(403, why);
    }
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let place = need!(setup_place(ctx));
    let name = need!(arg(body, "name"));
    let hosts = match hosts(body.get("hosts").and_then(Value::as_str).unwrap_or("")) {
        Ok(h) => h,
        Err(why) => return bad(400, why),
    };
    let stored = crate::secrets::read(&place.secrets).unwrap_or_default();
    let text = std::fs::read_to_string(&place.config).unwrap_or_default();
    if !cfg.secrets.contains_key(name)
        || !stored.iter().any(|(n, _)| n == name)
        || named_in(&text, name)
    {
        return bad(
            409,
            format!("{name} is used or kept elsewhere; where a provider's key goes is set by the provider, on the Models page."),
        );
    }
    need!(confirmed(
        body,
        format!("Change where {name} may be sent to {}?", hosts.join(", "))
    ));
    let done = (|| -> anyhow::Result<()> {
        let mut t = crate::setup::Target::load(place.config.clone())?;
        crate::setup::write_secret_hosts(t.root(), name, &hosts)?;
        t.save()
    })();
    if let Err(e) = done {
        return bad(500, format!("the sites couldn't be saved: {e:#}"));
    }
    audit(
        ctx,
        "dashboard.secret.hosts",
        json!({ "name": name, "hosts": hosts }),
    );
    ok(json!({
        "ok": true,
        "said": format!("{name} now goes only to {}. The proxy picks this up within a minute.", hosts.join(", ")),
    }))
}

/// What uses a name, in words: the provider brains, the channels and the
/// MCP apps the config names it for. Never the value.
pub(super) fn used_by(config_text: &str, name: &str) -> Vec<String> {
    let Ok(doc) = config_text.parse::<toml::Table>() else {
        return vec![];
    };
    let mut users = Vec::new();
    if let Some(providers) = doc.get("providers").and_then(toml::Value::as_table) {
        for (id, p) in providers {
            if p.get("api_key_env").and_then(toml::Value::as_str) == Some(name) {
                users.push(format!("the {id} brain"));
            }
        }
    }
    if let Some(gw) = doc.get("gateway").and_then(toml::Value::as_table) {
        for (key, channel) in [
            ("telegram_token_env", "Telegram"),
            ("discord_token_env", "Discord"),
            ("slack_token_env", "Slack"),
        ] {
            if gw.get(key).and_then(toml::Value::as_str) == Some(name) {
                users.push(channel.to_string());
            }
        }
    }
    let wanted = format!("${{{name}}}");
    if let Some(mcp) = doc.get("mcp").and_then(toml::Value::as_table) {
        if let Some(servers) = mcp.get("servers").and_then(toml::Value::as_array) {
            for s in servers {
                let headers_name = s
                    .get("headers")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|h| {
                        h.values()
                            .any(|v| v.as_str().is_some_and(|s| s.contains(&wanted)))
                    });
                if headers_name {
                    if let Some(app) = s.get("name").and_then(toml::Value::as_str) {
                        users.push(format!("the {app} app"));
                    }
                }
            }
        }
    }
    if users.is_empty() {
        users.push(
            if doc
                .get("secrets")
                .and_then(toml::Value::as_table)
                .is_some_and(|s| s.contains_key(name))
            {
                "a tool's commands".into()
            } else {
                "nothing in the config yet".into()
            },
        );
    }
    users
}

/// Managed mode: keys for tools are for commands, which `shell` covers.
pub(crate) fn locked() -> Option<String> {
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
pub(super) fn named_in(config_text: &str, name: &str) -> bool {
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
