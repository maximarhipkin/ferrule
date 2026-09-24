//! `[browser]` glue: the MCP server entry the agent gets, and the checks
//! `doctor` and `setup` share. The pieces themselves are in
//! `ferrule_mcp::browser`.

use crate::{config, service};
use anyhow::{anyhow, bail, Result};
use ferrule_mcp::browser::{self as mcp, BrowserProxy, Confine};
use ferrule_mcp::{BrowserConfig, McpServerConfig};
use ferrule_sandbox::Sandbox;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a launch test waits for Chrome to print a page.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The browser's MCP server for `cfg`, or `None` when `[browser]` is off.
/// An error says why it can't run here.
pub fn server(cfg: &config::Config) -> Result<Option<McpServerConfig>> {
    let b = &cfg.browser;
    if !b.enabled {
        return Ok(None);
    }
    if cfg.mcp.servers.iter().any(|s| s.name == mcp::SERVER_NAME) {
        bail!("an [[mcp.servers]] entry is already named `browser`; rename it");
    }
    let chrome = chrome(b)
        .ok_or_else(|| anyhow!("no Chrome or Chromium found; set `chrome` in [browser]"))?;
    let command = mcp::find_agent_browser(&b.command).map_err(|e| anyhow!(e))?;
    if let Some(why) = blocker(b) {
        bail!(
            "{why}. `chrome_sandbox = false` in [browser] accepts running it without \
             Chrome's own sandbox (see docs/browser.md)"
        );
    }
    let proxy = crate::shared_broker(cfg)?.map(|broker| {
        let (addr, username, password) = broker.proxy_auth();
        BrowserProxy {
            addr,
            username: username.into(),
            password: password.into(),
            ca_spki_sha256: broker.ca_spki_sha256().into(),
        }
    });
    let state_dir = crate::mcp_state_dir(mcp::SERVER_NAME)?;
    let mut server = b.server_config(&chrome, &state_dir, proxy.as_ref())?;
    server.command = command.to_string_lossy().into_owned();
    Ok(Some(server))
}

/// The Chrome `[browser]` drives: the configured one, else the first found.
pub fn chrome(b: &BrowserConfig) -> Option<PathBuf> {
    b.chrome.clone().or_else(mcp::find)
}

/// Why Chrome's own sandbox can't be kept here though the config asks for
/// it (agent-browser would turn it off by itself).
pub fn blocker(b: &BrowserConfig) -> Option<&'static str> {
    if b.chrome_sandbox {
        mcp::chrome_sandbox_blocker(service::is_root())
    } else {
        None
    }
}

/// Start `chrome` headless the way the browser server runs it: inside
/// ferrule's sandbox, as a helper with the server's state dir, when there
/// is a sandbox here. `no_sandbox` turns Chrome's own off.
pub fn launch_test(cfg: &config::Config, chrome: &Path, no_sandbox: bool) -> Result<(), String> {
    let sandbox = Sandbox::new(crate::sandbox_policy(cfg)).ok();
    let state_dir = crate::mcp_state_dir(mcp::SERVER_NAME).map_err(|e| e.to_string())?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| state_dir.clone());
    let confine = sandbox
        .as_ref()
        .filter(|s| s.is_active())
        .map(|sandbox| Confine {
            sandbox,
            workspace: &workspace,
            state_dir: &state_dir,
        });
    mcp::launch_test(chrome, no_sandbox, LAUNCH_TIMEOUT, confine.as_ref())
}
