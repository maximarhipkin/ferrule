//! The workspace walk, the "is this a code repo" check, and the per-file
//! tag cache that makes a refresh cheap.

use crate::lang::{self, SOURCE_EXTENSIONS};
use crate::tags::{self, FileTags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// At most this many files are walked.
pub const MAX_FILES: usize = 20_000;
/// Larger files are skipped (generated code, vendored bundles).
pub const MAX_FILE_BYTES: u64 = 1 << 20;
/// Files that say "this is a project".
pub const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];
const VCS_DIRS: &[&str] = &[".git", ".hg", ".jj"];
/// Source files needed on top of a VCS dir or a manifest.
pub const MIN_SOURCES: usize = 3;
const CACHE_VERSION: u32 = 1;

/// A file found by the walk.
pub struct Walked {
    /// Relative to the root, `/`-separated on every OS.
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: u64,
}

/// A 64-bit FNV-1a hash: stable across runs and builds (the cache file
/// name and the content check; not a security boundary).
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn canonical(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Every file under `root` the tools may read: `.gitignore`/`.ignore`
/// respected (parsed, git is never run), hidden entries and `hidden`
/// (M26's deny list) skipped, symlinks not followed, capped at
/// [`MAX_FILES`], sorted by path. `stop` ends the walk early.
pub fn walk(root: &Path, hidden: &[PathBuf], mut stop: impl FnMut(&Walked) -> bool) -> Vec<Walked> {
    let root = canonical(root);
    let hidden: Vec<PathBuf> = hidden.iter().map(|h| canonical(h)).collect();
    let deny = hidden.clone();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| !deny.iter().any(|h| e.path().starts_with(h)))
        .build();
    let mut out = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(rel) = entry.path().strip_prefix(&root) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        let w = Walked {
            rel,
            path: entry.path().to_path_buf(),
            size: meta.len(),
            mtime_ns,
        };
        let done = stop(&w);
        out.push(w);
        if done || out.len() >= MAX_FILES {
            break;
        }
    }
    out
}

pub(crate) fn extension(rel: &str) -> Option<&str> {
    let name = rel.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

/// A VCS dir or a manifest at the root, and at least [`MIN_SOURCES`]
/// source files: the repo map and `code_search` are for code, not for a
/// home dir or a folder of spreadsheets.
pub fn looks_like_code_repo(root: &Path, hidden: &[PathBuf]) -> bool {
    let marked = VCS_DIRS
        .iter()
        .chain(MANIFESTS)
        .any(|m| root.join(m).exists());
    if !marked {
        return false;
    }
    let mut sources = 0;
    walk(root, hidden, |w| {
        if extension(&w.rel).is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
            && w.size <= MAX_FILE_BYTES
        {
            sources += 1;
        }
        sources >= MIN_SOURCES
    });
    sources >= MIN_SOURCES
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    size: u64,
    mtime_ns: u64,
    hash: u64,
    tags: FileTags,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: BTreeMap<String, Entry>,
}

/// The tags of every supported file, by relative path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<String, FileTags>,
    /// Every file walked (supported or not), for text search.
    pub all_files: Vec<String>,
    /// Files this refresh actually parsed (the rest came from the cache).
    pub parsed: usize,
}

/// A workspace's tags, kept current by [`CodeMap::refresh`] and cached on
/// disk between sessions. Shared by the repo map and `code_search`.
pub struct CodeMap {
    root: PathBuf,
    hidden: Vec<PathBuf>,
    cache_path: Option<PathBuf>,
    state: Mutex<Option<Cache>>,
}

impl CodeMap {
    /// `cache_dir` is where `<fnv64(root)>.json` lives (`None`: memory only).
    pub fn new(root: &Path, hidden: Vec<PathBuf>, cache_dir: Option<&Path>) -> Self {
        let root = canonical(root);
        let cache_path = cache_dir.map(|d| {
            d.join(format!(
                "{:016x}.json",
                fnv64(root.to_string_lossy().as_bytes())
            ))
        });
        Self {
            root,
            hidden,
            cache_path,
            state: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_path(&self) -> Option<&Path> {
        self.cache_path.as_deref()
    }

    /// Stat every file; re-read the ones whose size or mtime changed, and
    /// re-parse only those whose content hash changed. Blocking.
    pub fn refresh(&self) -> Snapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let old = match state.take() {
            Some(c) => c,
            None => self.load(),
        };
        let walked = walk(&self.root, &self.hidden, |_| false);
        let all_files = walked.iter().map(|w| w.rel.clone()).collect();

        let mut files = BTreeMap::new();
        let mut todo: Vec<(&Walked, &'static lang::Lang, Option<&Entry>)> = Vec::new();
        for w in &walked {
            let Some(lang) = extension(&w.rel).and_then(lang::for_extension) else {
                continue;
            };
            if w.size > MAX_FILE_BYTES {
                continue;
            }
            let prev = old.files.get(&w.rel);
            match prev {
                Some(e) if e.size == w.size && e.mtime_ns == w.mtime_ns && !racy(w.mtime_ns) => {
                    files.insert(w.rel.clone(), e.clone());
                }
                _ => todo.push((w, lang, prev)),
            }
        }
        let fresh = parse_all(&todo);
        let parsed = fresh.iter().filter(|f| f.2).count();
        let changed = !todo.is_empty() || files.len() != old.files.len();
        files.extend(fresh.into_iter().map(|(rel, entry, _)| (rel, entry)));
        let cache = Cache {
            version: CACHE_VERSION,
            files,
        };
        if changed {
            self.save(&cache);
        }
        let snapshot = Snapshot {
            files: cache
                .files
                .iter()
                .map(|(k, e)| (k.clone(), e.tags.clone()))
                .collect(),
            all_files,
            parsed,
        };
        *state = Some(cache);
        snapshot
    }

    /// A missing, corrupt or old-format cache is an empty one.
    fn load(&self) -> Cache {
        let Some(path) = &self.cache_path else {
            return Cache::default();
        };
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
            .filter(|c| c.version == CACHE_VERSION)
            .unwrap_or_default()
    }

    /// Through a temp file and a rename; a failure only costs a re-parse
    /// next session.
    fn save(&self, cache: &Cache) {
        let Some(path) = &self.cache_path else {
            return;
        };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
            std::fs::rename(&tmp, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        };
        if let Err(e) = write() {
            tracing::debug!("codemap: couldn't write the cache {}: {e}", path.display());
        }
    }
}

/// Modified in the last couple of seconds: an edit of the same size in the
/// same mtime tick would look unchanged by stat alone, so the content is
/// re-hashed (git's "racy" rule).
fn racy(mtime_ns: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    now.saturating_sub(mtime_ns) < 2_000_000_000
}

/// Reads, hashes and (if the hash changed) parses, on a few threads;
/// `true` marks a file that was parsed.
fn parse_all(
    todo: &[(&Walked, &'static lang::Lang, Option<&Entry>)],
) -> Vec<(String, Entry, bool)> {
    if todo.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
        .min(todo.len());
    let chunk = todo.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter_map(|t| parse_one(*t))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

fn parse_one(
    (w, lang, prev): (&Walked, &'static lang::Lang, Option<&Entry>),
) -> Option<(String, Entry, bool)> {
    let bytes = std::fs::read(&w.path).ok()?;
    let hash = fnv64(&bytes);
    let (tags, parsed) = match prev {
        Some(e) if e.hash == hash => (e.tags.clone(), false),
        _ => {
            let text = std::str::from_utf8(&bytes).ok()?;
            (tags::extract(lang, text)?, true)
        }
    };
    Some((
        w.rel.clone(),
        Entry {
            size: bytes.len() as u64,
            mtime_ns: w.mtime_ns,
            hash,
            tags,
        },
        parsed,
    ))
}
