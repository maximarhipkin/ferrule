//! A line-wise unified diff (LCS), enough for a playbook of a few hundred
//! lines; no dependency for it.

const CONTEXT: usize = 3;
/// Past this many line pairs the LCS table is not built: the diff shows the
/// whole file replaced.
const MAX_CELLS: usize = 4_000_000;

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Same,
    Del,
    Add,
}

fn ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, &'a str)> {
    let (n, m) = (a.len(), b.len());
    if n.saturating_mul(m) > MAX_CELLS {
        let mut out: Vec<_> = a.iter().map(|l| (Op::Del, *l)).collect();
        out.extend(b.iter().map(|l| (Op::Add, *l)));
        return out;
    }
    // lcs[i][j]: the LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n + m);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push((Op::Same, a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // Deletions first, as `diff -u` prints them.
            out.push((Op::Del, a[i]));
            i += 1;
        } else {
            out.push((Op::Add, b[j]));
            j += 1;
        }
    }
    out
}

/// `diff -u`-style text; empty when the two are equal.
pub fn unified(before: &str, after: &str, from: &str, to: &str) -> String {
    if before == after {
        return String::new();
    }
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let ops = ops(&a, &b);
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Same)
        .map(|(k, _)| k)
        .collect();
    let mut out = format!("--- {from}\n+++ {to}\n");
    if changed.is_empty() {
        // Only the final newline differs.
        out.push_str("@@ newline at end of file changed @@\n");
        return out;
    }
    // Hunks: runs of changes whose context windows touch.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &k in &changed {
        let lo = k.saturating_sub(CONTEXT);
        let hi = (k + CONTEXT + 1).min(ops.len());
        match hunks.last_mut() {
            Some((_, end)) if lo <= *end => *end = hi,
            _ => hunks.push((lo, hi)),
        }
    }
    // Line numbers at every op index.
    let mut old_no = vec![0usize; ops.len() + 1];
    let mut new_no = vec![0usize; ops.len() + 1];
    for (k, (op, _)) in ops.iter().enumerate() {
        old_no[k + 1] = old_no[k] + usize::from(*op != Op::Add);
        new_no[k + 1] = new_no[k] + usize::from(*op != Op::Del);
    }
    for (lo, hi) in hunks {
        let old_len = old_no[hi] - old_no[lo];
        let new_len = new_no[hi] - new_no[lo];
        let start = |no: usize, len: usize| if len == 0 { no } else { no + 1 };
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            start(old_no[lo], old_len),
            old_len,
            start(new_no[lo], new_len),
            new_len
        ));
        for (op, line) in &ops[lo..hi] {
            let mark = match op {
                Op::Same => ' ',
                Op::Del => '-',
                Op::Add => '+',
            };
            out.push(mark);
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("a\nb\n", "a\nb\n", "x", "y"), "");
    }

    #[test]
    fn one_added_line() {
        let before = "# t\n\n- [pb-1] one\n- [pb-2] two\n";
        let after = "# t\n\n- [pb-1] one\n- [pb-2] two\n- [pb-3] three\n";
        assert_eq!(
            unified(before, after, "before", "after"),
            "--- before\n+++ after\n@@ -2,3 +2,4 @@\n \n - [pb-1] one\n - [pb-2] two\n+- [pb-3] three\n"
        );
    }

    #[test]
    fn far_apart_changes_are_separate_hunks() {
        let before: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let after = before.replace("l2\n", "L2\n").replace("l18\n", "");
        let d = unified(&before, &after, "a", "b");
        assert_eq!(d.matches("@@ -").count(), 2, "{d}");
        assert!(d.contains("-l2\n+L2\n"), "{d}");
        assert!(
            d.contains("@@ -15,6 +15,5 @@\n l15\n l16\n l17\n-l18\n l19\n l20\n"),
            "{d}"
        );
    }

    #[test]
    fn from_empty() {
        assert_eq!(
            unified("", "- a\n", "a", "b"),
            "--- a\n+++ b\n@@ -0,0 +1,1 @@\n+- a\n"
        );
    }
}
