//! Network through ferrule's proxy: one GET to api.github.com. With the
//! owner's `GITHUB_TOKEN` bound in ferrule's secrets, the request carries
//! its placeholder and the proxy swaps the real token in on the way out;
//! the plugin never sees it. Without it, the request goes unauthenticated.

use ferrule_plugin_sdk::host::{self, Request};
use ferrule_plugin_sdk::{export, json, Value};

fn call(tool: &str, args: Value) -> Result<Value, String> {
    match tool {
        "repo" => repo(
            args["owner"].as_str().ok_or("`owner` is required")?,
            args["repo"].as_str().ok_or("`repo` is required")?,
        ),
        other => Err(format!("no tool `{other}`")),
    }
}

export!(call);

fn repo(owner: &str, name: &str) -> Result<Value, String> {
    for part in [owner, name] {
        let ok = !part.is_empty()
            && part.len() <= 100
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && part != "."
            && part != "..";
        if !ok {
            return Err(format!("`{part}` isn't a GitHub owner or repository name"));
        }
    }
    let url = format!("https://api.github.com/repos/{owner}/{name}");
    let base = Request::get(&url)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28");
    let resp = match host::http(&base.clone().header("authorization", "Bearer ${GITHUB_TOKEN}")) {
        // No token set up: ask anyway, unauthenticated (60 requests/hour).
        Err(e) if e.contains("GITHUB_TOKEN") => host::http(&base)?,
        other => other?,
    };
    if resp.status != 200 {
        return Err(format!("GitHub answered {} for {owner}/{name}", resp.status));
    }
    let r = resp.json()?;
    Ok(json!({
        "full_name": r["full_name"],
        "description": r["description"],
        "stars": r["stargazers_count"],
        "forks": r["forks_count"],
        "open_issues": r["open_issues_count"],
        "default_branch": r["default_branch"],
        "language": r["language"],
        "archived": r["archived"],
        "pushed_at": r["pushed_at"],
    }))
}
