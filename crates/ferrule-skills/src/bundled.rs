//! M63.1: ferrule's own skills, compiled into the binary (`bundled/` in
//! this crate). [`install`] writes them under `<data>/bundled-skills`, where
//! discovery finds them as [`Scope::Bundled`](crate::Scope::Bundled) after
//! every project, user and installed skill, so a skill of the same name
//! anywhere else wins. They are on by default; `[skills] disabled` turns
//! one off and `[skills] bundled = false` all of them.
//!
//! They make no network call of their own: each tells the model which
//! request to make with the sandboxed shell, through the proxy and its
//! egress rules. Only `home-assistant` uses a key, one the owner saves
//! through `secret_request` (docs/skills.md, "Bundled skills"). M65's
//! `video-editing` makes no request at all: it is ffmpeg recipes for the
//! same shell.

use std::io;
use std::path::{Component, Path};

/// Under the data dir.
pub const DIR: &str = "bundled-skills";

/// Every file of the pack, by its path under [`DIR`].
const FILES: &[(&str, &str)] = &[
    (
        "exchange-rates/SKILL.md",
        include_str!("../bundled/exchange-rates/SKILL.md"),
    ),
    (
        "hacker-news/SKILL.md",
        include_str!("../bundled/hacker-news/SKILL.md"),
    ),
    (
        "home-assistant/SKILL.md",
        include_str!("../bundled/home-assistant/SKILL.md"),
    ),
    ("maps/SKILL.md", include_str!("../bundled/maps/SKILL.md")),
    ("news/SKILL.md", include_str!("../bundled/news/SKILL.md")),
    (
        "weather/SKILL.md",
        include_str!("../bundled/weather/SKILL.md"),
    ),
    (
        "weather/references/weather-codes.md",
        include_str!("../bundled/weather/references/weather-codes.md"),
    ),
    // M65 §2: ffmpeg recipes for the sandboxed shell.
    (
        "video-editing/SKILL.md",
        include_str!("../bundled/video-editing/SKILL.md"),
    ),
];

/// Writes the pack under `dir` as this binary has it: a file whose bytes
/// differ is replaced (written beside it, then renamed, so a process
/// reading it never sees half), and what the pack no longer has is
/// removed. `dir` is ferrule's own; an owner's edit there doesn't survive
/// the next start (a skill of the same name in `~/.config/ferrule/skills`
/// does).
pub fn install(dir: &Path) -> io::Result<()> {
    for (rel, text) in FILES {
        let path = dir.join(rel);
        if std::fs::read(&path).is_ok_and(|now| now == text.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    prune(dir, dir)
}

/// Removes what isn't in the pack: files, then directories left empty.
/// Symlinks are removed, never followed.
fn prune(root: &Path, dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            prune(root, &path)?;
            if std::fs::read_dir(&path)?.next().is_none() {
                std::fs::remove_dir(&path)?;
            }
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .ok()
            .map(|r| {
                r.components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => s.to_str(),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default();
        // Another process's write in flight.
        if rel.contains(".tmp-") {
            continue;
        }
        if !FILES.iter().any(|(p, _)| *p == rel) {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}
