//! M29 repo map and code_search, against small projects in five languages
//! (`tests/fixtures/<lang>`). The golden maps are in `tests/golden/`;
//! `UPDATE_GOLDEN=1 cargo test -p ferrule-codemap` rewrites them.

use ferrule_codemap::search::{search, SearchKind};
use ferrule_codemap::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const LANGS: [&str; 5] = ["rust", "python", "ts", "go", "java"];

fn fixture(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(lang)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// A fixture copied into a temp dir, so tests can change it.
fn scratch(lang: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture(lang), dir.path());
    dir
}

fn map_of(root: &Path, mentions: &str, budget: usize) -> Option<String> {
    let snapshot = CodeMap::new(root, vec![], None).refresh();
    repo_map(&snapshot, &Mentions::new(mentions), budget)
}

#[test]
fn golden_map_per_language() {
    for lang in LANGS {
        let map = map_of(&fixture(lang), "", DEFAULT_MAP_TOKENS).expect(lang);
        let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(format!("{lang}.map"));
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
            std::fs::write(&golden, &map).unwrap();
        }
        let want = std::fs::read_to_string(&golden)
            .unwrap_or_else(|_| panic!("no {}", golden.display()))
            .replace("\r\n", "\n");
        assert_eq!(map, want, "{lang}: the map changed");
    }
}

/// Locals (a function inside a function body, a const in a Go func) never
/// reach the map; methods are marked as methods.
#[test]
fn only_top_level_definitions_are_extracted() {
    for lang in LANGS {
        let snapshot = CodeMap::new(&fixture(lang), vec![], None).refresh();
        let names: Vec<(String, &str)> = snapshot
            .files
            .values()
            .flat_map(|t| t.defs.iter().map(|d| (d.name.clone(), d.kind.as_str())))
            .collect();
        for local in [
            "local_helper",
            "localHelper",
            "localConst",
            "lowercase_global",
        ] {
            assert!(
                !names.iter().any(|(n, _)| n == local),
                "{lang}: {local} leaked"
            );
        }
        let method = ["add_item", "addItem", "AddItem"];
        assert!(
            names
                .iter()
                .any(|(n, k)| method.contains(&n.as_str()) && *k == "method"),
            "{lang}: no add-item method in {names:?}"
        );
    }
}

#[test]
fn equal_input_gives_the_same_map_byte_for_byte() {
    for lang in LANGS {
        let a = map_of(&fixture(lang), "the checkout total", 300);
        let b = map_of(&fixture(lang), "the checkout total", 300);
        let copy = scratch(lang);
        let c = map_of(copy.path(), "the checkout total", 300);
        assert_eq!(a, b, "{lang}");
        assert_eq!(a, c, "{lang}: a copy elsewhere ranks the same");
    }
}

/// A file the conversation names, and identifiers it mentions, move up.
#[test]
fn mentions_move_files_up() {
    let root = fixture("rust");
    let plain = map_of(&root, "", 1024).unwrap();
    let first_file = |map: &str| map.lines().nth(1).unwrap().to_string();
    assert_eq!(first_file(&plain), "src/price.rs", "{plain}");
    let named = map_of(&root, "please look at src/lib.rs", 1024).unwrap();
    assert_eq!(first_file(&named), "src/lib.rs", "{named}");
    // A unique file name alone is enough.
    let by_name = map_of(&root, "what's in cart.rs?", 1024).unwrap();
    assert_eq!(first_file(&by_name), "src/cart.rs", "{by_name}");
}

#[test]
fn the_budget_trims_to_the_top_definitions() {
    let root = fixture("go");
    let full = map_of(&root, "", 10_000).unwrap();
    for budget in [60, 80, 120] {
        let map = map_of(&root, "", budget).unwrap();
        assert!(map.chars().count().div_ceil(4) <= budget, "{budget}: {map}");
        assert!(map.len() < full.len());
        // Every line kept is in the full map.
        for line in map.lines() {
            assert!(full.lines().any(|l| l == line), "{line}");
        }
    }
    // The tightest budget keeps the most-referenced definition.
    let tight = map_of(&root, "", 60).unwrap();
    assert!(
        tight.ends_with("func NewMoney(cents int64) Money {"),
        "{tight}"
    );
    assert_eq!(map_of(&root, "", 0), None, "0 turns it off");
    assert_eq!(map_of(&root, "", 10), None, "not even the header fits");
}

#[test]
fn the_cache_reuses_unchanged_files_and_reparses_changed_ones() {
    let ws = scratch("rust");
    let data = tempfile::tempdir().unwrap();
    let map = CodeMap::new(ws.path(), vec![], Some(data.path()));
    let first = map.refresh();
    assert_eq!(first.parsed, 3);
    assert!(map.cache_path().unwrap().exists());
    assert_eq!(map.refresh().parsed, 0, "nothing changed");

    // A new session (a new CodeMap) reads the cache from disk.
    let again = CodeMap::new(ws.path(), vec![], Some(data.path()));
    let snap = again.refresh();
    assert_eq!(snap.parsed, 0);
    assert_eq!(snap.files, first.files);

    // An edit re-parses that file only, and its new definition shows.
    let price = ws.path().join("src/price.rs");
    let mut text = std::fs::read_to_string(&price).unwrap();
    text.push_str("\npub fn round_to_shekel(m: Money) -> Money { m }\n");
    std::fs::write(&price, text).unwrap();
    let snap = again.refresh();
    assert_eq!(snap.parsed, 1);
    assert!(snap.files["src/price.rs"]
        .defs
        .iter()
        .any(|d| d.name == "round_to_shekel"));

    // Deleting a file drops it; a new one is picked up.
    std::fs::remove_file(ws.path().join("src/lib.rs")).unwrap();
    std::fs::write(ws.path().join("src/tax.rs"), "pub fn vat() {}\n").unwrap();
    let snap = again.refresh();
    assert!(!snap.files.contains_key("src/lib.rs"));
    assert!(snap.files.contains_key("src/tax.rs"));
    assert_eq!(snap.parsed, 1);

    // A corrupt cache is discarded, never trusted.
    std::fs::write(map.cache_path().unwrap(), b"{not json").unwrap();
    let fresh = CodeMap::new(ws.path(), vec![], Some(data.path()));
    let snap = fresh.refresh();
    assert_eq!(snap.parsed, 3);
    assert!(snap.files.contains_key("src/tax.rs"));
}

/// A same-size edit right after the last refresh (same mtime tick on a
/// coarse filesystem) is still seen: recent files are re-hashed.
#[test]
fn a_same_size_edit_is_not_missed() {
    let ws = scratch("python");
    let map = CodeMap::new(ws.path(), vec![], None);
    map.refresh();
    let price = ws.path().join("shop/price.py");
    let text = std::fs::read_to_string(&price).unwrap();
    let meta = std::fs::metadata(&price).unwrap();
    std::fs::write(&price, text.replace("apply_discount", "apply_rebates_")).unwrap();
    // Put the old mtime back: stat alone can't tell.
    let f = std::fs::File::options().write(true).open(&price).unwrap();
    f.set_modified(meta.modified().unwrap()).unwrap();
    drop(f);
    let snap = map.refresh();
    assert!(snap.files["shop/price.py"]
        .defs
        .iter()
        .any(|d| d.name == "apply_rebates_"));
}

#[test]
fn code_repo_detection() {
    for lang in LANGS {
        assert!(looks_like_code_repo(&fixture(lang), &[]), "{lang}");
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    for f in ["a.py", "b.py", "c.py"] {
        std::fs::write(p.join(f), "x = 1\n").unwrap();
    }
    assert!(!looks_like_code_repo(p, &[]), "no VCS dir and no manifest");
    std::fs::write(p.join("setup.py"), "").unwrap();
    assert!(looks_like_code_repo(p, &[]), "setup.py plus 4 sources");
    std::fs::remove_file(p.join("a.py")).unwrap();
    std::fs::remove_file(p.join("b.py")).unwrap();
    assert!(!looks_like_code_repo(p, &[]), "2 sources are too few");

    let docs = tempfile::tempdir().unwrap();
    std::fs::create_dir(docs.path().join(".git")).unwrap();
    for f in ["a.csv", "b.md", "c.txt", "d.json"] {
        std::fs::write(docs.path().join(f), "").unwrap();
    }
    assert!(
        !looks_like_code_repo(docs.path(), &[]),
        "a repo of data files"
    );
}

/// `.gitignore` is honoured without a `.git` dir (and git is never run);
/// hidden dirs and the deny list are skipped.
#[test]
fn the_walk_skips_ignored_hidden_and_denied_paths() {
    let ws = scratch("rust");
    let p = ws.path();
    std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
    for dir in ["target", ".cache", "secrets"] {
        std::fs::create_dir_all(p.join(dir)).unwrap();
        std::fs::write(p.join(dir).join("gen.rs"), "pub fn generated() {}\n").unwrap();
    }
    let snap = CodeMap::new(p, vec![p.join("secrets")], None).refresh();
    let files: Vec<&String> = snap.files.keys().collect();
    assert_eq!(files, ["src/cart.rs", "src/lib.rs", "src/price.rs"]);
    assert!(!snap.all_files.iter().any(|f| f.contains("gen.rs")));
}

fn run(lang: &str, query: &str, kind: SearchKind) -> String {
    let root = fixture(lang);
    let map = CodeMap::new(&root, vec![], None);
    let snap = map.refresh();
    search(map.root(), &snap, query, kind, "", 50)
}

#[test]
fn code_search_finds_definitions_per_language() {
    let cases = [
        (
            "rust",
            "apply_discount",
            "src/price.rs:12  def function apply_discount",
        ),
        ("python", "Money", "shop/price.py:8  def class Money"),
        (
            "ts",
            "applyDiscount",
            "src/price.ts:14  def function applyDiscount",
        ),
        ("go", "NewMoney", "shop/price.go:11  def function NewMoney"),
        (
            "java",
            "Checkout",
            "src/shop/Checkout.java:3  def interface Checkout",
        ),
    ];
    for (lang, query, want) in cases {
        let out = run(lang, query, SearchKind::Definitions);
        assert!(out.contains(want), "{lang}: {out}");
        assert!(!out.contains(" ref "), "{lang}: definitions only: {out}");
    }
}

#[test]
fn code_search_finds_references_per_language() {
    let cases = [
        (
            "rust",
            "apply_discount",
            &[
                "src/cart.rs:1  ref",
                "src/cart.rs:29  ref — apply_discount(Money::new(sum), 10)",
            ][..],
        ),
        (
            "python",
            "apply_discount",
            &["shop/cart.py:16  ref — return apply_discount(Money(total), 10)"][..],
        ),
        (
            "ts",
            "Money",
            &["src/cart.ts:18  ref", "src/price.ts:14  ref"][..],
        ),
        (
            "go",
            "NewMoney",
            &[
                "shop/cart.go:20  ref",
                "shop/main_test.go:7  ref",
                "shop/price.go:17  ref",
            ][..],
        ),
        ("java", "applyDiscount", &["src/shop/Cart.java:19  ref"][..]),
    ];
    for (lang, query, wants) in cases {
        let out = run(lang, query, SearchKind::References);
        for want in wants {
            assert!(out.contains(want), "{lang}: {want} in {out}");
        }
        assert!(!out.contains(" def "), "{lang}: {out}");
    }
}

#[test]
fn code_search_all_lists_definitions_first_with_a_count() {
    let out = run("go", "Money", SearchKind::All);
    let mut lines = out.lines();
    let head = lines.next().unwrap();
    assert!(head.contains("1 definitions"), "{head}");
    assert!(lines.next().unwrap().contains("def struct Money"), "{out}");
    // A qualified query searches its last segment.
    let q = run("rust", "Money::new", SearchKind::Definitions);
    assert!(q.contains("src/price.rs:7  def method new"), "{q}");
}

#[test]
fn code_search_falls_back_to_text_for_other_files() {
    let ws = scratch("rust");
    std::fs::write(
        ws.path().join("NOTES.md"),
        "Money is stored in cents.\nMoneyBag is not money.\n",
    )
    .unwrap();
    let map = CodeMap::new(ws.path(), vec![], None);
    let snap = map.refresh();
    let out = search(map.root(), &snap, "Money", SearchKind::All, "", 50);
    assert!(
        out.contains("NOTES.md:1  text — Money is stored in cents."),
        "{out}"
    );
    assert!(!out.contains("NOTES.md:2"), "whole words only: {out}");
    // A non-identifier query is a substring search, everywhere.
    let out = search(map.root(), &snap, "in cents", SearchKind::All, "", 50);
    assert!(out.starts_with("1 results"), "{out}");
    // Scoped to a dir.
    let out = search(
        map.root(),
        &snap,
        "Money",
        SearchKind::All,
        "src/cart.rs",
        50,
    );
    assert!(
        !out.contains("NOTES.md") && !out.contains("price.rs"),
        "{out}"
    );
    let none = search(map.root(), &snap, "Nowhere", SearchKind::All, "", 50);
    assert!(none.starts_with("no results for `Nowhere`"), "{none}");
}

#[test]
fn code_search_caps_results() {
    let out = run("go", "Money", SearchKind::All);
    let total: usize = out.split_whitespace().next().unwrap().parse().unwrap();
    assert!(total > 3);
    let root = fixture("go");
    let map = CodeMap::new(&root, vec![], None);
    let snap = map.refresh();
    let capped = search(map.root(), &snap, "Money", SearchKind::All, "", 3);
    assert_eq!(capped.lines().count(), 4, "{capped}");
    assert!(capped
        .lines()
        .next()
        .unwrap()
        .contains("showing the first 3"));
}

#[tokio::test]
async fn the_tool_is_read_only_and_stays_in_the_workspace() {
    use ferrule_core::tool::{Tool, ToolContext};
    let root = fixture("rust");
    let tool = CodeSearchTool::new(Arc::new(CodeMap::new(&root, vec![], None)));
    assert!(tool.read_only());
    assert!(!tool.changes_files());
    let ctx = ToolContext {
        workspace: root.clone(),
        max_output_chars: 10_000,
    };
    let out = tool
        .call(
            serde_json::json!({"query": "Cart", "kind": "definitions"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        out.content.contains("src/cart.rs:5  def struct Cart"),
        "{}",
        out.content
    );
    for path in ["../", "/etc", "src/../../x"] {
        let err = tool
            .call(serde_json::json!({"query": "Cart", "path": path}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("relative to the workspace"),
            "{err}"
        );
    }
    let err = tool
        .call(serde_json::json!({"query": "Cart", "kind": "types"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown kind"), "{err}");
}

/// Earlier maps and tool results don't count as mentions.
#[test]
fn mentions_come_from_the_conversation_not_from_maps_or_tool_output() {
    use ferrule_core::Message;
    let map = map_of(&fixture("rust"), "", 1024).unwrap();
    let history = vec![
        Message::user("fix the total"),
        Message::user(map),
        Message::tool_result("1", "cart.rs lib.rs price.rs"),
        Message::assistant(Some("looking at cart.rs".into()), vec![], None),
    ];
    let text = conversation_text("now the discount", &history);
    assert!(text.contains("fix the total") && text.contains("looking at cart.rs"));
    assert!(text.contains("now the discount"));
    assert!(
        !text.contains("Repo map") && !text.contains("price.rs"),
        "{text}"
    );
}

mod agent_turns {
    use super::*;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse, Provider};
    use ferrule_core::{
        Agent, AgentConfig, CoreError, HarnessProfile, Message, ToolContext, ToolRegistry, Usage,
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Provider for Recording {
        fn name(&self) -> &str {
            "recording"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::to_string(&req.messages).unwrap());
            Ok(CompletionResponse {
                message: Message::assistant(Some("ok".into()), vec![], None),
                usage: Usage::default(),
            })
        }
    }

    /// M27's property with the real map: a turn in which no file changed
    /// and nothing new was mentioned sends the previous request's bytes
    /// unchanged as its prefix; an edit appends a new map.
    #[tokio::test]
    async fn the_request_prefix_is_byte_stable_across_a_no_change_turn() {
        let ws = scratch("rust");
        let data = tempfile::tempdir().unwrap();
        let provider = Arc::new(Recording::default());
        let map = Arc::new(CodeMap::new(ws.path(), vec![], Some(data.path())));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext {
                workspace: ws.path().to_path_buf(),
                max_output_chars: 10_000,
            },
            None,
        )
        .with_turn_context(Arc::new(RepoMapContext::new(map, DEFAULT_MAP_TOKENS)));
        let (tx, _rx) = tokio::sync::mpsc::channel(1024);
        agent.run("fix it", tx.clone()).await.unwrap();
        agent.run("fix it", tx.clone()).await.unwrap();
        let seen = provider.0.lock().unwrap().clone();
        let (a, b) = (&seen[0], &seen[1]);
        assert!(a.contains("Repo map"), "{a}");
        let prefix = &a[..a.len() - 1]; // drop the closing `]`
        assert!(b.starts_with(prefix), "the first request is a prefix");
        assert_eq!(b.matches("[Repo map").count(), 1, "no second map: {b}");

        let price = ws.path().join("src/price.rs");
        let mut text = std::fs::read_to_string(&price).unwrap();
        text.push_str("\npub fn round_to_shekel(m: Money) -> Money { m }\n");
        std::fs::write(&price, text).unwrap();
        agent.run("fix it", tx).await.unwrap();
        let c = provider.0.lock().unwrap()[2].clone();
        assert!(c.starts_with(&b[..b.len() - 1]));
        assert_eq!(c.matches("[Repo map").count(), 2);
        assert!(c.contains("round_to_shekel"));
    }
}
