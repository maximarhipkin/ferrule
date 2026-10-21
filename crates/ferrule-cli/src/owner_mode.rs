//! M54 owner mode, the host's part (docs/m54-autonomous.md §1–§4): the
//! paths commands can read but not write, which sessions get owner mode
//! and which keep `workspace-write`, the binary's move into the data dir,
//! the fallback when owner mode can't hold here, and this process's own
//! spawns. The sandbox itself is `ferrule-sandbox`'s.

use crate::config::{self, Config};
use crate::service;
use crate::setup::tilde;
use ferrule_gateway::{TaskStore, SCHEDULER_PSEUDO_CHANNEL};
use ferrule_sandbox::{host, Mode, Policy, Sandbox};
use ferrule_trust::{ChatRef, Hub};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Why owner mode is off in this process although the config asks for it.
static FELL_BACK: OnceLock<String> = OnceLock::new();

/// Set when this process is the gateway, where a run no chat traces to is
/// someone else's (see [`owner_tree`]).
static GATEWAY: OnceLock<()> = OnceLock::new();

/// The chat each plan, goal and graph run in this process was started
/// from (its session id), by the run's tree.
// ponytail: never pruned; one entry per `/plan`, `/goal` or `/graph`.
static ORIGINS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Whether this process runs in owner mode: the config asks for it and it
/// didn't fall back. Cards, gates and guest narrowing follow this.
pub fn on(cfg: &Config) -> bool {
    cfg.sandbox.mode == Mode::Owner && FELL_BACK.get().is_none()
}

/// Whether the config asks for owner mode, held or fallen back (the flag
/// is only ever set then, and it covers the gateway's config, which says
/// `workspace-write` after a fallback). What runs outside the sandbox
/// follows this: whatever the bot wrote while owner mode held outlives it.
pub fn wanted(cfg: &Config) -> bool {
    cfg.sandbox.mode == Mode::Owner || FELL_BACK.get().is_some()
}

/// Marks this process as the gateway.
pub fn in_gateway() {
    let _ = GATEWAY.set(());
}

/// Records that the run `tree` was started from `channel`'s chat `chat`,
/// so it gets owner mode only if that chat would.
pub fn started_from(tree: &str, channel: &str, chat: &str) {
    let origin = ferrule_gateway::session::session_id(channel, chat);
    ORIGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tree.to_string(), origin);
}

/// Why owner mode fell back in this process, if it did.
pub fn fell_back() -> Option<&'static str> {
    FELL_BACK.get().map(String::as_str)
}

/// Turns owner mode off for the rest of this process, and says so once;
/// the first reason stays.
pub fn fall_back(why: impl Into<String>) {
    let why = why.into();
    if FELL_BACK.set(why.clone()).is_ok() {
        eprintln!("ferrule: owner mode is off this run: {why}");
    }
}

/// `policy` as this process runs it: owner mode that fell back is
/// `workspace-write` with `require` kept on, so it never degrades to
/// running commands unsandboxed.
pub fn settle(policy: &mut Policy) {
    if policy.mode == Mode::Owner && FELL_BACK.get().is_some() {
        policy.mode = Mode::WorkspaceWrite;
        policy.require = true;
    }
}

/// What commands can read but not write in owner mode (§2): the data dir,
/// the config file's dir, and the dirs systemd starts units from or takes
/// their environment from. Each missing dir is created first, empty, so
/// its protection never depends on a parent being carved.
pub fn protected_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = config::data_dir().into_iter().collect();
    // The file `Config::load` reads, so a `./ferrule.toml` counts too.
    let config_file = match config::config_path() {
        Ok(Some(path)) => std::path::absolute(path).ok(),
        _ => config::global_config_path().ok(),
    };
    paths.extend(config_file.and_then(|f| f.parent().map(Path::to_path_buf)));
    // Owner mode is Linux only: elsewhere these would only be litter.
    if let Some(home) = dirs::home_dir().filter(|_| cfg!(target_os = "linux")) {
        paths.extend(
            [
                ".config/systemd",
                ".local/share/systemd",
                ".config/environment.d",
            ]
            .map(|rel| home.join(rel)),
        );
    }
    for path in &paths {
        let _ = std::fs::create_dir_all(path);
    }
    paths
}

/// M54.6: one way the box is open, and how to close it.
pub struct Opening {
    /// Stable, for the dashboard's notices.
    pub id: &'static str,
    pub what: String,
    pub close: &'static str,
}

/// M54.6: what's open while it is, for `ferrule doctor` and the
/// dashboard's Home: owner mode running, the sandbox off, the approval
/// gates off. Each says what it opens and how to close it.
pub fn openings(cfg: &Config) -> Vec<Opening> {
    let mut out = Vec::new();
    if on(cfg) {
        // M54.5: the one gate left, or that it's off too.
        let asks = if cfg.trust.untrusted_send_gate {
            "only a send, a force-push or a DELETE after it has read outside content asks you \
             first"
        } else {
            "nothing asks you first, not even a send after it has read outside content ([trust] \
             untrusted_send_gate = false)"
        };
        out.push(Opening {
            id: "open-owner",
            what: format!(
                "Owner mode is on: the bot's commands can write everywhere your account can \
                 except the keys and ferrule's own files, the network is open, and {asks}."
            ),
            close: "Settings → Autonomy → Ask me first closes it (the gateway restarts).",
        });
    }
    if cfg.sandbox.mode == Mode::Off {
        out.push(Opening {
            id: "open-sandbox-off",
            what: "The sandbox is off ([sandbox] mode = \"off\"): the bot's commands can read \
                   your saved keys and rewrite ferrule's config and binary."
                .into(),
            close: "Settings → Autonomy → Ask me first (or Protected autonomous) turns it back \
                    on; on the machine, set [sandbox] mode = \"workspace-write\" and restart.",
        });
    }
    if !cfg.trust.gates {
        out.push(Opening {
            id: "open-gates-off",
            what: "Approval gates are off ([trust] gates = false): recursive deletes, \
                   force-pushes, DELETEs to bound hosts and sends through connected services \
                   run without asking."
                .into(),
            close: "Settings → Autonomy → Ask me first removes it; on the machine, delete \
                    gates = false under [trust] and restart.",
        });
    }
    out
}

/// The session transcripts and the learning loop's notes: hidden from
/// every command outside owner mode, and in owner mode from everyone's but
/// the owner's (§7).
pub fn history() -> Vec<PathBuf> {
    config::data_dir()
        .map(|data| vec![data.join("sessions"), data.join("learn")])
        .unwrap_or_default()
}

/// Where the sessions that aren't the owner's keep their long-term memory,
/// one file per chat ([`guest_memory`]).
const GUEST_MEMORY: &str = "guest-memory";

/// What only the owner's sessions may read, hidden from everyone else's
/// commands and file tools in every mode: [`history`], the owner's
/// long-term memory (`memory.db` and SQLite's files beside it), the other
/// chats' memories, the learned skills (the pass keeps some for the
/// owner's chats), and M61's captures of the owner's screen in
/// `workspace`.
pub fn guest_hidden(workspace: &Path) -> Vec<PathBuf> {
    let mut hidden = vec![workspace.join(crate::screen::DIR)];
    let Ok(data) = config::data_dir() else {
        return hidden;
    };
    hidden.extend(history());
    hidden.extend(
        [
            "memory.db",
            "memory.db-wal",
            "memory.db-shm",
            GUEST_MEMORY,
            "learned-skills",
        ]
        .map(|rel| data.join(rel)),
    );
    hidden
}

/// The long-term memory of a session that isn't the owner's: a file of its
/// own per chat it traces to ([`chat_of`]), so no other chat's facts, the
/// owner's profile above all, reach it, and nothing it stores reaches
/// anyone else.
pub fn guest_memory(tree: &str) -> anyhow::Result<PathBuf> {
    let dir = config::data_dir()?.join(GUEST_MEMORY);
    std::fs::create_dir_all(&dir)?;
    let name: String = chat_of(tree)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(dir.join(format!("{name}.db")))
}

/// The chat a run tree traces to, the way [`owner_tree`] traces it: a
/// chat's own session, the chat a scheduled task was made in, the chat a
/// plan, goal or graph run was started from; otherwise the tree itself.
pub fn chat_of(tree: &str) -> String {
    if crate::channels::of_session(tree).is_some() {
        return tree.to_string();
    }
    if let Some(id) = tree.strip_prefix(&format!("{SCHEDULER_PSEUDO_CHANNEL}__")) {
        return match task(id) {
            Some(t) if !t.chat_id.is_empty() => {
                ferrule_gateway::session::session_id(&t.channel, &t.chat_id)
            }
            _ => tree.to_string(),
        };
    }
    ORIGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tree)
        .cloned()
        .unwrap_or_else(|| tree.to_string())
}

/// Whether the run tree `tree` is the owner's (§1): the owner's private
/// chat, a Telegram group while the config asks for owner mode, the
/// dashboard chat or the gateway's own terminal; a scheduled task made
/// there or with no chat; a plan, goal or graph run started there; and runs
/// from the terminal. Every other chat, whatever was started from one, and
/// a run in the gateway that no chat traces to are someone else's. A
/// sub-agent's tree is its root's, so it follows the root.
///
/// In owner mode the owner's trees get it and everyone else's
/// `workspace-write`. In every mode only the owner's see the owner's
/// long-term memory, learned lessons and skills (docs/memory.md, "Whose
/// memory").
pub fn owner_tree(cfg: &Config, hub: &Hub, tree: &str) -> bool {
    let groups = wanted(cfg);
    if owner_chat(hub, tree, groups) {
        return true;
    }
    if crate::channels::of_session(tree).is_some() {
        return false;
    }
    if let Some(id) = tree.strip_prefix(&format!("{SCHEDULER_PSEUDO_CHANNEL}__")) {
        return owner_task(hub, id, groups);
    }
    let origin = ORIGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tree)
        .cloned();
    if let Some(origin) = origin {
        return owner_chat(hub, &origin, groups);
    }
    // `ferrule run`, `chat` and the other runs from a terminal.
    GATEWAY.get().is_none()
}

/// The owner's private chat, the dashboard chat, the gateway's own terminal
/// (`[gateway] local = true`), where whoever types started it, or with
/// `groups` a Telegram group (see [`owner_group`]).
fn owner_chat(hub: &Hub, session: &str, groups: bool) -> bool {
    crate::self_service::offered(hub, session).is_some()
        || session.starts_with("local__")
        || (groups && owner_group(session).is_some())
}

/// A Telegram group's session, as the chat its answers go to. Only the
/// owner's messages reach it whenever the config asks for owner mode
/// (`TelegramChannel::with_only_owner_in_groups`), so then a group is the
/// owner's chat. Otherwise anyone allowed there may write in it, and it is
/// someone else's.
pub fn owner_group(session: &str) -> Option<ChatRef> {
    let (info, chat) = crate::channels::of_session(session)?;
    (info.name == "telegram" && chat.parse::<i64>().ok()? < 0)
        .then(|| ChatRef::new(info.name, chat))
}

/// A scheduled task made in an owner chat, or with no chat. One that can't
/// be looked up is someone else's.
fn owner_task(hub: &Hub, id: &str, groups: bool) -> bool {
    match task(id) {
        Some(t) if t.chat_id.is_empty() => true,
        Some(t) => owner_chat(
            hub,
            &ferrule_gateway::session::session_id(&t.channel, &t.chat_id),
            groups,
        ),
        None => false,
    }
}

/// The scheduled task `id`, when it can be looked up.
fn task(id: &str) -> Option<ferrule_gateway::Task> {
    config::data_dir()
        .ok()
        .and_then(|data| TaskStore::open(data.join("tasks.db")).ok())
        .and_then(|store| store.get(id).ok().flatten())
}

/// Why owner mode can't run here, in one sentence the owner reads, or
/// `None` when it can. The gateway then falls back to `workspace-write`.
pub fn refusal(cfg: &Config, workspace: &Path) -> Option<String> {
    if !wanted(cfg) {
        return None;
    }
    // In the gateway that's also §3's: the binary couldn't be moved into
    // `<data>/bin` ([`relocate`] runs first).
    if let Some(why) = fell_back() {
        return Some(why.to_string());
    }
    if let Some(why) = unheld() {
        return Some(why);
    }
    let workspace = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let inside = protected_paths()
        .into_iter()
        .filter_map(|p| dunce::canonicalize(p).ok())
        .find(|p| workspace.starts_with(p));
    if let Some(p) = inside {
        return Some(format!(
            "the workspace {} is inside {}, which owner mode keeps read-only",
            crate::setup::tilde(&workspace),
            crate::setup::tilde(&p)
        ));
    }
    Sandbox::new(crate::sandbox_policy(cfg)).err()
}

/// Why owner mode can't run in this process, checked before its sandbox is
/// tried: another ferrule instance on this account, or outside the
/// gateway, a binary the service doesn't run from `<data>/bin` yet (the
/// gateway moves it itself: [`relocate`]).
fn unheld() -> Option<String> {
    if let Some(why) = other_instances() {
        return Some(why);
    }
    (GATEWAY.get().is_none() && !relocated())
        .then(|| "owner mode starts with the service, which runs ferrule from its data dir".into())
}

/// §2 protects one instance's files: beside another instance on this
/// account, each bot could write what the other runs. Not knowing counts
/// as another one.
fn other_instances() -> Option<String> {
    const ONLY: &str = "owner mode needs this to be the only ferrule instance on this account";
    let places = match crate::instances::Places::here() {
        Ok(places) => places,
        Err(e) => {
            return Some(format!(
                "{ONLY}, and the others couldn't be looked for: {e:#}"
            ))
        }
    };
    let me = crate::instance::current();
    let others: Vec<String> = crate::instances::discover(&places)
        .into_iter()
        .filter(|f| f.scope == service::scope() && f.name != me)
        .map(|f| f.label().to_string())
        .collect();
    (!others.is_empty()).then(|| format!("{ONLY} (found: {})", others.join(", ")))
}

/// §3 holds for this process: it runs from `<data>/bin`, and the service's
/// units name exactly that file. Outside the gateway owner mode needs it.
pub fn relocated() -> bool {
    let (Ok(exe), Ok(data)) = (
        std::env::current_exe().and_then(dunce::canonicalize),
        config::data_dir(),
    ) else {
        return false;
    };
    in_bin(&exe, &data.join("bin")) && units_name(&exe)
}

/// Whether `exe` is under `bin`.
fn in_bin(exe: &Path, bin: &Path) -> bool {
    dunce::canonicalize(bin).is_ok_and(|bin| exe.starts_with(bin))
}

/// Whether the service's gateway unit names exactly `exe`, and its update
/// unit too when there is one. A unit that doesn't read back doesn't.
fn units_name(exe: &Path) -> bool {
    service::installed_exe().as_deref() == Some(exe)
        && service::update_unit_file()
            .filter(|unit| unit.exists())
            .is_none_or(|unit| service::installed_exe_at(&unit).as_deref() == Some(exe))
}

/// Whether every unit of the service reads back, and none names `file`.
fn units_skip(file: &Path) -> bool {
    service::installed_exe().is_some_and(|exe| exe.as_path() != file)
        && service::update_unit_file()
            .filter(|unit| unit.exists())
            .is_none_or(|unit| {
                service::installed_exe_at(&unit).is_some_and(|exe| exe.as_path() != file)
            })
}

/// Whether the service manager started this process, so a clean exit
/// restarts it.
fn by_service() -> bool {
    use crate::self_service::restart::{how, How};
    how(
        crate::managed::on(),
        crate::last_good::supervised(),
        cfg!(unix),
    ) == How::Terminate
}

/// What [`relocate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// The binary runs from `<data>/bin`, and the service's units name it.
    Already,
    /// The units were just pointed at `<data>/bin`, or the file there just
    /// got an inode of its own: this process exits, and the service starts
    /// it from there.
    Restarting,
}

/// §3: puts the binary this process runs at `<data>/bin/ferrule`, which
/// owner mode keeps read-only, and points the service's units there without
/// restarting it; the caller exits for that. The original is never renamed
/// away: the new file is a hard link (or a copy), the units are rewritten,
/// and only then is the old path swapped for a symlink, in one step. So
/// wherever this stops, the units name a real file. Only the only instance
/// here, with a systemd user service that started this process, is moved
/// so: anything else is an error before anything changes.
pub fn relocate() -> Result<Moved, String> {
    let exe = std::env::current_exe()
        .and_then(dunce::canonicalize)
        .map_err(|e| format!("I can't tell which file runs me: {e}"))?;
    let bin = config::data_dir()
        .map_err(|e| format!("{e:#}"))?
        .join("bin");
    let moved = in_bin(&exe, &bin);
    if moved && units_name(&exe) {
        // A move cut short before the swap leaves the old path a hard link,
        // a name commands could change this file through.
        let copied = detach(&exe).map_err(|e| {
            format!(
                "{} has a second name commands could change it through, and it couldn't get a copy of its own: {e}",
                tilde(&exe)
            )
        })?;
        if !copied {
            return Ok(Moved::Already);
        }
        // This process runs the inode left behind; what starts next runs
        // the new one.
        if by_service() {
            eprintln!(
                "ferrule: {} has a file of its own now; restarting onto it",
                exe.display()
            );
            return Ok(Moved::Restarting);
        }
        crate::lifecycle::set_reexec_path(exe);
        return Ok(Moved::Already);
    }
    // The sandbox's own reason, which it gives before any probe.
    if !cfg!(target_os = "linux") {
        let owner = Policy {
            mode: Mode::Owner,
            ..Policy::default()
        };
        return Err(Sandbox::new(owner).err().unwrap_or_default());
    }
    if let Some(why) = other_instances() {
        return Err(why);
    }
    let installed = matches!(service::status(), service::Status::Installed { .. });
    if !installed || service::scope() == service::Scope::System {
        let instance = crate::instance::current();
        let system = service::Svc::new(instance.as_deref(), service::Scope::System);
        return Err(if system.system_unit_path().exists() {
            "owner mode needs a systemd user service; a system service's unit is root's to change"
                .into()
        } else {
            "owner mode needs ferrule to run as a service".into()
        });
    }
    if !by_service() {
        return Err(
            "owner mode moves ferrule only when its service starts it, and this gateway was started by hand"
                .into(),
        );
    }
    let target = if moved {
        // In place, but the units name another path (the old link, say).
        point(&exe)?;
        exe
    } else {
        move_in(&exe, bin)?
    };
    eprintln!(
        "ferrule: the service runs {} now, for owner mode; restarting onto it",
        target.display()
    );
    Ok(Moved::Restarting)
}

/// The move: `<bin>/ferrule` made the same file as `exe`, the units pointed
/// at it, then `exe` swapped for a symlink to it. An error before the units
/// name it removes it again and leaves `exe` as it was.
fn move_in(exe: &Path, bin: PathBuf) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&bin).map_err(|e| format!("creating {}: {e}", tilde(&bin)))?;
    let new = dunce::canonicalize(&bin).unwrap_or(bin).join("ferrule");
    place(exe, &new).map_err(|e| format!("couldn't put ferrule at {}: {e}", tilde(&new)))?;
    if let Err(why) = point(&new) {
        // A unit written before the error may name it; then it stays.
        if units_skip(&new) {
            let _ = std::fs::remove_file(&new);
        }
        return Err(format!("{why}; {} is as it was", tilde(exe)));
    }
    if let Err(e) = swap_in_link(&new, exe) {
        // Still a hard link: the next start's `detach` splits it.
        eprintln!(
            "ferrule: couldn't put a link at {}: {e}; it stays a file",
            exe.display()
        );
    }
    Ok(new)
}

/// The service's units rewritten to run `exe` and reloaded, without a
/// restart, then read back.
fn point(exe: &Path) -> Result<(), String> {
    crate::setup::refresh_service_with(Some(exe.to_path_buf()))
        .map_err(|e| format!("couldn't point the service at {}: {e:#}", tilde(exe)))?;
    if !units_name(exe) {
        return Err(format!("the service's units don't name {}", tilde(exe)));
    }
    Ok(())
}

/// `new` made the same file as `old`: a hard link where the filesystem
/// allows it, else a copy (0755). Anything else at `new` is replaced in one
/// step, so `new` is never missing on the way.
fn place(old: &Path, new: &Path) -> std::io::Result<()> {
    if inode(old).zip(inode(new)).is_some_and(|(a, b)| a.0 == b.0) {
        return Ok(());
    }
    let tmp = beside(new, ".ferrule-new");
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(old, &tmp)
        .and_then(|()| std::fs::rename(&tmp, new))
        .is_ok()
    {
        return Ok(());
    }
    let _ = std::fs::remove_file(&tmp);
    copy_over(old, new)
}

/// Gives `file` an inode of its own when another name shares it; says
/// whether it did.
fn detach(file: &Path) -> std::io::Result<bool> {
    let shared = inode(file).is_some_and(|(_, names)| names > 1);
    if shared {
        copy_over(file, file)?;
    }
    Ok(shared)
}

/// `from` copied over `to` (mode 0755) through a file beside it, so `to` is
/// never half written, nor written through a link. The copy is on disk
/// before the rename.
fn copy_over(from: &Path, to: &Path) -> std::io::Result<()> {
    let tmp = beside(to, ".ferrule-new");
    let _ = std::fs::remove_file(&tmp);
    let copied = std::fs::copy(from, &tmp).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)?
            .sync_all()?;
        std::fs::rename(&tmp, to)
    });
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied
}

/// `old` swapped for a symlink to `new` in one step: the link is made
/// beside it, then renamed over it.
fn swap_in_link(new: &Path, old: &Path) -> std::io::Result<()> {
    let link = beside(old, ".ferrule-link");
    let _ = std::fs::remove_file(&link);
    symlink(new, &link)?;
    std::fs::rename(&link, old).inspect_err(|_| {
        let _ = std::fs::remove_file(&link);
    })
}

/// `path` with `suffix` added to its file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// The file `path` names (a link's own, not its target) as device and
/// inode, and how many names it has.
#[cfg(unix)]
fn inode(path: &Path) -> Option<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(((meta.dev(), meta.ino()), meta.nlink()))
}

/// Owner mode is Linux only, so this is never reached.
#[cfg(not(unix))]
fn symlink(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(not(unix))]
fn inode(_: &Path) -> Option<((u64, u64), u64)> {
    None
}

/// What runs commands when `Sandbox::new` failed with `why` and the config
/// asks for owner mode: `workspace-write` with `require` on (owner mode is
/// off from then on), or when that can't be applied either, a sandbox that
/// refuses every command, so chat still works. Never one that runs them
/// unsandboxed. In any other mode `why` stands.
pub fn after_failure(cfg: &Config, why: String) -> Result<Sandbox, String> {
    if !wanted(cfg) {
        return Err(why);
    }
    let why = if on(cfg) {
        fall_back(why);
        match Sandbox::new(crate::sandbox_policy(cfg)) {
            Ok(sandbox) => return Ok(sandbox),
            Err(why) => why,
        }
    } else {
        why
    };
    // The policy's hidden paths stay, for the file tools.
    let policy = Policy {
        mode: Mode::Off,
        ..crate::sandbox_policy(cfg)
    };
    Ok(Sandbox::new(policy)
        .unwrap_or_else(|_| Sandbox::off())
        .refuse(why))
}

/// What the owner is told, in their chat, when owner mode is off this run:
/// why, and what runs instead. `gates`: `[trust] gates`, whether risky
/// commands wait for the owner's yes.
pub fn owner_line(why: &str, commands_refused: bool, gates: bool) -> String {
    let why = why.trim_end_matches('.');
    let instead = match (commands_refused, gates) {
        (true, _) => "I can't run commands; chat still works",
        (false, true) => {
            "commands run in the workspace-write sandbox, and I ask you before risky ones"
        }
        (false, false) => {
            "commands run in the workspace-write sandbox, without approvals ([trust] gates is off)"
        }
    };
    format!("Owner mode is off this run: {why}. Until that's fixed, {instead}.")
}

/// Puts this process's own spawns (§4: hooks, scheduler gates, worktree
/// git, the helpers it runs by name) through the sandbox whenever the
/// config asks for owner mode, with `<data>/worktrees` granted, and for
/// worktree git `workspace` too, so a worker's close-out can commit to the
/// repo there. That's the owner sandbox, and after a fallback one built
/// from the owner policy all the same ([`fallback_launcher`]). A sandbox
/// that can't be built refuses them instead, so none runs outside one.
/// Only the gateway's has the credential proxy; elsewhere none is started
/// for it.
///
/// Outside the gateway (which asks [`refusal`] itself), owner mode that
/// can't hold here falls back first: beside another instance, or before
/// the service runs ferrule from `<data>/bin` (§3).
pub fn arm(cfg: &Config, workspace: Option<&Path>) {
    if !wanted(cfg) {
        return;
    }
    if on(cfg) && GATEWAY.get().is_none() {
        if let Some(why) = unheld() {
            fall_back(why);
        }
    }
    let built = if on(cfg) {
        let built = match GATEWAY.get() {
            Some(()) => crate::shared_sandbox(cfg)
                .map(|s| Sandbox::clone(&s))
                .map_err(|e| format!("{e:#}")),
            None => Sandbox::new(crate::sandbox_policy(cfg)).or_else(|why| after_failure(cfg, why)),
        };
        // Building it may have fallen back just now.
        match built {
            Ok(_) if !on(cfg) => fallback_launcher(cfg),
            built => built,
        }
    } else {
        fallback_launcher(cfg)
    };
    let (spawns, git) = match (built, config::data_dir()) {
        (Ok(sandbox), Ok(data)) => {
            let worktrees = data.join("worktrees");
            let workspace: Vec<PathBuf> = workspace
                .map(|w| dunce::canonicalize(w).unwrap_or_else(|_| w.to_path_buf()))
                .into_iter()
                .collect();
            (
                sandbox.for_helper(&worktrees, &[]),
                sandbox.for_helper(&worktrees, &workspace),
            )
        }
        (Err(why), _) => (
            Sandbox::off().refuse(why.clone()),
            Sandbox::off().refuse(why),
        ),
        (_, Err(e)) => {
            let why = format!("{e:#}");
            (
                Sandbox::off().refuse(why.clone()),
                Sandbox::off().refuse(why),
            )
        }
    };
    host::set(spawns);
    sandbox_git(git);
}

/// The process's own spawns after a fallback: the owner policy as owner
/// mode builds it, with ferrule's own binary protected too, so helpers keep
/// writing their own state while the keys stay hidden and ferrule's files
/// read-only. Building it flips nothing: owner mode stays off, and the
/// bot's sessions keep the fallback sandbox. The gateway's gets the
/// credential proxy, as its commands do.
fn fallback_launcher(cfg: &Config) -> Result<Sandbox, String> {
    let mut policy = crate::owner_policy(cfg);
    if let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|exe| dunce::canonicalize(exe).ok())
    {
        policy.protected.push(exe);
    }
    let sandbox = Sandbox::new(policy)?;
    if GATEWAY.get().is_none() {
        return Ok(sandbox);
    }
    let sandbox = match crate::shared_broker(cfg).map_err(|e| format!("{e:#}"))? {
        Some(broker) => sandbox.with_env(broker.child_env()).with_egress(Some(
            crate::tool_egress(broker).map_err(|e| format!("{e:#}"))?,
        )),
        None => sandbox,
    };
    Ok(crate::managed::guard(sandbox))
}

/// Sub-agent worktree git through `sandbox` (§4), so what a repo's config
/// names (filters, the pager, ssh) runs confined too. Once per process;
/// the first call wins.
pub fn sandbox_git(sandbox: Sandbox) {
    ferrule_agents::worktree::set_git(Arc::new(move |dir: &Path| {
        sandbox.command("git", Vec::<&str>::new(), dir)
    }));
}
