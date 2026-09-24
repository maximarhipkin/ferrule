//! The fake value a sandboxed command sees instead of a real secret.
//!
//! Only a known token prefix (`ghp_`, `sk-ant-`, …) is copied from the real
//! value, so a tool that checks the token's shape still accepts it; the rest
//! is hex derived from a local seed and the variable name, so it says nothing
//! about the secret and stays the same across restarts and token rotations.

use ring::digest;
use std::fmt::Write as _;

/// Longest match wins, so `sk-ant-` beats `sk-`.
const KNOWN_PREFIXES: &[&str] = &[
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "glpat-",
    "sk-ant-",
    "sk-proj-",
    "sk-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "hf_",
    "sk_live_",
    "sk_test_",
    "rk_live_",
];

/// Never fewer random characters than this, even for a short secret (which
/// then gets a longer placeholder).
const MIN_RANDOM: usize = 16;

/// Same length as `real` whenever that leaves at least [`MIN_RANDOM`] random
/// characters, so `Content-Length` survives the swap.
pub fn placeholder(seed: &[u8], name: &str, real: &str) -> String {
    let prefix = KNOWN_PREFIXES
        .iter()
        .filter(|p| real.starts_with(**p))
        .max_by_key(|p| p.len())
        .copied()
        .filter(|p| real.len() - p.len() >= MIN_RANDOM)
        .unwrap_or("");
    let total = prefix.len() + (real.len() - prefix.len()).max(MIN_RANDOM);
    let mut out = String::with_capacity(total + 64);
    out.push_str(prefix);
    let mut counter = 0u32;
    while out.len() < total {
        let mut ctx = digest::Context::new(&digest::SHA256);
        ctx.update(seed);
        ctx.update(name.as_bytes());
        ctx.update(&counter.to_be_bytes());
        for b in ctx.finish().as_ref() {
            let _ = write!(out, "{b:02x}");
        }
        counter += 1;
    }
    out.truncate(total);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &[u8] = &[7; 32];

    #[test]
    fn keeps_a_known_prefix_and_the_length() {
        let real = "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let p = placeholder(SEED, "GITHUB_TOKEN", real);
        assert_eq!(p.len(), real.len());
        assert!(p.starts_with("ghp_"));
        assert!(p[4..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!p.contains("AbCd"));

        let ant = placeholder(
            SEED,
            "ANTHROPIC_API_KEY",
            "sk-ant-api03-0123456789abcdef0123",
        );
        assert!(ant.starts_with("sk-ant-") && !ant.starts_with("sk-ant-api03"));

        let long = "x".repeat(200);
        assert_eq!(
            placeholder(SEED, "LONG", &long).len(),
            200,
            "extends past one digest"
        );
    }

    #[test]
    fn unknown_or_short_values_get_no_prefix_and_at_least_16_chars() {
        let p = placeholder(SEED, "DB_PASS", "hunter2");
        assert_eq!(p.len(), 16);
        assert!(p.bytes().all(|b| b.is_ascii_hexdigit()));
        let short_sk = placeholder(SEED, "K", "sk-12345");
        assert!(
            !short_sk.starts_with("sk-"),
            "prefix only when 16 random chars remain"
        );
    }

    #[test]
    fn stable_per_seed_and_name_and_independent_of_the_value() {
        let a = placeholder(SEED, "TOKEN", "ghp_1111111111111111111111");
        assert_eq!(a, placeholder(SEED, "TOKEN", "ghp_1111111111111111111111"));
        assert_eq!(
            a,
            placeholder(SEED, "TOKEN", "ghp_2222222222222222222222"),
            "rotation keeps it"
        );
        assert_ne!(a, placeholder(SEED, "OTHER", "ghp_1111111111111111111111"));
        assert_ne!(
            a,
            placeholder(&[8; 32], "TOKEN", "ghp_1111111111111111111111")
        );
    }
}
