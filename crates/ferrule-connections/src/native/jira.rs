//! Jira and Confluence Cloud over REST, with an Atlassian account's email
//! and API token (Basic). Every request goes to the one site the owner
//! named: the token is bound to `https://<site>.atlassian.net`.

use super::{arg_str, cap, limit, opt_str, seg, tool};
use crate::catalog::Service;
use async_trait::async_trait;
use base64::Engine;
use ferrule_mcp::local::LocalServer;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// `acme`, `acme.atlassian.net` or `https://acme.atlassian.net/jira/…` →
/// `https://acme.atlassian.net`.
pub fn site_url(input: &str, loopback: bool) -> Result<String, String> {
    let input = input.trim().trim_end_matches('/');
    if loopback && input.starts_with("http://127.0.0.1:") {
        return Ok(input.to_string());
    }
    let bare = input
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = bare.split('/').next().unwrap_or("").to_ascii_lowercase();
    let host = if host.contains('.') {
        host
    } else {
        format!("{host}.atlassian.net")
    };
    let name = host.strip_suffix(".atlassian.net").unwrap_or("");
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(
            "the site should look like yourcompany.atlassian.net (the address you \
             open Jira at)"
                .into(),
        );
    }
    Ok(format!("https://{host}"))
}

pub struct Jira {
    id: String,
    service: Service,
    site: String,
    auth: String,
    write: bool,
    http: reqwest::Client,
}

impl Jira {
    pub fn new(
        id: &str,
        service: Service,
        fields: &BTreeMap<String, String>,
        write: bool,
        http: reqwest::Client,
    ) -> Result<Self, String> {
        let get = |k: &str| {
            fields
                .get(k)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| format!("the {k} is missing"))
        };
        let (email, token) = (get("email")?, get("token")?);
        Ok(Self {
            id: format!("native:jira:{id}"),
            service,
            site: get("site")?,
            auth: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            ),
            write,
            http,
        })
    }

    async fn req(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.site))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .query(query)
            .timeout(super::net::TIMEOUT);
        if let Some(b) = body {
            r = r.json(&b);
        }
        super::answer(&self.service, r.send().await, &self.site).await
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        self.req(reqwest::Method::GET, path, query, None).await
    }

    /// The display name the token belongs to.
    pub async fn whoami(&self) -> Result<String, String> {
        let me = self.get("/rest/api/3/myself", &[]).await?;
        Ok(me["displayName"]
            .as_str()
            .or(me["emailAddress"].as_str())
            .unwrap_or("you")
            .to_string())
    }

    async fn search(&self, args: &Value) -> Result<String, String> {
        let jql = arg_str(args, "jql")?;
        let v = self
            .get(
                "/rest/api/3/search/jql",
                &[
                    ("jql", jql.to_string()),
                    ("maxResults", limit(args, 20, 50).to_string()),
                    (
                        "fields",
                        "summary,status,assignee,priority,issuetype,updated".into(),
                    ),
                ],
            )
            .await?;
        let issues = v["issues"].as_array().cloned().unwrap_or_default();
        if issues.is_empty() {
            return Ok("no issues match".into());
        }
        let mut out = String::new();
        for i in &issues {
            let f = &i["fields"];
            out.push_str(&format!(
                "{} [{}] {} — {} ({}, updated {})\n",
                i["key"].as_str().unwrap_or("?"),
                f["issuetype"]["name"].as_str().unwrap_or(""),
                f["summary"].as_str().unwrap_or(""),
                f["status"]["name"].as_str().unwrap_or(""),
                f["assignee"]["displayName"]
                    .as_str()
                    .unwrap_or("unassigned"),
                f["updated"].as_str().unwrap_or("").get(..10).unwrap_or(""),
            ));
        }
        if v["nextPageToken"].is_string() {
            out.push_str("(more match: narrow the JQL or raise `limit`)\n");
        }
        Ok(out)
    }

    async fn get_issue(&self, args: &Value) -> Result<String, String> {
        let key = arg_str(args, "key")?;
        let v = self
            .get(
                &format!("/rest/api/3/issue/{}", seg(key)),
                &[(
                    "fields",
                    "summary,status,assignee,reporter,priority,issuetype,description,comment,\
                     created,updated,labels,parent"
                        .into(),
                )],
            )
            .await?;
        let f = &v["fields"];
        let mut out = format!(
            "{} — {}\nType: {} · Status: {} · Priority: {}\nAssignee: {} · Reporter: {}\n\
             Created {} · Updated {}\n",
            v["key"].as_str().unwrap_or(key),
            f["summary"].as_str().unwrap_or(""),
            f["issuetype"]["name"].as_str().unwrap_or(""),
            f["status"]["name"].as_str().unwrap_or(""),
            f["priority"]["name"].as_str().unwrap_or("none"),
            f["assignee"]["displayName"]
                .as_str()
                .unwrap_or("unassigned"),
            f["reporter"]["displayName"].as_str().unwrap_or(""),
            f["created"].as_str().unwrap_or(""),
            f["updated"].as_str().unwrap_or(""),
        );
        if let Some(labels) = f["labels"].as_array().filter(|l| !l.is_empty()) {
            let l: Vec<&str> = labels.iter().filter_map(|x| x.as_str()).collect();
            out.push_str(&format!("Labels: {}\n", l.join(", ")));
        }
        out.push_str(&format!("\n{}\n", adf_text(&f["description"]).trim()));
        let comments = f["comment"]["comments"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !comments.is_empty() {
            out.push_str(&format!("\nComments ({}):\n", comments.len()));
            for c in comments.iter().rev().take(20).rev() {
                out.push_str(&format!(
                    "— {} ({}): {}\n",
                    c["author"]["displayName"].as_str().unwrap_or(""),
                    c["created"].as_str().unwrap_or("").get(..10).unwrap_or(""),
                    adf_text(&c["body"]).trim()
                ));
            }
        }
        Ok(cap(out))
    }

    async fn create_issue(&self, args: &Value) -> Result<String, String> {
        let mut fields = json!({
            "project": { "key": arg_str(args, "project")? },
            "summary": arg_str(args, "summary")?,
            "issuetype": { "name": opt_str(args, "issue_type").unwrap_or("Task") },
        });
        if let Some(d) = opt_str(args, "description") {
            fields["description"] = adf(d);
        }
        if let Some(p) = opt_str(args, "parent") {
            fields["parent"] = json!({ "key": p });
        }
        let v = self
            .req(
                reqwest::Method::POST,
                "/rest/api/3/issue",
                &[],
                Some(json!({ "fields": fields })),
            )
            .await?;
        let key = v["key"].as_str().unwrap_or("?");
        Ok(format!("created {key}: {}/browse/{key}", self.site))
    }

    async fn update_issue(&self, args: &Value) -> Result<String, String> {
        let key = arg_str(args, "key")?;
        let mut fields = args["fields"].as_object().cloned().unwrap_or_default();
        if let Some(s) = opt_str(args, "summary") {
            fields.insert("summary".into(), json!(s));
        }
        if let Some(d) = opt_str(args, "description") {
            fields.insert("description".into(), adf(d));
        }
        if fields.is_empty() {
            return Err("nothing to change: give `summary`, `description` or `fields`".into());
        }
        self.req(
            reqwest::Method::PUT,
            &format!("/rest/api/3/issue/{}", seg(key)),
            &[],
            Some(json!({ "fields": fields })),
        )
        .await?;
        Ok(format!("updated {key}"))
    }

    async fn add_comment(&self, args: &Value) -> Result<String, String> {
        let key = arg_str(args, "key")?;
        self.req(
            reqwest::Method::POST,
            &format!("/rest/api/3/issue/{}/comment", seg(key)),
            &[],
            Some(json!({ "body": adf(arg_str(args, "body")?) })),
        )
        .await?;
        Ok(format!("commented on {key}"))
    }

    async fn transition(&self, args: &Value) -> Result<String, String> {
        let key = arg_str(args, "key")?;
        let path = format!("/rest/api/3/issue/{}/transitions", seg(key));
        let v = self.get(&path, &[]).await?;
        let list = v["transitions"].as_array().cloned().unwrap_or_default();
        let describe = || {
            list.iter()
                .map(|t| {
                    format!(
                        "{} ({} → {})",
                        t["id"].as_str().unwrap_or("?"),
                        t["name"].as_str().unwrap_or(""),
                        t["to"]["name"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let Some(want) = opt_str(args, "transition") else {
            return Ok(format!("{key} can move with: {}", describe()));
        };
        let found = list.iter().find(|t| {
            t["id"].as_str() == Some(want)
                || t["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(want))
                || t["to"]["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(want))
        });
        let Some(t) = found else {
            return Err(format!(
                "{key} has no transition `{want}`; it can move with: {}",
                describe()
            ));
        };
        self.req(
            reqwest::Method::POST,
            &path,
            &[],
            Some(json!({ "transition": { "id": t["id"] } })),
        )
        .await?;
        Ok(format!(
            "{key} moved to {}",
            t["to"]["name"].as_str().unwrap_or(want)
        ))
    }

    async fn confluence_search(&self, args: &Value) -> Result<String, String> {
        let cql = match (opt_str(args, "cql"), opt_str(args, "text")) {
            (Some(c), _) => c.to_string(),
            (None, Some(t)) => format!(
                "type = page AND text ~ \"{}\" ORDER BY lastmodified DESC",
                t.replace('\\', "\\\\").replace('"', "\\\"")
            ),
            (None, None) => return Err("give `text` or `cql`".into()),
        };
        let v = self
            .get(
                "/wiki/rest/api/search",
                &[("cql", cql), ("limit", limit(args, 10, 25).to_string())],
            )
            .await?;
        let results = v["results"].as_array().cloned().unwrap_or_default();
        if results.is_empty() {
            return Ok("no pages match".into());
        }
        let mut out = String::new();
        for r in &results {
            let c = &r["content"];
            out.push_str(&format!(
                "{} — {} (space {}, {})\n  {}\n",
                c["id"].as_str().unwrap_or("?"),
                c["title"].as_str().or(r["title"].as_str()).unwrap_or(""),
                r["resultGlobalContainer"]["title"].as_str().unwrap_or(""),
                r["friendlyLastModified"].as_str().unwrap_or(""),
                super::mime::html_to_text(r["excerpt"].as_str().unwrap_or(""))
                    .trim()
                    .replace('\n', " "),
            ));
        }
        Ok(out)
    }

    async fn confluence_page(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "page_id")?;
        let v = self
            .get(
                &format!("/wiki/api/v2/pages/{}", seg(id)),
                &[("body-format", "storage".into())],
            )
            .await?;
        Ok(cap(format!(
            "{} (version {})\n\n{}",
            v["title"].as_str().unwrap_or(""),
            v["version"]["number"],
            super::mime::html_to_text(v["body"]["storage"]["value"].as_str().unwrap_or("")).trim()
        )))
    }
}

/// Plain text as an Atlassian Document: a paragraph per line.
pub fn adf(text: &str) -> Value {
    let content: Vec<Value> = text
        .lines()
        .map(|line| {
            if line.is_empty() {
                json!({ "type": "paragraph" })
            } else {
                json!({ "type": "paragraph", "content": [{ "type": "text", "text": line }] })
            }
        })
        .collect();
    json!({ "type": "doc", "version": 1, "content": content })
}

/// An Atlassian Document's text, a line per block.
pub fn adf_text(v: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        if let Some(t) = v["text"].as_str() {
            out.push_str(t);
        }
        match v["type"].as_str() {
            Some("hardBreak") => out.push('\n'),
            Some("mention") => out.push_str(v["attrs"]["text"].as_str().unwrap_or("")),
            _ => {}
        }
        if let Some(children) = v["content"].as_array() {
            for c in children {
                walk(c, out);
            }
        }
        match v["type"].as_str() {
            Some("paragraph") => out.push('\n'),
            Some("heading" | "listItem" | "codeBlock" | "blockquote") if !out.ends_with('\n') => {
                out.push('\n')
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    walk(v, &mut out);
    out
}

#[async_trait]
impl LocalServer for Jira {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let key = json!({ "type": "string", "description": "The issue key, like PROJ-123" });
        let mut t = vec![
            tool(
                "jira_search",
                "Find Jira issues with JQL, e.g. `assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC`.",
                json!({ "jql": { "type": "string" }, "limit": { "type": "integer", "maximum": 50 } }),
                &["jql"],
                true,
            ),
            tool(
                "jira_get_issue",
                "One Jira issue: its fields, description and latest comments.",
                json!({ "key": key }),
                &["key"],
                true,
            ),
            tool(
                "confluence_search",
                "Find Confluence pages by text (or with CQL).",
                json!({ "text": { "type": "string" }, "cql": { "type": "string" }, "limit": { "type": "integer", "maximum": 25 } }),
                &[],
                true,
            ),
            tool(
                "confluence_get_page",
                "One Confluence page's text, by its id.",
                json!({ "page_id": { "type": "string" } }),
                &["page_id"],
                true,
            ),
        ];
        if self.write {
            t.extend([
                tool(
                    "jira_create_issue",
                    "Create a Jira issue.",
                    json!({
                        "project": { "type": "string", "description": "The project key, like PROJ" },
                        "summary": { "type": "string" },
                        "description": { "type": "string" },
                        "issue_type": { "type": "string", "description": "Task (default), Bug, Story…" },
                        "parent": { "type": "string", "description": "A parent issue key, for a sub-task" },
                    }),
                    &["project", "summary"],
                    false,
                ),
                tool(
                    "jira_update_issue",
                    "Change a Jira issue's summary, description or other fields (`fields` as Jira's REST API names them).",
                    json!({ "key": key, "summary": { "type": "string" }, "description": { "type": "string" }, "fields": { "type": "object" } }),
                    &["key"],
                    false,
                ),
                tool(
                    "jira_add_comment",
                    "Comment on a Jira issue.",
                    json!({ "key": key, "body": { "type": "string" } }),
                    &["key", "body"],
                    false,
                ),
                tool(
                    "jira_transition",
                    "Move a Jira issue (to a status name or a transition id). Without `transition`, lists where it can move.",
                    json!({ "key": key, "transition": { "type": "string" } }),
                    &["key"],
                    false,
                ),
            ]);
        }
        t
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "jira_search" => self.search(&args).await,
            "jira_get_issue" => self.get_issue(&args).await,
            "confluence_search" => self.confluence_search(&args).await,
            "confluence_get_page" => self.confluence_page(&args).await,
            _ if !self.write => Err("this connection is read-only".into()),
            "jira_create_issue" => self.create_issue(&args).await,
            "jira_update_issue" => self.update_issue(&args).await,
            "jira_add_comment" => self.add_comment(&args).await,
            "jira_transition" => self.transition(&args).await,
            other => Err(format!("no tool `{other}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_is_named_any_way_and_bound_to_atlassian_net() {
        for s in [
            "acme",
            "acme.atlassian.net",
            "https://acme.atlassian.net/jira/software/projects",
            " ACME.atlassian.net/ ",
        ] {
            assert_eq!(
                site_url(s, false).unwrap(),
                "https://acme.atlassian.net",
                "{s}"
            );
        }
        for bad in ["evil.com", "acme.atlassian.net.evil.com", "", "a b"] {
            assert!(site_url(bad, false).is_err(), "{bad}");
        }
        assert!(site_url("http://127.0.0.1:9/x", false).is_err());
        assert!(site_url("http://127.0.0.1:9", true).is_ok());
    }

    #[test]
    fn adf_round_trips_lines() {
        let doc = adf("one\n\nthree");
        assert_eq!(adf_text(&doc), "one\n\nthree\n");
        assert_eq!(adf_text(&Value::Null), "");
    }
}
