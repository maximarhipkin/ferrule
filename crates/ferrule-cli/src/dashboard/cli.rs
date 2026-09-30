//! `ferrule dashboard`: a login link to the running gateway's page, or the
//! page served from this process when no gateway runs; for when Telegram
//! itself can't be reached.

use super::{door, Ctx, Dashboard};
use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub enum DashCmd {
    /// Print a one-time login link to the running gateway's dashboard
    Link {
        /// Open a cloudflared quick tunnel from here, for another device;
        /// it stays open until Ctrl-C
        #[arg(long)]
        remote: bool,
    },
    /// Revoke every login link and session, in every process
    #[command(alias = "revoke")]
    Off,
}

/// `<data>/gateway/dashboard.json`: where the gateway's page listens.
#[derive(Debug, Serialize, Deserialize)]
pub struct Marker {
    pub pid: u32,
    pub port: u16,
}

pub fn marker_path() -> Result<PathBuf> {
    Ok(crate::health::dir()?.join("dashboard.json"))
}

pub fn write_marker(port: u16) {
    let write = || -> Result<()> {
        let path = marker_path()?;
        let _lock = crate::filewrite::Lock::take(&path)?;
        crate::filewrite::write(
            &path,
            &serde_json::to_vec(&Marker {
                pid: std::process::id(),
                port,
            })?,
        )
    };
    if let Err(e) = write() {
        tracing::warn!("dashboard marker: {e:#}");
    }
}

pub fn remove_marker() {
    if let Ok(p) = marker_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// The running gateway's dashboard port.
pub fn gateway_port() -> Option<u16> {
    let text = std::fs::read(marker_path().ok()?).ok()?;
    let m: Marker = serde_json::from_slice(&text).ok()?;
    (crate::health::pid_alive(m.pid) != Some(false)).then_some(m.port)
}

/// `ferrule health`: asks the gateway on this machine for /healthz over a
/// plain TCP connection (no proxy, no HTTP client), and prints what it
/// says. `false` when it is `failing` or nothing answers.
pub fn health(probe: bool) -> Result<bool> {
    let port = match gateway_port()
        .or_else(|| {
            std::env::var(crate::managed::DASHBOARD_PORT_ENV)
                .ok()
                .and_then(|v| v.trim().parse::<u16>().ok())
                .filter(|p| *p != 0)
        })
        .or_else(|| {
            Config::load()
                .ok()
                .map(|(c, _)| c.dashboard.port)
                .filter(|p| *p != 0)
        }) {
        Some(p) => p,
        None => bail!("no gateway is running here (no dashboard port found)"),
    };
    let body = match healthz_body(port) {
        Ok(b) => b,
        Err(e) => {
            println!("failing: nothing answers on 127.0.0.1:{port}: {e}");
            return Ok(false);
        }
    };
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("127.0.0.1:{port}/healthz didn't answer JSON"))?;
    let status = v["status"].as_str().unwrap_or("failing");
    let reasons: Vec<&str> = v["reasons"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
        .unwrap_or_default();
    if reasons.is_empty() {
        println!("{status}");
    } else {
        println!("{status}: {}", reasons.join("; "));
    }
    if !probe {
        println!(
            "version {}, up {}s, {} turn(s) running, {} queued",
            v["version"].as_str().unwrap_or("?"),
            v["uptime_secs"].as_u64().unwrap_or(0),
            v["turns"].as_u64().unwrap_or(0),
            v["queued"].as_u64().unwrap_or(0),
        );
    }
    Ok(status != "failing")
}

/// The body of `GET /healthz`, whatever its status.
fn healthz_body(port: u16) -> std::io::Result<String> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    let limit = Duration::from_secs(3);
    let mut s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), limit)?;
    s.set_read_timeout(Some(limit))?;
    s.set_write_timeout(Some(limit))?;
    write!(
        s,
        "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    text.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or_else(|| std::io::Error::other("no HTTP answer"))
}

pub async fn cmd(op: Option<DashCmd>) -> Result<()> {
    let (cfg, _) = Config::load()?;
    let links = super::auth::Links::at(super::auth::Links::default_path()?);
    match op {
        Some(DashCmd::Off) => {
            links.revoke()?;
            // The file too, so no later start loads a session (the
            // revocation time alone already refuses them).
            super::auth::Sessions::beside(
                &links,
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .clear()?;
            println!("Every dashboard link and session is revoked.");
            if gateway_port().is_some() {
                println!("The gateway closes its tunnel once it's idle; `/dashboard off` in Telegram closes it now.");
            }
            Ok(())
        }
        Some(DashCmd::Link { remote }) => {
            let Some(port) = gateway_port() else {
                bail!("no ferrule gateway is running here; `ferrule dashboard` serves the page on its own");
            };
            if !cfg.dashboard.enabled {
                bail!("[dashboard] enabled = false");
            }
            let minutes = std::time::Duration::from_secs(cfg.dashboard.link_minutes.max(1) * 60);
            if !remote {
                let token = links.mint(None, minutes)?;
                println!("{}", super::link_url(&cfg.dashboard, port, &token));
                println!(
                    "One login, valid for {} min.",
                    cfg.dashboard.link_minutes.max(1)
                );
                no_public_address(&cfg);
                return Ok(());
            }
            let cloudflared = super::cloudflared(&cfg);
            if cloudflared.needs_fetch() {
                println!("Fetching cloudflared (Cloudflare's tunnel program) the first time…");
            }
            let bin = cloudflared.path().await?;
            let tunnel = ferrule_connections::tunnel::open(&bin, port).await?;
            let host = super::host_of(&tunnel.url)?;
            let token = links.mint(Some(&host), minutes)?;
            println!("https://{host}/login#{token}");
            println!(
                "One login, valid for {} min. The tunnel stays open until Ctrl-C.",
                cfg.dashboard.link_minutes.max(1)
            );
            let _ = tokio::signal::ctrl_c().await;
            drop(tunnel);
            Ok(())
        }
        None => {
            if let Some(port) = gateway_port() {
                let token = links.mint(
                    None,
                    std::time::Duration::from_secs(cfg.dashboard.link_minutes.max(1) * 60),
                )?;
                println!("{}", super::link_url(&cfg.dashboard, port, &token));
                no_public_address(&cfg);
                println!("(the running gateway's dashboard; `ferrule dashboard link --remote` for another device)");
                return Ok(());
            }
            standalone(cfg, links).await
        }
    }
}

/// Managed mode with no public address: the link only works inside the
/// container, so say where it is opened from.
fn no_public_address(cfg: &Config) {
    if crate::managed::on() && cfg.dashboard.public_url.is_none() {
        println!(
            "This bot has no public address yet (FERRULE_PUBLIC_URL); open it from the panel."
        );
    }
}

/// No gateway: serve the page from here until Ctrl-C.
async fn standalone(cfg: Config, links: super::auth::Links) -> Result<()> {
    let dash = Dashboard::new(cfg.dashboard.clone(), links, Ctx::from_config(&cfg));
    dash.bind(cfg.dashboard.port)
        .await
        .context("starting the dashboard")?;
    let link = dash.local_link()?;
    println!("No gateway is running; serving the dashboard from here until Ctrl-C.");
    println!("{}", door::link_text(&dash, &link, false));
    let _ = tokio::signal::ctrl_c().await;
    Ok(())
}
