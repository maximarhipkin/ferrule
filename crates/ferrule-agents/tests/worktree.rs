//! Worktrees and the verifier's snapshot, on real git repos.

mod common;

use common::*;
use ferrule_agents::{Limits, Role, SpawnRequest, Spawned, Supervisor};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Semaphore;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one commit, under the rig's dir.
fn repo(rig: &Rig) -> PathBuf {
    let dir = rig.dir.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    // Windows runners convert line endings on checkout by default.
    git(&dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("lib.rs"), "fn a() {}\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    dir
}

fn same(a: &Path, b: &Path) -> bool {
    dunce::canonicalize(a).unwrap() == dunce::canonicalize(b).unwrap()
}

fn spawn_as(sup: &Supervisor, caller: &str, role: Role, worktree: bool) -> Spawned {
    sup.spawn(
        caller,
        SpawnRequest {
            task: "t".into(),
            name: None,
            role,
            worktree,
        },
    )
    .unwrap()
}

fn gated() -> (Rig, Arc<Semaphore>) {
    let gate = Arc::new(Semaphore::new(0));
    let rig = rig(Limits::default(), reporter("done"), Some(gate.clone()));
    rig.sup.set_worktrees_dir(rig.dir.path().join("worktrees"));
    (rig, gate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_children_work_in_their_own_worktrees_and_close_keeps_only_real_work() {
    let (rig, gate) = gated();
    let main = repo(&rig);
    idle_root_in(&rig, "r", &main);
    let a = spawn_as(&rig.sup, "r", Role::Worker, true);
    // The parent's uncommitted change: the next child is told it lacks it.
    std::fs::write(main.join("lib.rs"), "fn a() { todo!() }\n").unwrap();
    let b = spawn_as(&rig.sup, "r", Role::Planner, true);

    for s in [&a, &b] {
        assert!(s.workspace.starts_with(rig.dir.path().join("worktrees")));
        assert_eq!(
            git(&s.workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
            format!("ferrule/{}", s.id)
        );
        assert!(
            s.notes[0].contains(&format!("branch ferrule/{}", s.id)),
            "{:?}",
            s.notes
        );
    }
    assert_ne!(a.workspace, b.workspace);
    assert_eq!(a.notes.len(), 1, "{:?}", a.notes);
    assert!(
        b.notes[1].contains("uncommitted changes aren't in its copy"),
        "{:?}",
        b.notes
    );
    // The spec: its worktree, the repo's git dir writable so it can commit.
    let specs = rig.specs.lock().unwrap().clone();
    assert!(same(&specs[0].workspace, &a.workspace));
    assert_eq!(specs[0].extra_writable.len(), 1);
    assert!(same(&specs[0].extra_writable[0], &main.join(".git")));
    assert!(!specs[0].read_only);

    // A commits one thing and leaves another uncommitted; B does nothing.
    std::fs::write(a.workspace.join("new.rs"), "fn b() {}\n").unwrap();
    git(&a.workspace, &["add", "new.rs"]);
    git(&a.workspace, &["commit", "-q", "-m", "b"]);
    std::fs::write(a.workspace.join("draft.rs"), "fn c() {}\n").unwrap();
    // The parent's checkout doesn't see any of it.
    assert!(!main.join("new.rs").exists());
    assert_eq!(git(&main, &["status", "--porcelain"]), "M lib.rs");

    gate.add_permits(10);
    rig.sup
        .wait("r", std::slice::from_ref(&a.id), Some(10))
        .await
        .unwrap();
    rig.sup
        .wait("r", std::slice::from_ref(&b.id), Some(10))
        .await
        .unwrap();

    let closed = rig.sup.close("r", &a.id).await.unwrap();
    assert!(
        closed.contains(&format!(
            "Kept branch ferrule/{} (2 commits, the last one its uncommitted work).",
            a.id
        )),
        "{closed}"
    );
    assert!(!a.workspace.exists());
    let files = git(
        &main,
        &["ls-tree", "--name-only", &format!("ferrule/{}", a.id)],
    );
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        ["draft.rs", "lib.rs", "new.rs"]
    );

    let closed = rig.sup.close("r", &b.id).await.unwrap();
    assert_eq!(closed, format!("Closed {}.", b.id));
    assert!(!b.workspace.exists());
    let branches = git(&main, &["branch", "--format=%(refname:short)"]);
    assert_eq!(
        branches.lines().collect::<Vec<_>>(),
        [format!("ferrule/{}", a.id).as_str(), "main"]
    );
    assert_eq!(git(&main, &["worktree", "list"]).lines().count(), 1);
    // The parent's own change is untouched.
    assert_eq!(git(&main, &["status", "--porcelain"]), "M lib.rs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verifier_checks_a_snapshot_that_is_thrown_away_when_it_finishes() {
    let (rig, gate) = gated();
    let main = repo(&rig);
    std::fs::write(main.join("lib.rs"), "fn a() { changed() }\n").unwrap();
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/new.rs"), "fn new() {}\n").unwrap();
    idle_root_in(&rig, "r", &main);

    let v = spawn_as(&rig.sup, "r", Role::Verifier, true);
    assert!(v.notes[0].contains("disposable copy"), "{:?}", v.notes);
    let snap = v.workspace.clone();
    // HEAD, plus the uncommitted edit, plus the new file.
    assert_eq!(
        std::fs::read_to_string(snap.join("lib.rs")).unwrap(),
        "fn a() { changed() }\n"
    );
    assert_eq!(
        std::fs::read_to_string(snap.join("src/new.rs")).unwrap(),
        "fn new() {}\n"
    );
    let spec = rig.specs.lock().unwrap()[0].clone();
    assert!(same(&spec.workspace, &snap));
    assert!(!spec.read_only);
    assert!(spec.extra_writable.is_empty());
    // What it does there stays there.
    std::fs::write(snap.join("lib.rs"), "broken").unwrap();
    std::fs::write(snap.join("target.log"), "ran the tests").unwrap();

    gate.add_permits(1);
    rig.sup
        .wait("r", std::slice::from_ref(&v.id), Some(10))
        .await
        .unwrap();
    until("the snapshot is gone", || !snap.exists()).await;
    assert_eq!(
        std::fs::read_to_string(main.join("lib.rs")).unwrap(),
        "fn a() { changed() }\n"
    );
    assert!(!main.join("target.log").exists());
    assert_eq!(git(&main, &["branch", "--format=%(refname:short)"]), "main");

    // Resumed, it gets a fresh snapshot of the parent's work as it is now.
    std::fs::write(main.join("lib.rs"), "fn a() { fixed() }\n").unwrap();
    rig.sup.resume("r", &v.id, "check again").unwrap();
    assert_eq!(
        std::fs::read_to_string(snap.join("lib.rs")).unwrap(),
        "fn a() { fixed() }\n"
    );
    rig.sup.close("r", &v.id).await.unwrap();
    assert!(!snap.exists());
    assert_eq!(git(&main, &["worktree", "list"]).lines().count(), 1);
}

#[tokio::test]
async fn without_a_repo_of_its_own_a_child_shares_the_parent_workspace() {
    let (rig, _gate) = gated();
    let main = repo(&rig);
    let plain = rig.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    idle_root_in(&rig, "in-repo", &main);
    idle_root_in(&rig, "no-repo", &plain);
    idle_root_in(&rig, "subdir", &main.join("sub"));
    std::fs::create_dir_all(main.join("sub")).unwrap();

    // Not a repo: nothing to say.
    let s = spawn_as(&rig.sup, "no-repo", Role::Worker, true);
    assert_eq!((s.workspace.as_path(), s.notes.len()), (plain.as_path(), 0));
    // Asked not to.
    let s = spawn_as(&rig.sup, "in-repo", Role::Worker, false);
    assert_eq!((s.workspace.as_path(), s.notes.len()), (main.as_path(), 0));
    // A verifier with no snapshot works read-only in the parent's files.
    spawn_as(&rig.sup, "no-repo", Role::Verifier, true);
    let spec = rig.specs.lock().unwrap().last().unwrap().clone();
    assert!(spec.read_only);
    assert!(same(&spec.workspace, &plain));
    // The repo's git dir is outside the parent's workspace: a writable git
    // dir would widen what the child may touch, so it shares.
    let s = spawn_as(&rig.sup, "subdir", Role::Worker, true);
    assert_eq!(s.workspace, main.join("sub"));
    assert!(
        s.notes[0].contains("git directory is outside your workspace"),
        "{:?}",
        s.notes
    );
    assert_eq!(git(&main, &["worktree", "list"]).lines().count(), 1);
}
