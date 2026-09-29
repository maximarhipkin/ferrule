//! M41: `ferrule backup` and `ferrule restore` through the binary: a round
//! trip gives back the same sessions, memory, tasks and ledger; a restore
//! refuses while a gateway runs, and refuses an archive that was changed
//! or cut short, before anything moves.

use ferrule_gateway::{NewTask, TaskKind, TaskStore};
use ferrule_memory::MemoryStore;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn ferrule(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ferrule"))
        .args(args)
        .env("FERRULE_ROOT", root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("FERRULE_CONFIG")
        .env_remove("FERRULE_DATA_DIR")
        .env_remove("FERRULE_INSTANCE")
        .env("NO_COLOR", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn data(root: &Path) -> PathBuf {
    root.join("data").join("ferrule")
}

fn config(root: &Path) -> PathBuf {
    root.join("config").join("ferrule").join("config.toml")
}

const CONFIG: &str = "[gateway]\ntyping = false\n";

/// An instance with a bit of everything: a chat transcript, a memory
/// (left open, so its newest writes sit in the WAL), a scheduled task,
/// the ledger, a config and a secret.
fn populate(root: &Path) -> MemoryStore {
    let data = data(root);
    std::fs::create_dir_all(data.join("sessions")).unwrap();
    std::fs::create_dir_all(data.join("private")).unwrap();
    std::fs::create_dir_all(data.join("models")).unwrap();
    std::fs::write(
        data.join("sessions/telegram_42.jsonl"),
        "{\"role\":\"user\",\"content\":\"hi\"}\n{\"role\":\"assistant\",\"content\":\"hello\"}\n",
    )
    .unwrap();
    std::fs::write(
        data.join("ledger.jsonl"),
        "{\"ts\":1,\"model\":\"mock\",\"cost_usd\":0.01}\n",
    )
    .unwrap();
    std::fs::write(data.join("private/secrets.env"), "OPENAI_API_KEY=sk-test\n").unwrap();
    std::fs::write(data.join("models/big.gguf"), vec![7u8; 4096]).unwrap();
    std::fs::create_dir_all(config(root).parent().unwrap()).unwrap();
    std::fs::write(config(root), CONFIG).unwrap();

    let memory = MemoryStore::open(data.join("memory.db")).unwrap();
    memory
        .remember("Max takes his coffee black", &["prefs"])
        .unwrap();
    memory
        .remember("the release is on Friday", &["work"])
        .unwrap();
    let tasks = TaskStore::open(data.join("tasks.db")).unwrap();
    tasks
        .add(
            NewTask {
                name: "morning".into(),
                kind: TaskKind::Cron,
                schedule: "0 8 * * *".into(),
                timezone: "UTC".into(),
                channel: "telegram".into(),
                chat_id: "42".into(),
                prompt: "the news, briefly".into(),
                gate: None,
                model: None,
            },
            "t1".into(),
            1_700_000_000,
            Some(1_700_030_000),
        )
        .unwrap();
    memory
}

/// Every row of every table, as text, by table.
fn rows(db: &Path) -> BTreeMap<String, Vec<String>> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut out = BTreeMap::new();
    for t in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
        let n = stmt.column_count();
        let mut all: Vec<String> = stmt
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        all.sort();
        out.insert(t, all);
    }
    out
}

/// The entries of a .tar.gz, in order.
fn entries(file: &Path) -> Vec<(String, Vec<u8>)> {
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(file).unwrap());
    let mut tar = tar::Archive::new(gz);
    tar.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

fn write_entries(file: &Path, entries: &[(String, Vec<u8>)]) {
    let gz = flate2::write::GzEncoder::new(
        std::fs::File::create(file).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(gz);
    for (name, bytes) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o600);
        h.set_entry_type(tar::EntryType::Regular);
        // Byte by byte, so a name tar itself would refuse (`..`) goes in.
        let field = &mut h.as_old_mut().name;
        field.fill(0);
        field[..name.len()].copy_from_slice(name.as_bytes());
        h.set_cksum();
        tar.append(&h, bytes.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
}

fn backup(root: &Path, extra: &[&str]) -> PathBuf {
    let out = root.join("b.tar.gz");
    let mut args = vec!["backup", "--out", out.to_str().unwrap()];
    args.extend_from_slice(extra);
    let o = ferrule(root, &args);
    assert!(o.status.success(), "{}", text(&o));
    out
}

/// Every name under a directory other than `pre-restore`/secrets/caches,
/// for "nothing moved".
fn listing(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn go(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(root).unwrap().display().to_string());
            if p.is_dir() {
                go(root, &p, out);
            }
        }
    }
    go(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_round_trip_gives_back_the_same_sessions_memory_tasks_and_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let memory = populate(root);
    let data = data(root);
    let before_memory = rows(&data.join("memory.db"));
    let before_tasks = rows(&data.join("tasks.db"));
    assert!(!before_memory.values().all(Vec::is_empty));

    let out = backup(root, &[]);
    drop(memory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let names: Vec<String> = entries(&out).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names[0], "manifest.json");
    assert!(names.contains(&"data/memory.db".to_string()), "{names:?}");
    assert!(
        names.contains(&"config/config.toml".to_string()),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .all(|n| !n.contains("private") && !n.contains("models")),
        "no secrets, no caches: {names:?}"
    );
    assert!(names.iter().all(|n| !n.ends_with("-wal")), "{names:?}");
    // The doctor now knows when.
    let doctor = text(&ferrule(root, &["doctor", "--offline"]));
    assert!(doctor.contains("the last one was"), "{doctor}");

    // Lose everything but the secret, then put it back.
    std::fs::remove_file(data.join("memory.db")).unwrap();
    let _ = std::fs::remove_file(data.join("memory.db-wal"));
    let _ = std::fs::remove_file(data.join("memory.db-shm"));
    std::fs::remove_file(data.join("tasks.db")).unwrap();
    std::fs::remove_dir_all(data.join("sessions")).unwrap();
    std::fs::write(data.join("ledger.jsonl"), "").unwrap();
    std::fs::write(config(root), "# lost\n").unwrap();

    let dry = ferrule(root, &["restore", "--dry-run", out.to_str().unwrap()]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(text(&dry).contains("Nothing was changed"), "{}", text(&dry));
    assert!(!data.join("memory.db").exists());

    let o = ferrule(root, &["restore", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(rows(&data.join("memory.db")), before_memory);
    assert_eq!(rows(&data.join("tasks.db")), before_tasks);
    assert_eq!(
        std::fs::read_to_string(data.join("sessions/telegram_42.jsonl")).unwrap(),
        "{\"role\":\"user\",\"content\":\"hi\"}\n{\"role\":\"assistant\",\"content\":\"hello\"}\n"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("ledger.jsonl")).unwrap(),
        "{\"ts\":1,\"model\":\"mock\",\"cost_usd\":0.01}\n"
    );
    assert_eq!(std::fs::read_to_string(config(root)).unwrap(), CONFIG);
    // The backup had no secrets, so this machine's stayed.
    assert_eq!(
        std::fs::read_to_string(data.join("private/secrets.env")).unwrap(),
        "OPENAI_API_KEY=sk-test\n"
    );

    // Nothing was deleted: the data and config from before are beside.
    let parent = data.parent().unwrap();
    let aside: Vec<PathBuf> = std::fs::read_dir(parent)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains("ferrule.pre-restore-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(
        std::fs::read_to_string(aside[0].join("ledger.jsonl")).unwrap(),
        ""
    );
    assert!(aside[0].join("models/big.gguf").exists());
    let cfg_dir = config(root).parent().unwrap().to_path_buf();
    assert!(std::fs::read_dir(&cfg_dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("config.toml.pre-restore-")));
    // No staging left behind.
    assert!(std::fs::read_dir(parent).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".restore-")));
}

#[test]
fn secrets_go_in_only_when_asked_and_the_manifest_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    drop(populate(root));
    let out = root.join("s.tar.gz");
    let o = ferrule(
        root,
        &["backup", "--include-secrets", "-o", out.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("warning"), "{}", text(&o));
    let all = entries(&out);
    assert!(all.iter().any(|(n, _)| n == "data/private/secrets.env"));
    let manifest: serde_json::Value = serde_json::from_slice(&all[0].1).unwrap();
    assert_eq!(manifest["secrets"], true);
    assert_eq!(manifest["format"], 1);
    assert_eq!(manifest["instance"], "default");

    // An existing file is never overwritten.
    let again = ferrule(root, &["backup", "-o", out.to_str().unwrap()]);
    assert!(!again.status.success());
    assert!(text(&again).contains("already exists"), "{}", text(&again));
}

#[test]
fn a_restore_refuses_while_a_gateway_runs_on_the_data() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    drop(populate(root));
    let out = backup(root, &[]);
    let data = data(root);
    std::fs::create_dir_all(data.join("gateway")).unwrap();
    std::fs::write(
        data.join("gateway/running.json"),
        serde_json::json!({
            "pid": std::process::id(),
            "version": "0.0.0",
            "started": 0,
            "turns": [],
        })
        .to_string(),
    )
    .unwrap();
    let before = listing(&data);

    let o = ferrule(root, &["restore", out.to_str().unwrap()]);
    assert!(!o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("a gateway is running"), "{t}");
    assert!(t.contains(&std::process::id().to_string()), "{t}");
    assert!(t.contains("Nothing was changed"), "{t}");
    assert_eq!(listing(&data), before);
}

/// A restore of `file` fails with `why`, and the data is as it was.
fn refused(root: &Path, file: &Path, why: &str) {
    let data = data(root);
    let before = listing(&data);
    let o = ferrule(root, &["restore", file.to_str().unwrap()]);
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(t.contains(why), "wanted {why:?}: {t}");
    assert_eq!(listing(&data), before);
    let parent = data.parent().unwrap();
    assert!(
        std::fs::read_dir(parent).unwrap().all(|e| {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            !n.contains("pre-restore") && !n.contains(".restore-")
        }),
        "nothing set aside, no staging left"
    );
}

#[test]
fn a_changed_or_damaged_archive_is_refused_before_anything_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    drop(populate(root));
    let out = backup(root, &[]);
    let good = entries(&out);

    // One byte of the ledger changed, the manifest left alone.
    let mut tampered = good.clone();
    let ledger = tampered
        .iter_mut()
        .find(|(n, _)| n == "data/ledger.jsonl")
        .unwrap();
    ledger.1[5] ^= 1;
    let file = root.join("tampered.tar.gz");
    write_entries(&file, &tampered);
    refused(root, &file, "checksum");

    // A file the manifest doesn't list.
    let mut extra = good.clone();
    extra.push(("data/evil.sh".into(), b"rm -rf ~".to_vec()));
    let file = root.join("extra.tar.gz");
    write_entries(&file, &extra);
    refused(root, &file, "doesn't list");

    // A path that climbs out.
    let mut climbing = good.clone();
    climbing.push(("data/../../escape".into(), b"x".to_vec()));
    let file = root.join("climb.tar.gz");
    write_entries(&file, &climbing);
    refused(root, &file, "never does");
    assert!(!root.join("escape").exists());

    // A file missing.
    let missing: Vec<_> = good
        .iter()
        .filter(|(n, _)| n != "data/tasks.db")
        .cloned()
        .collect();
    let file = root.join("missing.tar.gz");
    write_entries(&file, &missing);
    refused(root, &file, "not in the archive");

    // Cut short, as a copy that didn't finish.
    let bytes = std::fs::read(&out).unwrap();
    let file = root.join("short.tar.gz");
    std::fs::write(&file, &bytes[..bytes.len() / 2]).unwrap();
    refused(root, &file, "damaged");

    // Made by a newer ferrule.
    let mut newer = good.clone();
    let mut manifest: serde_json::Value = serde_json::from_slice(&newer[0].1).unwrap();
    manifest["ferrule_version"] = "99.0.0".into();
    newer[0].1 = serde_json::to_vec(&manifest).unwrap();
    let file = root.join("newer.tar.gz");
    write_entries(&file, &newer);
    refused(root, &file, "ferrule update");

    // The good one still restores.
    let o = ferrule(root, &["restore", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o));
}
