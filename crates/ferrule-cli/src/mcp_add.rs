//! M17: `ferrule mcp add`, `list` and `remove`. An owner-added server is
//! started live, its tools are scanned, its secrets are bound to hosts, and
//! only then is it written to the config, where running gateways pick it up
//! (`config_follow`). Design: `docs/m17-mcp-add.md`.

use crate::mcp_config::{self, add_server, config_file, secret_refs};
use crate::setup::{ask_hosts, ask_secret, info, no_shape, ok, split_hosts, tilde, warn};
use crate::setup::{write_secret_hosts, Target};
use crate::{config, secrets, self_extend};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use ferrule_extensions::scan::report_for_owner;
use ferrule_extensions::{AllowList, ExtensionManager, Layout, ManagerConfig, Probe};
use ferrule_mcp::McpServerConfig;
use ferrule_proxy::Broker;
use ferrule_sandbox::{Egress, Sandbox};
use inquire::{Confirm, Select, Text};
use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Subcommand)]
pub enum McpCmd {
    /// Add an MCP server: start it, list and scan its tools, bind its
    /// secrets, then write it to the config. Running gateways pick it up
    /// within seconds, no restart
    Add(Box<AddArgs>),
    /// Configured and installed MCP servers and their status
    List,
    /// Remove a configured or installed server; running agents stop it
    /// within seconds. `[secrets]` stay
    Remove {
        name: String,
        /// Delete the server's state dir too
        #[arg(long)]
        purge: bool,
    },
    /// Turn a configured server off (`[mcp] disabled`); running agents
    /// stop it within seconds. It stays in the config
    Disable { name: String },
    /// Turn a disabled server back on
    Enable { name: String },
}

#[derive(Args, Default)]
pub struct AddArgs {
    /// Its tools are named `mcp__<name>__<tool>`
    pub name: String,
    /// The server's command line, for a stdio server: `-- npx -y pkg`
    #[arg(last = true)]
    pub command: Vec<String>,
    /// A Streamable HTTP server's endpoint, instead of a command
    #[arg(long)]
    pub url: Option<String>,
    /// KEY=VALUE for a stdio server's environment; no credentials (see --secret)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// A header for a URL server; `${NAME}` sends a secret's placeholder
    #[arg(long = "header", value_name = "NAME=VALUE")]
    pub headers: Vec<String>,
    /// Bind a secret to hosts: the server gets a placeholder, the proxy
    /// swaps the real value in on requests to those hosts. The value comes
    /// from the saved secrets, the environment, or a prompt
    #[arg(long = "secret", value_name = "NAME[=HOST,…]")]
    pub secrets: Vec<String>,
    /// Offer only these tools; a trailing `*` is a prefix
    #[arg(long = "enabled-tool", value_name = "TOOL")]
    pub enabled_tools: Vec<String>,
    /// Cap on one tool result from this server, in chars
    #[arg(long, value_name = "CHARS")]
    pub max_output: Option<usize>,
    /// Cap on one tool's results, in chars
    #[arg(long = "output-cap", value_name = "TOOL=CHARS")]
    pub output_caps: Vec<String>,
    /// Per-call timeout, in seconds
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
    /// Run it outside the sandbox (doctor flags it)
    #[arg(long)]
    pub no_sandbox: bool,
    /// Add it even with tools the scan blocks
    #[arg(long, conflicts_with = "skip_flagged")]
    pub waive: bool,
    /// Leave out the tools the scan blocks
    #[arg(long)]
    pub skip_flagged: bool,
    /// Replace the configured server of that name
    #[arg(long)]
    pub replace: bool,
    /// Ask nothing: what a question would settle is an error instead
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Where the server runs for the check
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,
    /// Skip the closing doctor run (the setup wizard's step)
    #[arg(skip)]
    pub no_doctor: bool,
}

/// `ferrule mcp disable|enable`, through the shared settings operation (M24).
fn mcp_toggle(name: &str, off: bool) -> Result<()> {
    let s = crate::settings_admin::Settings::open(None)?;
    println!("{}", s.mcp_set_disabled(name, off, "cli")?.said);
    Ok(())
}

pub async fn run(op: McpCmd) -> Result<()> {
    match op {
        McpCmd::Add(args) => add(*args).await.map(|_| ()),
        McpCmd::List => self_extend::list(true),
        McpCmd::Remove { name, purge } => {
            let (cfg, _) = config::Config::load()?;
            let installed =
                ferrule_extensions::LockStore::new(Layout::new(config::data_dir()?).lock_path())
                    .load()?
                    .servers
                    .contains_key(&name);
            if !installed && !cfg.mcp.servers.iter().any(|s| s.name == name) {
                bail!("no MCP server `{name}` (`ferrule mcp list` shows them)");
            }
            self_extend::run(self_extend::ExtCmd::Remove { name, purge }).await
        }
        McpCmd::Disable { name } => mcp_toggle(&name, true),
        McpCmd::Enable { name } => mcp_toggle(&name, false),
    }
}

/// A secret the server will use: its hosts (`None`: keep the config's) and
/// its value, and whether the value still has to be saved.
struct NewSecret {
    name: String,
    hosts: Option<Vec<String>>,
    value: String,
    save: bool,
}

fn valid_server_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Headers that carry a credential, so a literal value is a key in the
/// config.
fn credential_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "authorization" | "proxy-authorization" | "cookie"
    ) || n.ends_with("api-key")
        || n.ends_with("apikey")
        || n.contains("token")
        || n.contains("secret")
}

fn pair(what: &str, text: &str) -> Result<(String, String)> {
    let (k, v) = text
        .split_once('=')
        .or_else(|| text.split_once(':'))
        .ok_or_else(|| anyhow!("{what} `{text}`: expected NAME=VALUE"))?;
    let k = k.trim();
    if k.is_empty() {
        bail!("{what} `{text}`: no name");
    }
    Ok((k.to_string(), v.trim().to_string()))
}

fn url_host(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("--url {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("--url {url}: only http(s) servers");
    }
    parsed
        .host_str()
        .map(str::to_lowercase)
        .ok_or_else(|| anyhow!("--url {url}: no host"))
}

/// The saved value of `name`, read from the file (the environment may
/// hold a different one that wins at run time).
fn saved_secret(name: &str) -> Option<String> {
    secrets::read(&secrets::path().ok()?)
        .ok()?
        .into_iter()
        .find(|(n, v)| n == name && !v.is_empty())
        .map(|(_, v)| v)
}

/// Ask for what the command line didn't say. Only with a terminal.
fn ask_missing(a: &mut AddArgs) -> Result<()> {
    if a.command.is_empty() && a.url.is_none() {
        let kinds = vec![
            "A command it runs (stdio), like `npx -y @modelcontextprotocol/server-filesystem .`",
            "A URL (Streamable HTTP), like https://mcp.example.com/mcp",
        ];
        if Select::new("How is the server reached?", kinds)
            .raw_prompt()?
            .index
            == 0
        {
            let line = Text::new("Command line")
                .with_help_message(
                    "split on spaces; for quoting, use `ferrule mcp add NAME -- cmd args…`",
                )
                .prompt()?;
            a.command = line.split_whitespace().map(str::to_string).collect();
            if a.command.is_empty() {
                bail!("no command");
            }
        } else {
            a.url = Some(Text::new("URL").prompt()?.trim().to_string());
        }
        if a.secrets.is_empty()
            && Confirm::new("Does it need a token or API key?")
                .with_default(false)
                .prompt()?
        {
            let name = Text::new("Variable name for it")
                .with_placeholder("GITHUB_TOKEN")
                .prompt()?
                .trim()
                .to_string();
            if a.url.is_some() {
                let header = format!("Authorization=Bearer ${{{name}}}");
                if Confirm::new(&format!("Send it as `Authorization: Bearer ${{{name}}}`?"))
                    .with_default(true)
                    .prompt()?
                {
                    a.headers.push(header);
                }
            }
            a.secrets.push(name);
        }
    }
    Ok(())
}

/// Resolve `--secret NAME[=hosts]` against the config: hosts and values.
fn resolve_secrets(a: &AddArgs, cfg: &config::Config, interactive: bool) -> Result<Vec<NewSecret>> {
    let mut out: Vec<NewSecret> = Vec::new();
    for spec in &a.secrets {
        let (name, hosts) = match spec.split_once('=') {
            Some((n, h)) => (n.trim().to_string(), Some(split_hosts(h))),
            None => (spec.trim().to_string(), None),
        };
        if !secrets::valid_name(&name) {
            bail!("--secret {spec}: `{name}` isn't a variable name");
        }
        if out.iter().any(|s| s.name == name) {
            continue;
        }
        if hosts
            .as_ref()
            .is_some_and(|h| h.is_empty() || h.iter().any(|h| h.contains(['/', ':', ' '])))
        {
            bail!("--secret {spec}: hosts are names like api.example.com, comma-separated");
        }
        let hosts = match hosts {
            Some(h) => Some(h),
            None if cfg.secrets.contains_key(&name) => None,
            None => match &a.url {
                Some(url) => Some(vec![url_host(url)?]),
                None if interactive => {
                    info(format!(
                        "{name} is swapped in only on requests to the hosts you allow."
                    ));
                    Some(ask_hosts(&[])?)
                }
                None => bail!(
                    "--secret {name}: which hosts may it go to? `--secret {name}=api.example.com`"
                ),
            },
        };
        let (value, save) = match saved_secret(&name) {
            Some(v) => (v, false),
            None => match std::env::var(&name).ok().filter(|v| !v.is_empty()) {
                Some(v) => (v, true),
                None if interactive => match ask_secret(&format!("{name} value"), false, no_shape)?
                {
                    Some(v) => (v, true),
                    None => bail!("no value for {name}"),
                },
                None => bail!(
                    "no value for {name}: export it, or run without --yes at a terminal to enter it"
                ),
            },
        };
        out.push(NewSecret {
            name,
            hosts,
            value,
            save,
        });
    }
    Ok(out)
}

/// The server's entry from the flags, refusing credentials in plain text.
fn server_config(a: &AddArgs, secret_names: &[String]) -> Result<McpServerConfig> {
    let mut env = HashMap::new();
    for e in &a.env {
        let (k, v) = pair("--env", e)?;
        if ferrule_sandbox::looks_secret(&k) {
            bail!(
                "--env {k}=…: that looks like a credential, and the config isn't the place for one. \
                 Use `--secret {k}` instead: the server gets a placeholder in ${k}, and the real \
                 value only goes to the hosts you allow"
            );
        }
        env.insert(k, v);
    }
    let mut headers = HashMap::new();
    for h in &a.headers {
        let (k, v) = pair("--header", h)?;
        if credential_header(&k) && !v.contains("${") {
            bail!(
                "--header {k}: a literal credential would sit in the config. Put it in a secret \
                 and refer to it: `--secret NAME --header '{k}=… ${{NAME}}'`"
            );
        }
        headers.insert(k, v);
    }
    let mut output_caps = HashMap::new();
    for c in &a.output_caps {
        let (tool, n) = pair("--output-cap", c)?;
        let n: usize = n
            .parse()
            .map_err(|_| anyhow!("--output-cap {c}: CHARS must be a number"))?;
        output_caps.insert(tool, n);
    }
    let cfg = McpServerConfig {
        name: a.name.clone(),
        command: a.command.first().cloned().unwrap_or_default(),
        args: a.command.iter().skip(1).cloned().collect(),
        env,
        url: a.url.clone(),
        headers,
        timeout_secs: a.timeout,
        sandbox: !a.no_sandbox,
        enabled_tools: a.enabled_tools.clone(),
        max_output_chars: a.max_output,
        output_caps,
        ..Default::default()
    };
    for r in secret_refs(&cfg) {
        if ferrule_sandbox::looks_secret(&r) && !secret_names.contains(&r) {
            bail!("`${{{r}}}` in a header needs to be a secret: add `--secret {r}`");
        }
    }
    Ok(cfg)
}

/// The sandbox the server will run in, with a proxy of its own holding the
/// config's secrets and the new values, so the check sees what the daemon
/// will. The broker lives as long as the check.
fn probe_sandbox(
    cfg: &config::Config,
    new: &[NewSecret],
) -> Result<(Arc<Sandbox>, Option<Broker>)> {
    let mut sandbox = Sandbox::new(crate::sandbox_policy(cfg)).map_err(|e| anyhow!(e))?;
    let values: BTreeMap<&str, &str> = new
        .iter()
        .map(|s| (s.name.as_str(), s.value.as_str()))
        .collect();
    let broker = if cfg.secrets.is_empty() {
        None
    } else {
        Broker::start(crate::broker_config(cfg)?, |name| {
            values
                .get(name)
                .map(|v| v.to_string())
                .or_else(|| std::env::var(name).ok())
        })?
    };
    if let Some(broker) = &broker {
        let ca_cert_pem = std::fs::read_to_string(broker.ca_cert_path())
            .with_context(|| format!("reading {}", broker.ca_cert_path().display()))?;
        sandbox = sandbox
            .with_env(broker.child_env())
            .with_egress(Some(Egress {
                proxy_url: broker.proxy_url(),
                ca_cert_pem,
            }));
    }
    Ok((Arc::new(sandbox), broker))
}

fn show_tools(probe: &Probe) {
    for t in &probe.tools {
        let line = t.description.lines().next().unwrap_or("");
        let short: String = line.chars().take(80).collect();
        let flag = if probe.blocked.contains(&t.name) {
            "  [BLOCKED]"
        } else {
            ""
        };
        println!("    {}{flag}  {short}", t.name);
    }
    if let Some(why) = &probe.sandbox_degraded {
        warn(format!("not fully sandboxed here: {why}"));
    }
    if probe.findings.is_empty() {
        ok("scan: clean");
    } else {
        println!("  scan:\n{}", report_for_owner(&probe.findings));
    }
}

/// `ferrule mcp add`. Returns the names of the tools it added.
pub async fn add(a: AddArgs) -> Result<Vec<String>> {
    add_to(config_file()?, a).await
}

/// `add` on the config at `path`.
pub async fn add_to(path: PathBuf, mut a: AddArgs) -> Result<Vec<String>> {
    let interactive = !a.yes && std::io::stdin().is_terminal();
    let mut t = Target::load(path)?;
    let cfg0 = t.config()?;

    // 1. What and where.
    if !valid_server_name(&a.name) {
        bail!(
            "`{}`: a server name is 1-64 letters, digits, - and _",
            a.name
        );
    }
    if a.name == "browser" {
        bail!("`browser` is the built-in browser's name ([browser] in the config)");
    }
    let layout = Layout::new(config::data_dir()?);
    if ferrule_extensions::LockStore::new(layout.lock_path())
        .load()?
        .servers
        .contains_key(&a.name)
    {
        bail!(
            "`{}` is a server the agent installed; `ferrule mcp remove {}` first to configure it yourself",
            a.name,
            a.name
        );
    }
    if cfg0.mcp.servers.iter().any(|s| s.name == a.name) && !a.replace {
        if !interactive
            || !Confirm::new(&format!("`{}` is configured already. Replace it?", a.name))
                .with_default(false)
                .prompt()?
        {
            bail!("`{}` is configured already (--replace replaces it)", a.name);
        }
        a.replace = true;
    }
    if interactive {
        ask_missing(&mut a)?;
    }
    match (a.command.is_empty(), &a.url) {
        (true, None) => bail!(
            "what is the server? `ferrule mcp add {} -- <command> <args>…` or `--url https://…`",
            a.name
        ),
        (false, Some(_)) => bail!("a command or --url, not both"),
        (_, Some(url)) => {
            url_host(url)?;
            if !a.env.is_empty() {
                bail!("--env is for a stdio server; a URL server gets --header");
            }
        }
        (false, None) => {
            if !a.headers.is_empty() {
                bail!("--header is for a URL server; a stdio server gets --env");
            }
        }
    }

    // 2-3. The entry and its secrets.
    let new = resolve_secrets(&a, &cfg0, interactive)?;
    let mut names: Vec<String> = cfg0.secrets.keys().cloned().collect();
    names.extend(new.iter().map(|s| s.name.clone()));
    let mut server = server_config(&a, &names)?;
    if server.url.is_none() && !a.command.is_empty() && a.no_sandbox {
        warn("--no-sandbox: it can write anywhere you can and read the saved keys");
    }
    add_server(t.root(), &server, a.replace)?;
    for s in &new {
        if let Some(hosts) = &s.hosts {
            write_secret_hosts(t.root(), &s.name, hosts)?;
        }
    }
    let cfg = t
        .config()
        .context("the new entry wouldn't parse; nothing was written")?;
    let entry = cfg
        .mcp
        .servers
        .iter()
        .find(|s| s.name == a.name)
        .cloned()
        .expect("just added");

    // 4-5. Start it as the daemon would, list and scan.
    let workspace = dunce::canonicalize(&a.workspace)
        .map_err(|e| anyhow!("workspace {}: {e}", a.workspace.display()))?;
    let state_dir = layout.mcp_state(&a.name);
    let fresh_dir = !state_dir.exists();
    let (sandbox, broker) = probe_sandbox(&cfg, &new)?;
    let manager = ExtensionManager::new(ManagerConfig {
        allow: AllowList::new::<String>(&[]),
        layout,
        sandbox,
        workspace,
        skills: None,
    });
    println!("Starting `{}` and listing its tools…", a.name);
    let probed = manager.probe(entry.clone()).await;
    let probe = match probed {
        Ok(p) if p.tools.is_empty() && !entry.enabled_tools.is_empty() => {
            let all = manager
                .probe(McpServerConfig {
                    enabled_tools: vec![],
                    ..entry.clone()
                })
                .await
                .map(|p| p.tools.into_iter().map(|t| t.name).collect::<Vec<_>>())
                .unwrap_or_default();
            Err(anyhow!(
                "none of the server's tools match --enabled-tool {} (it offers: {})",
                entry.enabled_tools.join(", "),
                all.join(", ")
            ))
        }
        Ok(p) => Ok(p),
        Err(e) => Err(anyhow!("the server didn't start or list its tools: {e}")),
    };
    drop(broker);
    let probe = match probe {
        Ok(p) => p,
        Err(e) => {
            if fresh_dir {
                let _ = std::fs::remove_dir_all(&state_dir);
            }
            return Err(e.context("nothing was written"));
        }
    };
    println!("`{}` offers {} tool(s):", a.name, probe.tools.len());
    show_tools(&probe);
    if probe.tools.is_empty() {
        warn("it offers no tools yet; ones it adds later are scanned then");
    }

    // A block hit: waive, skip, or stop.
    let mut tools: Vec<String> = probe.tools.iter().map(|t| t.name.clone()).collect();
    if !probe.blocked.is_empty() {
        let choice = if a.waive {
            "waive".to_string()
        } else if a.skip_flagged {
            "skip".to_string()
        } else if interactive {
            Text::new("The scan BLOCKED the text above. Type `waive` to add them anyway, `skip` to leave them out, anything else stops:")
                .prompt()?
                .trim()
                .to_string()
        } else {
            String::new()
        };
        match choice.as_str() {
            "waive" => warn(format!("waived: {}", probe.blocked.join(", "))),
            "skip" => {
                tools.retain(|t| !probe.blocked.contains(t));
                if tools.is_empty() {
                    bail!("every tool is blocked; nothing was written");
                }
                server.enabled_tools = tools.clone();
                add_server(t.root(), &server, true)?;
                info(format!("leaving out {}", probe.blocked.join(", ")));
            }
            _ => bail!(
                "the scan blocked {}; nothing was written (--waive adds them anyway, --skip-flagged leaves them out)",
                probe.blocked.join(", ")
            ),
        }
    }

    // 6. Confirm.
    if interactive
        && !Confirm::new(&format!("Add `{}`?", a.name))
            .with_default(true)
            .prompt()?
    {
        bail!("nothing was written");
    }

    // 7. Write: the config checked and staged, the secrets saved, then the
    // config moved in; if that fails, the secrets go back.
    let file = secrets::path()?;
    let mut undo: Vec<(String, Option<String>)> = Vec::new();
    let saved = t.save_then(|| {
        for s in new.iter().filter(|s| s.save) {
            let before = saved_secret(&s.name);
            secrets::set(&file, &s.name, &s.value)?;
            undo.push((s.name.clone(), before));
        }
        Ok(())
    });
    if let Err(e) = saved {
        for (name, before) in undo {
            let _ = match before {
                Some(v) => secrets::set(&file, &name, &v),
                None => secrets::remove(&file, &name),
            };
        }
        return Err(e);
    }
    let prefixed: Vec<String> = tools
        .iter()
        .map(|t| format!("mcp__{}__{t}", a.name))
        .collect();
    ok(format!("added `{}` to {}", a.name, tilde(&t.path)));
    for s in &new {
        let hosts = cfg
            .secrets
            .get(&s.name)
            .map(|spec| ferrule_proxy::SecretRule::from(spec).hosts.join(", "))
            .unwrap_or_default();
        ok(format!(
            "{} → {hosts}{}",
            s.name,
            if s.save { " (saved)" } else { "" }
        ));
    }
    if crate::self_extend::allow_trusted(&t.path) {
        info("Running gateways and chats pick it up within a few seconds, no restart.");
    } else {
        info(format!(
            "{} is a working-directory config: running agents pick it up at their next start.",
            tilde(&t.path)
        ));
    }

    // 8. The same checks `ferrule doctor` runs; a failing one is reported,
    // the server stays added.
    if !a.no_doctor {
        println!();
        if let Err(e) = crate::doctor::run(true, false).await {
            warn(format!("doctor: {e:#}"));
        }
    }
    Ok(prefixed)
}

/// The setup wizard's MCP step: the configured servers, add one, remove one.
pub async fn setup_step(t: &mut Target, guided: bool) -> Result<()> {
    if guided {
        info("MCP servers give the agent more tools. Each is started, its tools scanned, and");
        info("its keys bound to the hosts you allow before it's added.");
        if !Confirm::new("Add an MCP server now?")
            .with_default(false)
            .prompt()?
        {
            return Ok(());
        }
        return setup_add(t).await;
    }
    loop {
        let cfg = t.config()?;
        let names: Vec<String> = cfg.mcp.servers.iter().map(|s| s.name.clone()).collect();
        let mut labels: Vec<String> = cfg
            .mcp
            .servers
            .iter()
            .map(|s| {
                let what = s.url.clone().unwrap_or_else(|| s.command.clone());
                format!("{} · {what}", s.name)
            })
            .collect();
        labels.push("Add a server".into());
        labels.push("Done".into());
        let pick = Select::new("MCP servers", labels).raw_prompt()?.index;
        match names.get(pick) {
            Some(name) => {
                if Confirm::new(&format!("Remove `{name}`?"))
                    .with_default(false)
                    .prompt()?
                {
                    let removed = mcp_config::remove_configured(t.path.clone(), name, false)?;
                    t.reload()?;
                    ok(format!("removed `{name}`"));
                    for s in removed.map(|r| r.secrets_kept).unwrap_or_default() {
                        info(format!("{s} stays in Tool credentials"));
                    }
                }
            }
            None if pick == names.len() => setup_add(t).await?,
            None => return Ok(()),
        }
    }
}

async fn setup_add(t: &mut Target) -> Result<()> {
    let name = Text::new("A name for it")
        .with_help_message("its tools become mcp__<name>__<tool>")
        .with_validator(
            |v: &str| -> Result<inquire::validator::Validation, inquire::CustomUserError> {
                Ok(if valid_server_name(v.trim()) {
                    inquire::validator::Validation::Valid
                } else {
                    inquire::validator::Validation::Invalid("letters, digits, - and _".into())
                })
            },
        )
        .prompt()?;
    // `add` writes the file itself; the setup's copy is reloaded after.
    let result = add_to(
        t.path.clone(),
        AddArgs {
            name: name.trim().to_string(),
            workspace: PathBuf::from("."),
            no_doctor: true,
            ..Default::default()
        },
    )
    .await;
    t.reload()?;
    result.map(|_| ())
}
