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

pub async fn cmd(op: Option<DashCmd>) -> Result<()> {
    let (cfg, _) = Config::load()?;
    let links = super::auth::Links::at(super::auth::Links::default_path()?);
    match op {
        Some(DashCmd::Off) => {
            links.revoke()?;
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
                println!("http://127.0.0.1:{port}/login#{token}");
                println!(
                    "One login, valid for {} min.",
                    cfg.dashboard.link_minutes.max(1)
                );
                return Ok(());
            }
            let Some(bin) = super::cloudflared(&cfg) else {
                bail!("cloudflared isn't installed (or [connections] cloudflared is \"off\")");
            };
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
                println!("http://127.0.0.1:{port}/login#{token}");
                println!("(the running gateway's dashboard; `ferrule dashboard link --remote` for another device)");
                return Ok(());
            }
            standalone(cfg, links).await
        }
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
