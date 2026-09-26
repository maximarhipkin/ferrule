//! M32: WASM plugins through M13's flow — allow-list, exact pin, scan,
//! approval queue — plus the capabilities the owner grants. A plugin runs
//! in-process, so there is no server to start: "live" is a loaded module
//! and its tools, checked against the lock before every load.

use super::*;
use crate::lock::PluginEntry;
use crate::source::PluginRequest;
use ferrule_plugins::manifest::sha256_hex;
use ferrule_plugins::{Capabilities, Manifest, Plugin, PluginError, MANIFEST_FILE};
use serde_json::json;
use std::time::SystemTime;

/// The largest `.wasm` accepted (the runtime's own limit).
const MAX_WASM: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Size and mtime of each file in a plugin's directory: a cheap way to
/// notice the files changing under a loaded plugin between syncs.
type Stamp = Vec<(String, u64, Option<SystemTime>)>;

pub(super) struct LivePlugin {
    entry: PluginEntry,
    stamp: Stamp,
    plugin: Arc<Plugin>,
    tools: Vec<Arc<dyn Tool>>,
}

/// A plugin fetched, hash-checked, loaded and scanned, not yet committed.
struct PreparedPlugin {
    source: String,
    pin: Option<String>,
    manifest: Manifest,
    manifest_text: String,
    wasm: Vec<u8>,
    plugin: Arc<Plugin>,
    findings: Vec<Finding>,
    /// Model-facing tool name (and the plugin's own name) → digest.
    digests: BTreeMap<String, String>,
}

enum Where {
    Git(Source),
    Url(Source),
    Local(PathBuf),
}

impl ExtensionManager {
    /// The model's `plugin_add`. An allow-listed git or url source installs
    /// at once unless it asks for files, network or secrets beyond what was
    /// already granted; everything else waits for the owner, who is shown
    /// the capabilities. A local directory is never allow-listed.
    pub async fn install_plugin(self: &Arc<Self>, mut req: PluginRequest) -> Result<Outcome> {
        let _ops = self.ops.lock().await;
        req.capabilities = None;
        match locate(&req.source)? {
            Where::Local(dir) => {
                let dir = self.in_workspace(&dir)?;
                req.source = dir.to_string_lossy().into_owned();
                let prepared = self.prepare_plugin(&req, None).await?;
                self.refuse_blocked(&req, &prepared)?;
                self.check_plugin_slot(&prepared.manifest.name, req.replace)?;
                req.capabilities = Some(prepared.manifest.capabilities.clone());
                self.ask_owner_plugin(
                    req,
                    "a plugin from a local directory always needs the owner's approval",
                    Some(prepared),
                )
                .await
            }
            Where::Git(src) | Where::Url(src) => {
                if !self.cfg.allow.covers(&src) {
                    let reason = format!("{src} is not on the allow-list");
                    return self.ask_owner_plugin(req, &reason, None).await;
                }
                let prepared = self.prepare_plugin(&req, Some(&self.cfg.allow)).await?;
                self.refuse_blocked(&req, &prepared)?;
                self.check_plugin_slot(&prepared.manifest.name, req.replace)?;
                let caps = prepared.manifest.capabilities.clone();
                let asks = owner_asks(&caps, &self.granted(&prepared.manifest.name)?);
                if !asks.is_empty() {
                    req.capabilities = Some(caps);
                    let reason = format!("it asks for {}", asks.join("; "));
                    return self.ask_owner_plugin(req, &reason, Some(prepared)).await;
                }
                self.commit_plugin(prepared, Origin::Agent, vec![], req.replace)
            }
        }
    }

    /// The owner's `ferrule plugins add`: no allow-list, but the same
    /// fetch, hash check, load and scan, and a review of the capabilities.
    /// Block hits the owner confirms become waivers.
    pub async fn install_plugin_as_owner(
        self: &Arc<Self>,
        mut req: PluginRequest,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<Outcome> {
        let _ops = self.ops.lock().await;
        req.capabilities = None;
        let prepared = self.prepare_plugin(&req, None).await?;
        let name = prepared.manifest.name.clone();
        if !req.replace && self.store.load()?.plugins.contains_key(&name) {
            return Err(refused(format!(
                "a plugin `{name}` is already installed; pass --replace to reinstall it"
            )));
        }
        let old = self
            .store
            .load()?
            .plugins
            .get(&name)
            .map(|e| e.capabilities.clone());
        let review = review_of(&prepared, old.as_ref());
        if !confirm(&review) {
            return Err(refused("not approved; nothing was installed"));
        }
        let waivers = waivers_for(
            &unwaived(&prepared.findings, &prepared.digests, &[]),
            &prepared.digests,
        );
        self.commit_plugin(prepared, Origin::Owner, waivers, req.replace)
    }

    pub async fn remove_plugin(&self, name: &str, by_owner: bool) -> Result<()> {
        let _ops = self.ops.lock().await;
        let lock = self.store.load()?;
        let Some(entry) = lock.plugins.get(name) else {
            return Err(refused(format!("no installed plugin named `{name}`")));
        };
        if !by_owner && entry.origin == Origin::Owner {
            return Err(refused(format!(
                "`{name}` was installed by the owner; only they can remove it"
            )));
        }
        self.store.update(|l| {
            l.plugins.remove(name);
            let _ = fs::remove_dir_all(self.cfg.layout.plugins_dir().join(name));
            Ok(())
        })?;
        self.plugins.write().unwrap().remove(name);
        Ok(())
    }

    // ---- used by the manager's shared paths ----------------------------------

    pub(super) fn plugin_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.plugins
            .read()
            .unwrap()
            .values()
            .flat_map(|l| l.tools.iter().cloned())
            .collect()
    }

    /// Rebuild every live plugin's tools over the current sandbox (M17: a
    /// secret bound mid-run is then offered as a placeholder).
    pub(super) fn rebuild_plugin_tools(&self) {
        let sandbox = self.sandbox();
        for l in self.plugins.write().unwrap().values_mut() {
            l.tools = ferrule_plugins::tools(l.plugin.clone(), &sandbox);
        }
    }

    /// `approve` for a queued plugin request: the owner reviews the real
    /// capabilities, whatever the request said.
    pub(super) async fn approve_plugin(
        self: &Arc<Self>,
        req: PluginRequest,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<Outcome> {
        let prepared = self.prepare_plugin(&req, None).await?;
        let old = self.granted_entry(&prepared.manifest.name)?;
        let review = review_of(&prepared, old.as_ref());
        if !confirm(&review) {
            return Err(refused("not approved; nothing was installed"));
        }
        let waivers = waivers_for(
            &unwaived(&prepared.findings, &prepared.digests, &[]),
            &prepared.digests,
        );
        self.commit_plugin(prepared, Origin::Agent, waivers, req.replace)
    }

    /// `resume` for a suspended plugin: re-read its files, show what they
    /// are now, and approve that on yes.
    pub(super) fn resume_plugin(
        &self,
        name: &str,
        entry: PluginEntry,
        confirm: impl FnOnce(&Review) -> bool,
    ) -> Result<()> {
        if entry.status != Status::Suspended {
            return Err(refused(format!("`{name}` is not suspended")));
        }
        let dir = self.cfg.layout.plugins_dir().join(name);
        let (_, m, wasm) = read_dir_files(&dir)?;
        if m.name != name {
            return Err(refused(format!(
                "its manifest now names it `{}`; remove and reinstall it",
                m.name
            )));
        }
        let plugin = load(&m, &wasm)?;
        let (findings, digests) = surface(&m);
        let review = Review {
            what: format!("resume WASM plugin `{name}` from {}", entry.source),
            items: tool_names(&m),
            findings: findings.clone(),
            sandbox_degraded: None,
            capabilities: cap_lines(&m.capabilities, Some(&entry.capabilities)),
        };
        if !confirm(&review) {
            return Err(refused("not resumed"));
        }
        let mut waivers = entry.waivers.clone();
        waivers.extend(waivers_for(
            &unwaived(&findings, &digests, &entry.waivers),
            &digests,
        ));
        let updated = self.store.update(|l| {
            let e = l
                .plugins
                .get_mut(name)
                .ok_or_else(|| refused(format!("`{name}` was removed meanwhile")))?;
            e.version = m.version.clone();
            e.wasm_sha256 = m.sha256.clone();
            e.manifest_sha256 = m.digest();
            e.capabilities = m.capabilities.clone();
            e.tools = tool_digests(&m);
            e.status = Status::Active;
            e.reason = None;
            e.waivers = waivers.clone();
            Ok(e.clone())
        })?;
        self.go_live(name, updated, Arc::new(plugin), stamp_of(&dir));
        Ok(())
    }

    /// `sync` for plugins: load active entries whose lock entry or files
    /// changed, re-checking manifest digest, `.wasm` hash and capabilities;
    /// anything off suspends the plugin. Unload the rest.
    pub(super) fn sync_plugins(&self, lock: &LockFile) {
        let dir = self.cfg.layout.plugins_dir();
        for (name, entry) in &lock.plugins {
            if entry.status == Status::Suspended {
                self.plugins.write().unwrap().remove(name);
                continue;
            }
            let stamp = stamp_of(&dir.join(name));
            let unchanged = self
                .plugins
                .read()
                .unwrap()
                .get(name)
                .is_some_and(|l| l.entry == *entry && l.stamp == stamp);
            if unchanged {
                continue;
            }
            self.plugins.write().unwrap().remove(name);
            if !ferrule_plugins::AVAILABLE {
                if !self.no_plugin_runtime.swap(true, Ordering::Relaxed) {
                    tracing::warn!("installed plugins are not loaded: this ferrule was built without plugin support");
                }
                continue;
            }
            match load_installed(&dir.join(name), name, entry) {
                Ok(plugin) => self.go_live(name, entry.clone(), Arc::new(plugin), stamp),
                Err(reason) => self.suspend_plugin(name, &reason),
            }
        }
        self.plugins
            .write()
            .unwrap()
            .retain(|n, _| lock.plugins.contains_key(n));
    }

    pub(super) fn list_plugins(&self, lock: &LockFile, out: &mut Vec<Listed>) {
        let live = self.plugins.read().unwrap();
        for (name, e) in &lock.plugins {
            out.push(Listed {
                name: name.clone(),
                kind: "plugin",
                origin: e.origin.as_str().into(),
                source: e.source.clone(),
                pin: e.pin.clone(),
                status: match (e.status, live.contains_key(name)) {
                    (Status::Suspended, _) => "suspended",
                    (Status::Active, true) => "active",
                    (Status::Active, false) => "not loaded",
                }
                .into(),
                reason: e.reason.clone(),
                tools: e.tools.keys().cloned().collect(),
            });
        }
    }

    // ---- internals -------------------------------------------------------------

    /// Queue the request and ask the approver. A yes installs now if the
    /// scan is clean and the owner was shown every capability the plugin
    /// asks for; when they weren't (the source wasn't fetched yet), it is
    /// asked once more with them.
    async fn ask_owner_plugin(
        self: &Arc<Self>,
        mut req: PluginRequest,
        reason: &str,
        mut prepared: Option<PreparedPlugin>,
    ) -> Result<Outcome> {
        let approver = self.approver.read().unwrap().clone();
        loop {
            let p = self
                .queue
                .add(Request::Plugin(req.clone()), reason, vec![])?;
            match approver.decide(&p).await {
                None => return Ok(Outcome::Pending { id: p.id }),
                Some(false) => {
                    self.queue.remove(&p.id)?;
                    return Err(refused("the owner denied it"));
                }
                Some(true) => {}
            }
            let ready = match prepared.take() {
                Some(x) => x,
                None => match self.prepare_plugin(&req, None).await {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = self.queue.remove(&p.id);
                        return Err(e);
                    }
                },
            };
            let blocks = unwaived(&ready.findings, &ready.digests, &[]);
            if !blocks.is_empty() {
                return Err(refused(format!(
                    "the owner allowed the source, but the scan flagged {}; nothing was installed. It waits as {} for the owner's review",
                    scan::summary_for_model(&blocks),
                    p.id
                )));
            }
            let caps = ready.manifest.capabilities.clone();
            let shown = match &req.capabilities {
                Some(c) => caps.is_subset_of(c),
                None => owner_asks(&caps, &self.granted(&ready.manifest.name)?).is_empty(),
            };
            self.queue.remove(&p.id)?;
            if !shown {
                // Only possible while `req.capabilities` was None: the
                // next round shows them.
                req.capabilities = Some(caps);
                prepared = Some(ready);
                continue;
            }
            return self.commit_plugin(ready, Origin::Agent, vec![], req.replace);
        }
    }

    async fn prepare_plugin(
        &self,
        req: &PluginRequest,
        allow: Option<&AllowList>,
    ) -> Result<PreparedPlugin> {
        let (source, pin, text, manifest, wasm) = match locate(&req.source)? {
            Where::Local(dir) => {
                if req.path.is_some() {
                    return Err(refused("`path` is only for git sources"));
                }
                let (text, m, wasm) = read_dir_files(&dir)?;
                (dir.to_string_lossy().into_owned(), None, text, m, wasm)
            }
            Where::Git(src) => {
                let staging = self.cfg.layout.staging();
                fs::create_dir_all(&staging)?;
                let clone = staging.join(uuid::Uuid::new_v4().simple().to_string());
                let got = (|| {
                    let sha =
                        crate::git::fetch_pinned(&src.locator, src.version.as_deref(), &clone)?;
                    if allow.is_some_and(|a| !a.permits(&src, &sha)) {
                        return Err(refused(format!(
                            "{src} resolved to {sha}, which the allow-list's commit pin doesn't allow"
                        )));
                    }
                    let dir = plugin_dir_in(&clone, req.path.as_deref())?;
                    Ok((sha, read_dir_files(&dir)?))
                })();
                let _ = fs::remove_dir_all(&clone);
                let (sha, (text, m, wasm)) = got?;
                (src.to_string(), Some(sha), text, m, wasm)
            }
            Where::Url(src) => {
                if req.path.is_some() {
                    return Err(refused("`path` is only for git sources"));
                }
                let Some(want) = &req.sha256 else {
                    return Err(refused(
                        "a url source needs `sha256`: the SHA-256 of the plugin's .wasm",
                    ));
                };
                if allow.is_some_and(|a| !a.permits(&src, "")) {
                    return Err(refused(format!(
                        "{src} is not allowed by the allow-list's version pin"
                    )));
                }
                let (text, m, wasm) = fetch_url(&src.locator).await?;
                (
                    src.to_string(),
                    Some(want.to_ascii_lowercase()),
                    text,
                    m,
                    wasm,
                )
            }
        };
        if let Some(want) = &req.sha256 {
            if !want.eq_ignore_ascii_case(&manifest.sha256) {
                return Err(refused(format!(
                    "the plugin's manifest pins the .wasm to {}, not the requested {want}; refusing it",
                    manifest.sha256
                )));
            }
        }
        let plugin = load(&manifest, &wasm)?;
        let (findings, digests) = surface(&manifest);
        Ok(PreparedPlugin {
            source,
            pin,
            manifest,
            manifest_text: text,
            wasm,
            plugin: Arc::new(plugin),
            findings,
            digests,
        })
    }

    fn commit_plugin(
        &self,
        p: PreparedPlugin,
        origin: Origin,
        waivers: Vec<Waiver>,
        replace: bool,
    ) -> Result<Outcome> {
        let layout = &self.cfg.layout;
        let name = p.manifest.name.clone();
        let dest = layout.plugins_dir().join(&name);
        fs::create_dir_all(layout.staging())?;
        let copy = layout
            .staging()
            .join(format!("{}-plugin", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            fs::create_dir_all(&copy)?;
            fs::write(copy.join(MANIFEST_FILE), &p.manifest_text)?;
            fs::write(copy.join(&p.manifest.wasm), &p.wasm)?;
            self.store.update(|l| {
                if let Some(old) = l.plugins.get(&name) {
                    if !replace {
                        return Err(refused(format!(
                            "a plugin `{name}` is already installed; pass replace=true to reinstall it"
                        )));
                    }
                    if origin != Origin::Owner && old.origin == Origin::Owner {
                        return Err(refused(format!("`{name}` was installed by the owner")));
                    }
                }
                skill::replace_dir(&copy, &dest)?;
                let entry = PluginEntry {
                    source: p.source.clone(),
                    pin: p.pin.clone(),
                    version: p.manifest.version.clone(),
                    wasm_sha256: p.manifest.sha256.clone(),
                    manifest_sha256: p.manifest.digest(),
                    capabilities: p.manifest.capabilities.clone(),
                    tools: tool_digests(&p.manifest),
                    origin,
                    installed_at: lock::now(),
                    status: Status::Active,
                    reason: None,
                    waivers: waivers.clone(),
                };
                l.plugins.insert(name.clone(), entry.clone());
                Ok(entry)
            })
        })();
        let _ = fs::remove_dir_all(&copy);
        let entry = result?;
        let warnings = p.findings.len() - scan::blocks(&p.findings).count();
        let tools = tool_names(&p.manifest);
        self.go_live(&name, entry, p.plugin, stamp_of(&dest));
        Ok(Outcome::Installed {
            name,
            tools,
            warnings,
        })
    }

    fn go_live(&self, name: &str, entry: PluginEntry, plugin: Arc<Plugin>, stamp: Stamp) {
        let tools = ferrule_plugins::tools(plugin.clone(), &self.sandbox());
        self.plugins.write().unwrap().insert(
            name.to_string(),
            LivePlugin {
                entry,
                stamp,
                plugin,
                tools,
            },
        );
    }

    fn suspend_plugin(&self, name: &str, reason: &str) {
        tracing::warn!(plugin = %name, "installed plugin `{name}` suspended: {reason}");
        if let Err(e) = self.store.update(|l| {
            if let Some(e) = l.plugins.get_mut(name) {
                e.status = Status::Suspended;
                e.reason = Some(reason.into());
            }
            Ok(())
        }) {
            tracing::error!(plugin = %name, "couldn't record `{name}` as suspended: {e}");
        }
    }

    fn refuse_blocked(&self, req: &PluginRequest, p: &PreparedPlugin) -> Result<()> {
        let blocks = unwaived(&p.findings, &p.digests, &[]);
        if blocks.is_empty() {
            return Ok(());
        }
        let pending = self.queue.add(
            Request::Plugin(req.clone()),
            "the description scan flagged it",
            p.findings.clone(),
        )?;
        Err(refused(format!(
            "the scan flagged {}; nothing was installed. The owner can review it as {}",
            scan::summary_for_model(&blocks),
            pending.id
        )))
    }

    /// The model may not overwrite without `replace`, nor replace what the
    /// owner installed.
    fn check_plugin_slot(&self, name: &str, replace: bool) -> Result<()> {
        if let Some(e) = self.store.load()?.plugins.get(name) {
            if !replace {
                return Err(refused(format!(
                    "`{name}` is already installed; pass replace=true to reinstall it"
                )));
            }
            if e.origin == Origin::Owner {
                return Err(refused(format!("`{name}` was installed by the owner")));
            }
        }
        Ok(())
    }

    fn granted_entry(&self, name: &str) -> Result<Option<Capabilities>> {
        Ok(self
            .store
            .load()?
            .plugins
            .get(name)
            .map(|e| e.capabilities.clone()))
    }

    /// What an installed plugin of this name was granted; nothing if none.
    fn granted(&self, name: &str) -> Result<Capabilities> {
        Ok(self.granted_entry(name)?.unwrap_or_default())
    }

    /// A local directory the agent names: inside its workspace, where the
    /// sandbox would let it read anyway.
    fn in_workspace(&self, dir: &Path) -> Result<PathBuf> {
        let ws = dunce::canonicalize(&self.cfg.workspace)?;
        let full = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            ws.join(dir)
        };
        let full = dunce::canonicalize(&full)
            .map_err(|_| refused(format!("no directory `{}`", dir.display())))?;
        if !full.starts_with(&ws) {
            return Err(refused(
                "a local plugin must be a directory inside the workspace",
            ));
        }
        Ok(full)
    }
}

/// `git:` and `url:` sources; anything else is a local directory.
fn locate(spec: &str) -> Result<Where> {
    let spec = spec.trim();
    for prefix in ["npm:", "pypi:"] {
        if spec.starts_with(prefix) {
            return Err(refused(
                "plugins install from git:<https url>[@rev], url:<https url of plugin.json>, or a local directory",
            ));
        }
    }
    if spec.starts_with("git:") || spec.starts_with("url:") {
        let src = Source::parse(spec).map_err(refused)?;
        return Ok(match src.kind {
            Kind::Git => Where::Git(src),
            _ => Where::Url(src),
        });
    }
    if spec.is_empty() {
        return Err(refused("`source` is empty"));
    }
    Ok(Where::Local(PathBuf::from(spec)))
}

/// The plugin's directory inside a clone: `path` or the root, which must
/// stay inside it and hold a `plugin.json`.
fn plugin_dir_in(clone: &Path, path: Option<&str>) -> Result<PathBuf> {
    let root = dunce::canonicalize(clone)?;
    let dir = dunce::canonicalize(root.join(path.unwrap_or(".")))
        .map_err(|_| refused(format!("no `{}` in the repo", path.unwrap_or("."))))?;
    if !dir.starts_with(&root) {
        return Err(refused("`path` must stay inside the repo"));
    }
    if !dir.join(MANIFEST_FILE).is_file() {
        return Err(refused(format!(
            "no {MANIFEST_FILE} in `{}` of the repo",
            path.unwrap_or(".")
        )));
    }
    Ok(dir)
}

/// `plugin.json` and the module beside it, both regular files inside `dir`
/// (a symlink out of it is refused) and within the size limits.
fn read_dir_files(dir: &Path) -> Result<(String, Manifest, Vec<u8>)> {
    let dir = dunce::canonicalize(dir)
        .map_err(|_| refused(format!("no plugin directory `{}`", dir.display())))?;
    let text = String::from_utf8(read_inside(&dir, MANIFEST_FILE, MAX_MANIFEST)?)
        .map_err(|_| refused(format!("{MANIFEST_FILE} is not UTF-8")))?;
    let m = Manifest::parse(&text).map_err(|e| refused(e.to_string()))?;
    let wasm = read_inside(&dir, &m.wasm, MAX_WASM)?;
    Ok((text, m, wasm))
}

fn read_inside(dir: &Path, file: &str, max: u64) -> Result<Vec<u8>> {
    let path = dunce::canonicalize(dir.join(file))
        .map_err(|_| refused(format!("the plugin has no `{file}`")))?;
    if !path.starts_with(dir) || !path.is_file() {
        return Err(refused(format!(
            "`{file}` must be a file inside the plugin's directory"
        )));
    }
    if fs::metadata(&path)?.len() > max {
        return Err(refused(format!("`{file}` is over {max} bytes")));
    }
    Ok(fs::read(&path)?)
}

/// Download `plugin.json` and the module next to it (by URL), capped.
async fn fetch_url(locator: &str) -> Result<(String, Manifest, Vec<u8>)> {
    let url = reqwest::Url::parse(locator).map_err(|e| refused(format!("bad url: {e}")))?;
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| refused(format!("http client: {e}")))?;
    let text = String::from_utf8(get_capped(&client, url.clone(), MAX_MANIFEST).await?)
        .map_err(|_| refused(format!("{MANIFEST_FILE} is not UTF-8")))?;
    let m = Manifest::parse(&text).map_err(|e| refused(e.to_string()))?;
    let wasm_url = url
        .join(&m.wasm)
        .map_err(|e| refused(format!("bad module url: {e}")))?;
    if wasm_url.scheme() != "https" {
        return Err(refused("the module must be fetched over https"));
    }
    let wasm = get_capped(&client, wasm_url, MAX_WASM).await?;
    Ok((text, m, wasm))
}

async fn get_capped(client: &reqwest::Client, url: reqwest::Url, max: u64) -> Result<Vec<u8>> {
    let fail = |e: reqwest::Error| refused(format!("fetching {url}: {}", e.without_url()));
    let mut resp = client.get(url.clone()).send().await.map_err(fail)?;
    if !resp.status().is_success() {
        return Err(refused(format!("fetching {url}: HTTP {}", resp.status())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(fail)? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > max {
            return Err(refused(format!("{url} is over {max} bytes")));
        }
    }
    Ok(body)
}

fn load(m: &Manifest, wasm: &[u8]) -> Result<Plugin> {
    Plugin::load(m.clone(), wasm).map_err(|e| {
        refused(match e {
            PluginError::NoRuntime => {
                "this ferrule was built without plugin support (the `plugins` feature); nothing was installed".to_string()
            }
            e @ PluginError::Hash { .. } => e.to_string(),
            e => format!("the plugin doesn't load: {e}"),
        })
    })
}

/// Re-check an installed plugin against its lock entry and load it. The
/// reason, if not, is model-safe: rule and file names only.
fn load_installed(
    dir: &Path,
    name: &str,
    entry: &PluginEntry,
) -> std::result::Result<Plugin, String> {
    let m = ferrule_plugins::read_manifest(dir)
        .map_err(|_| "its manifest is missing or doesn't parse".to_string())?;
    if m.name != name {
        return Err(format!("its manifest now names it `{}`", m.name));
    }
    if m.digest() != entry.manifest_sha256 {
        return Err("its manifest changed since it was approved".into());
    }
    let wider = m.capabilities.widening(&entry.capabilities);
    if !wider.is_empty() {
        return Err(format!(
            "it asks for more than was granted: {}",
            wider.join("; ")
        ));
    }
    let wasm =
        read_inside(dir, &m.wasm, MAX_WASM).map_err(|_| "its .wasm is missing".to_string())?;
    if sha256_hex(&wasm) != entry.wasm_sha256 {
        return Err("its .wasm doesn't match the approved SHA-256".into());
    }
    Plugin::load(m, &wasm).map_err(|e| format!("it doesn't load: {e}"))
}

/// Every description the model will read, scanned: the plugin's own and
/// each tool's (under its model-facing name). Digests bind waivers.
fn surface(m: &Manifest) -> (Vec<Finding>, BTreeMap<String, String>) {
    let mut findings = scan::scan_tool(&m.name, &m.description, &json!({}));
    let mut digests = BTreeMap::from([(m.name.clone(), m.digest())]);
    for t in &m.tools {
        let n = m.tool_name(&t.name);
        findings.extend(scan::scan_tool(&n, &t.description, &t.parameters));
        digests.insert(n, t.digest());
    }
    (findings, digests)
}

fn unwaived(
    findings: &[Finding],
    digests: &BTreeMap<String, String>,
    waivers: &[Waiver],
) -> Vec<Finding> {
    scan::blocks(findings)
        .filter(|f| {
            let d = digests.get(&f.item).map(String::as_str).unwrap_or("");
            !lock::waived(waivers, f, d)
        })
        .cloned()
        .collect()
}

fn tool_names(m: &Manifest) -> Vec<String> {
    m.tools.iter().map(|t| m.tool_name(&t.name)).collect()
}

fn tool_digests(m: &Manifest) -> BTreeMap<String, String> {
    m.tools
        .iter()
        .map(|t| (m.tool_name(&t.name), t.digest()))
        .collect()
}

/// What in `caps` needs the owner beyond `granted`: files, network and
/// secrets. The clock and randomness are harmless and never ask.
fn owner_asks(caps: &Capabilities, granted: &Capabilities) -> Vec<String> {
    let mut c = caps.clone();
    c.clock = false;
    c.random = false;
    c.widening(granted)
}

/// The capability lines of the approval screen; with a previous grant, what
/// is new is called out.
fn cap_lines(caps: &Capabilities, old: Option<&Capabilities>) -> Vec<String> {
    let mut out = caps.describe();
    if let Some(old) = old {
        let wider = caps.widening(old);
        if !wider.is_empty() {
            out.push(format!("NEW since the last approval: {}", wider.join("; ")));
        }
    }
    out
}

fn review_of(p: &PreparedPlugin, old: Option<&Capabilities>) -> Review {
    let m = &p.manifest;
    Review {
        what: format!("WASM plugin `{}` {} from {}", m.name, m.version, p.source),
        items: m
            .tools
            .iter()
            .map(|t| {
                let mut s = m.tool_name(&t.name);
                if t.approval {
                    s.push_str(" (asks before every call)");
                }
                s
            })
            .collect(),
        findings: p.findings.clone(),
        sandbox_degraded: None,
        capabilities: cap_lines(&m.capabilities, old),
    }
}

fn stamp_of(dir: &Path) -> Stamp {
    let mut out: Stamp = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    Some((
                        e.file_name().to_string_lossy().into_owned(),
                        meta.len(),
                        meta.modified().ok(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}
