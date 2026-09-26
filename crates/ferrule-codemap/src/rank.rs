//! Aider-style ranking: a graph of files joined by the identifiers one
//! references and another defines, PageRank personalised towards what the
//! conversation mentions, and the definitions ranked by the rank flowing
//! into them. Then the map: as many top definitions as fit the budget.

use crate::repo::Snapshot;
use crate::tags::Def;
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub const HEADER: &str = "[Repo map: ranked outline of this repository — definitions with \
line numbers; replaces any earlier map. Use code_search/read_file for detail.]";
const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 30;

/// What the conversation mentions: identifiers (any word) and text in
/// which file paths are looked for.
pub struct Mentions {
    words: HashSet<String>,
    text: String,
}

impl Mentions {
    pub fn new(text: &str) -> Self {
        let words = text
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        Self {
            words,
            text: text.to_string(),
        }
    }

    pub fn none() -> Self {
        Self::new("")
    }

    fn ident(&self, name: &str) -> bool {
        self.words.contains(name)
    }
}

/// Files the text names: the relative path, or a file name no other file
/// has.
fn mentioned_files(snapshot: &Snapshot, mentions: &Mentions) -> BTreeSet<String> {
    if mentions.text.is_empty() {
        return BTreeSet::new();
    }
    let mut by_name: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for rel in snapshot.files.keys() {
        by_name
            .entry(rel.rsplit('/').next().unwrap_or(rel))
            .or_default()
            .push(rel);
    }
    let mut out = BTreeSet::new();
    for rel in snapshot.files.keys() {
        if mentions.text.contains(rel.as_str()) {
            out.insert(rel.clone());
        }
    }
    for (name, rels) in by_name {
        if rels.len() == 1 && mentions.words_contain_file(name) {
            out.insert(rels[0].clone());
        }
    }
    out
}

impl Mentions {
    /// `name` appears with non-path characters (or nothing) around it.
    fn words_contain_file(&self, name: &str) -> bool {
        let bytes = self.text.as_bytes();
        let edge = |b: Option<&u8>| {
            b.is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' || *c == b'.'))
        };
        self.text.match_indices(name).any(|(at, _)| {
            let before = at.checked_sub(1).and_then(|i| bytes.get(i));
            let after = bytes.get(at + name.len());
            // `/` before is fine: `src/lib.rs` names `lib.rs`.
            (before == Some(&b'/') || edge(before)) && edge(after)
        })
    }
}

fn long_name(name: &str) -> bool {
    name.chars().count() >= 8
        && (name.contains('_')
            || name.chars().any(|c| c.is_uppercase()) && name.chars().any(|c| c.is_lowercase()))
}

/// A definition and its score.
#[derive(Clone, Debug)]
pub struct Ranked<'a> {
    pub path: &'a str,
    pub def: &'a Def,
    pub score: f64,
}

/// File ranks and definitions, best first. Equal input gives equal output.
pub fn rank<'a>(
    snapshot: &'a Snapshot,
    mentions: &Mentions,
) -> (BTreeMap<&'a str, f64>, Vec<Ranked<'a>>) {
    let files: Vec<&str> = snapshot.files.keys().map(String::as_str).collect();
    let index: BTreeMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (*f, i)).collect();
    let n = files.len();
    if n == 0 {
        return (BTreeMap::new(), Vec::new());
    }

    let mut defines: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        for d in &snapshot.files[*f].defs {
            defines.entry(d.name.as_str()).or_default().insert(i);
        }
    }
    // (referencer, name) → count
    let mut refs: BTreeMap<(usize, &str), usize> = BTreeMap::new();
    for (i, f) in files.iter().enumerate() {
        for r in &snapshot.files[*f].refs {
            *refs.entry((i, r.name.as_str())).or_default() += 1;
        }
    }

    // edges[a] = (b, name, weight)
    let mut edges: Vec<Vec<(usize, &str, f64)>> = vec![Vec::new(); n];
    for (&(a, name), &count) in &refs {
        let Some(definers) = defines.get(name) else {
            continue;
        };
        let mut m = 1.0;
        if mentions.ident(name) {
            m *= 10.0;
        }
        if long_name(name) {
            m *= 10.0;
        }
        if name.starts_with('_') {
            m *= 0.1;
        }
        if definers.len() > 5 {
            m *= 0.1;
        }
        let w = (count as f64).sqrt() * m;
        for &b in definers {
            if b != a {
                edges[a].push((b, name, w));
            }
        }
    }
    let out_weight: Vec<f64> = edges.iter().map(|e| e.iter().map(|x| x.2).sum()).collect();

    let named = mentioned_files(snapshot, mentions);
    let mut personal = vec![1.0; n];
    for f in &named {
        personal[index[f.as_str()]] = 100.0;
    }
    let total: f64 = personal.iter().sum();
    personal.iter_mut().for_each(|p| *p /= total);

    let mut r = personal.clone();
    for _ in 0..ITERATIONS {
        let dangling: f64 = (0..n).filter(|&i| out_weight[i] == 0.0).map(|i| r[i]).sum();
        let mut next: Vec<f64> = personal
            .iter()
            .map(|p| (1.0 - DAMPING) * p + DAMPING * dangling * p)
            .collect();
        for a in 0..n {
            if out_weight[a] == 0.0 {
                continue;
            }
            for &(b, _, w) in &edges[a] {
                next[b] += DAMPING * r[a] * w / out_weight[a];
            }
        }
        r = next;
    }

    // Rank flowing along each edge lands on the definitions of its name
    // in the target file; every definition keeps a sliver of its file's
    // rank so unreferenced ones still order by file.
    let mut def_score: BTreeMap<(usize, &str), f64> = BTreeMap::new();
    for a in 0..n {
        for &(b, name, w) in &edges[a] {
            *def_score.entry((b, name)).or_default() += r[a] * w / out_weight[a];
        }
    }
    let mut ranked = Vec::new();
    for (i, f) in files.iter().enumerate() {
        for d in &snapshot.files[*f].defs {
            let flow = def_score.get(&(i, d.name.as_str())).copied().unwrap_or(0.0);
            ranked.push(Ranked {
                path: f,
                def: d,
                score: flow + r[i] * 1e-3,
            });
        }
    }
    ranked.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then_with(|| x.path.cmp(y.path))
            .then_with(|| x.def.line.cmp(&y.def.line))
    });
    let file_rank = files.iter().enumerate().map(|(i, f)| (*f, r[i])).collect();
    (file_rank, ranked)
}

/// The map from the top `k` definitions: files in rank order, definitions
/// in line order.
fn render(file_rank: &BTreeMap<&str, f64>, top: &[Ranked]) -> String {
    let mut by_file: BTreeMap<&str, Vec<&Def>> = BTreeMap::new();
    for r in top {
        by_file.entry(r.path).or_default().push(r.def);
    }
    let mut order: Vec<&str> = by_file.keys().copied().collect();
    order.sort_by(|a, b| file_rank[b].total_cmp(&file_rank[a]).then_with(|| a.cmp(b)));
    let mut out = String::from(HEADER);
    for f in order {
        out.push('\n');
        out.push_str(f);
        let defs = by_file.get_mut(f).expect("listed");
        defs.sort_by_key(|d| d.line);
        for d in defs.iter() {
            out.push_str(&format!("\n{:>6}│{}", d.line, d.text));
        }
    }
    out
}

/// Tokens as the budget counts them: chars / 4, rounded up.
pub fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The repo map within `budget` tokens, or `None` if no definition fits
/// (or there are none, or the budget is 0).
pub fn repo_map(snapshot: &Snapshot, mentions: &Mentions, budget: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let (file_rank, ranked) = rank(snapshot, mentions);
    // The most top-ranked definitions that fit: binary search on k.
    let (mut lo, mut hi) = (0usize, ranked.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if tokens(&render(&file_rank, &ranked[..mid])) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    (lo > 0).then(|| render(&file_rank, &ranked[..lo]))
}
