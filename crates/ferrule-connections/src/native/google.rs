//! Drive, Sheets, Docs and Calendar over Google's REST APIs, with a
//! service account's key (the simple way: it sees what's shared with it)
//! or an OAuth grant (the advanced way: it acts as you).

use super::jwt::{pkcs8_der, Signer};
use super::{arg_str, cap, limit, opt_str, seg, tool, Cred, Endpoints};
use crate::catalog::Service;
use async_trait::async_trait;
use ferrule_mcp::local::LocalServer;
use ferrule_mcp::CredentialSource;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const READ_SCOPES: &str = "https://www.googleapis.com/auth/drive.readonly \
     https://www.googleapis.com/auth/spreadsheets.readonly \
     https://www.googleapis.com/auth/documents.readonly \
     https://www.googleapis.com/auth/calendar.readonly";
const WRITE_SCOPES: &str = "https://www.googleapis.com/auth/drive \
     https://www.googleapis.com/auth/spreadsheets \
     https://www.googleapis.com/auth/documents \
     https://www.googleapis.com/auth/calendar.events \
     https://www.googleapis.com/auth/calendar.readonly";

/// What a service account's downloaded JSON key holds that ferrule uses.
pub struct ServiceAccount {
    pub email: String,
    pub der: Vec<u8>,
}

impl ServiceAccount {
    pub fn parse(json: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(json).map_err(|_| {
            "that isn't the JSON key file: paste the whole file Google downloaded (it starts \
             with {)"
                .to_string()
        })?;
        if v["type"] != "service_account" {
            return Err(
                "that JSON isn't a service account key (an OAuth client file looks \
                 similar: create a key under the service account's Keys tab)"
                    .into(),
            );
        }
        let email = v["client_email"]
            .as_str()
            .filter(|e| e.contains('@'))
            .ok_or("the key file has no client_email")?
            .to_string();
        let der = pkcs8_der(v["private_key"].as_str().unwrap_or(""))?;
        Signer::new(&der)?;
        Ok(Self { email, der })
    }
}

enum Auth {
    Key {
        sa: ServiceAccount,
        scopes: &'static str,
    },
    Source(Arc<dyn CredentialSource>),
}

pub struct Google {
    id: String,
    service: Service,
    auth: Auth,
    write: bool,
    http: reqwest::Client,
    ep: Endpoints,
    token: Mutex<Option<(String, Instant)>>,
}

pub fn hosts(ep: &Endpoints) -> Vec<String> {
    match &ep.google_base {
        Some(b) => vec![crate::explain::host_of(b)],
        None => [
            "oauth2.googleapis.com",
            "www.googleapis.com",
            "sheets.googleapis.com",
            "docs.googleapis.com",
        ]
        .map(String::from)
        .to_vec(),
    }
}

impl Google {
    pub fn new(
        id: &str,
        service: Service,
        cred: &Cred,
        write: bool,
        http: reqwest::Client,
        ep: Endpoints,
    ) -> Result<Self, String> {
        let auth = match cred {
            Cred::Fields(f) => Auth::Key {
                sa: ServiceAccount::parse(f.get("key").map_or("", |s| s))?,
                scopes: if write { WRITE_SCOPES } else { READ_SCOPES },
            },
            Cred::Source(s) => Auth::Source(s.clone()),
        };
        Ok(Self {
            id: format!("native:google:{id}"),
            service,
            auth,
            write,
            http,
            ep,
            token: Mutex::new(None),
        })
    }

    fn base(&self, api: &str) -> String {
        match (&self.ep.google_base, api) {
            (Some(b), _) => format!("{}/{api}", b.trim_end_matches('/')),
            (None, "drive/v3") => "https://www.googleapis.com/drive/v3".into(),
            (None, "calendar/v3") => "https://www.googleapis.com/calendar/v3".into(),
            (None, "sheets/v4") => "https://sheets.googleapis.com/v4".into(),
            (None, "docs/v1") => "https://docs.googleapis.com/v1".into(),
            (None, other) => format!("https://www.googleapis.com/{other}"),
        }
    }

    /// `Authorization`'s value: a cached service-account token, or the
    /// grant's current one.
    pub async fn token(&self) -> Result<String, String> {
        let (sa, scopes) = match &self.auth {
            Auth::Source(s) => return s.header().await.map(|(_, v)| v),
            Auth::Key { sa, scopes } => (sa, *scopes),
        };
        if let Some((t, until)) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            if Instant::now() < until {
                return Ok(t);
            }
        }
        let assertion = Signer::new(&sa.der)?.assertion(
            &sa.email,
            scopes,
            "https://oauth2.googleapis.com/token",
            crate::store::now(),
        )?;
        let resp = self
            .http
            .post(&self.ep.google_token)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .timeout(super::net::TIMEOUT)
            .send()
            .await
            .map_err(|_| "couldn't reach Google's sign-in service".to_string())?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if status != 200 {
            return Err(match v["error"].as_str() {
                Some("invalid_grant") => "Google refused the key: it was deleted or disabled \
                     in Google Cloud (Service accounts → Keys), or this machine's clock is off. \
                     Make a new key and paste it."
                    .into(),
                Some("unauthorized_client") | Some("invalid_client") => {
                    "Google doesn't recognise this service account (it was deleted or disabled)"
                        .into()
                }
                _ => format!("Google refused the key ({status})"),
            });
        }
        let token = v["access_token"]
            .as_str()
            .ok_or("Google's answer had no token")?
            .to_string();
        let life = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
        let bearer = format!("Bearer {token}");
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((bearer.clone(), Instant::now() + Duration::from_secs(life)));
        Ok(bearer)
    }

    async fn req(
        &self,
        method: reqwest::Method,
        url: String,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut r = self
            .http
            .request(method, url)
            .header("Authorization", self.token().await?)
            .query(query)
            .timeout(super::net::TIMEOUT);
        if let Some(b) = body {
            r = r.json(&b);
        }
        super::answer(&self.service, r.send().await, "Google").await
    }

    async fn get(&self, url: String, query: &[(&str, String)]) -> Result<Value, String> {
        self.req(reqwest::Method::GET, url, query, None).await
    }

    async fn text(&self, url: String, query: &[(&str, String)]) -> Result<String, String> {
        let resp = self
            .http
            .get(url)
            .header("Authorization", self.token().await?)
            .query(query)
            .timeout(super::net::TIMEOUT)
            .send()
            .await
            .map_err(|_| crate::explain::unreachable(&self.service, "Google"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(crate::explain::http(&self.service, status, &body).text);
        }
        Ok(body)
    }

    async fn drive_search(&self, args: &Value) -> Result<String, String> {
        let q = match (opt_str(args, "q"), opt_str(args, "text")) {
            (Some(q), _) => q.to_string(),
            (None, Some(t)) => format!(
                "fullText contains '{}' and trashed = false",
                t.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            (None, None) => "trashed = false".into(),
        };
        let v = self
            .get(
                format!("{}/files", self.base("drive/v3")),
                &[
                    ("q", q),
                    ("pageSize", limit(args, 20, 100).to_string()),
                    ("orderBy", "modifiedTime desc".into()),
                    (
                        "fields",
                        "files(id,name,mimeType,modifiedTime,webViewLink)".into(),
                    ),
                    ("supportsAllDrives", "true".into()),
                    ("includeItemsFromAllDrives", "true".into()),
                ],
            )
            .await?;
        let files = v["files"].as_array().cloned().unwrap_or_default();
        if files.is_empty() {
            return Ok(match self.auth {
                Auth::Key { ref sa, .. } => format!(
                    "no files match. A service account only sees what's shared with it: share \
                     files or a folder with {}",
                    sa.email
                ),
                Auth::Source(_) => "no files match".into(),
            });
        }
        Ok(files
            .iter()
            .map(|f| {
                format!(
                    "{} — {} ({}, modified {})\n",
                    f["id"].as_str().unwrap_or(""),
                    f["name"].as_str().unwrap_or(""),
                    short_type(f["mimeType"].as_str().unwrap_or("")),
                    f["modifiedTime"]
                        .as_str()
                        .unwrap_or("")
                        .get(..10)
                        .unwrap_or("")
                )
            })
            .collect())
    }

    async fn drive_get(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "file_id")?;
        let url = format!("{}/files/{}", self.base("drive/v3"), seg(id));
        let meta = self
            .get(
                url.clone(),
                &[
                    (
                        "fields",
                        "id,name,mimeType,modifiedTime,size,webViewLink".into(),
                    ),
                    ("supportsAllDrives", "true".into()),
                ],
            )
            .await?;
        let mime = meta["mimeType"].as_str().unwrap_or("");
        let head = format!(
            "{} ({}, modified {})\n{}\n\n",
            meta["name"].as_str().unwrap_or(""),
            short_type(mime),
            meta["modifiedTime"].as_str().unwrap_or(""),
            meta["webViewLink"].as_str().unwrap_or("")
        );
        let export = match mime {
            "application/vnd.google-apps.document" => Some("text/plain"),
            "application/vnd.google-apps.spreadsheet" => Some("text/csv"),
            "application/vnd.google-apps.presentation" => Some("text/plain"),
            _ => None,
        };
        let body = if let Some(as_type) = export {
            self.text(format!("{url}/export"), &[("mimeType", as_type.into())])
                .await?
        } else if mime.starts_with("text/") || mime == "application/json" {
            self.text(
                url,
                &[
                    ("alt", "media".into()),
                    ("supportsAllDrives", "true".into()),
                ],
            )
            .await?
        } else {
            "(not text: open the link to see it)".into()
        };
        Ok(cap(head + &body))
    }

    async fn sheets_read(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "spreadsheet_id")?;
        let range = opt_str(args, "range").unwrap_or("A1:Z200");
        let v = self
            .get(
                format!(
                    "{}/spreadsheets/{}/values/{}",
                    self.base("sheets/v4"),
                    seg(id),
                    seg(range)
                ),
                &[],
            )
            .await?;
        let rows = v["values"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            return Ok(format!("{} is empty", v["range"].as_str().unwrap_or(range)));
        }
        let mut out = format!("{}:\n", v["range"].as_str().unwrap_or(range));
        for r in rows {
            let cells: Vec<String> = r
                .as_array()
                .map(|c| {
                    c.iter()
                        .map(|x| x.as_str().map_or_else(|| x.to_string(), String::from))
                        .collect()
                })
                .unwrap_or_default();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        Ok(cap(out))
    }

    async fn sheets_write(&self, args: &Value, append: bool) -> Result<String, String> {
        let id = arg_str(args, "spreadsheet_id")?;
        let range = arg_str(args, "range")?;
        let values = args["values"]
            .as_array()
            .filter(|v| v.iter().all(|r| r.is_array()))
            .ok_or("`values` is a list of rows, each a list of cells")?;
        let base = format!(
            "{}/spreadsheets/{}/values/{}",
            self.base("sheets/v4"),
            seg(id),
            seg(range)
        );
        let body = json!({ "range": range, "majorDimension": "ROWS", "values": values });
        let q = [("valueInputOption", "USER_ENTERED".to_string())];
        let v = if append {
            self.req(
                reqwest::Method::POST,
                format!("{base}:append"),
                &q,
                Some(body),
            )
            .await?
        } else {
            self.req(reqwest::Method::PUT, base, &q, Some(body)).await?
        };
        let updated = if append { &v["updates"] } else { &v };
        Ok(format!(
            "{} {} row(s) at {}",
            if append { "appended" } else { "wrote" },
            updated["updatedRows"]
                .as_u64()
                .unwrap_or(values.len() as u64),
            updated["updatedRange"].as_str().unwrap_or(range)
        ))
    }

    async fn docs_get(&self, args: &Value) -> Result<String, String> {
        let id = arg_str(args, "document_id")?;
        let v = self
            .get(
                format!("{}/documents/{}", self.base("docs/v1"), seg(id)),
                &[],
            )
            .await?;
        let mut out = format!("{}\n\n", v["title"].as_str().unwrap_or(""));
        for block in v["body"]["content"].as_array().into_iter().flatten() {
            for el in block["paragraph"]["elements"]
                .as_array()
                .into_iter()
                .flatten()
            {
                out.push_str(el["textRun"]["content"].as_str().unwrap_or(""));
            }
            if block["table"].is_object() {
                out.push_str("[a table: read the file with drive_get for its text]\n");
            }
        }
        Ok(cap(out))
    }

    async fn calendar_list(&self, args: &Value) -> Result<String, String> {
        let cal = opt_str(args, "calendar_id").unwrap_or("primary");
        let mut q = vec![
            ("singleEvents", "true".to_string()),
            ("orderBy", "startTime".into()),
            ("maxResults", limit(args, 20, 100).to_string()),
        ];
        q.push((
            "timeMin",
            opt_str(args, "from")
                .map(String::from)
                .unwrap_or_else(rfc3339_now),
        ));
        if let Some(to) = opt_str(args, "to") {
            q.push(("timeMax", to.into()));
        }
        if let Some(t) = opt_str(args, "text") {
            q.push(("q", t.into()));
        }
        let v = self
            .get(
                format!("{}/calendars/{}/events", self.base("calendar/v3"), seg(cal)),
                &q,
            )
            .await?;
        let items = v["items"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            let hint = matches!(self.auth, Auth::Key { .. }) && cal == "primary";
            return Ok(if hint {
                "no events. `primary` is the service account's own (empty) calendar: share your \
                 calendar with it and pass your calendar's id (usually your email)"
                    .into()
            } else {
                "no events in that range".into()
            });
        }
        Ok(items
            .iter()
            .map(|e| {
                let when = |k: &str| {
                    e[k]["dateTime"]
                        .as_str()
                        .or(e[k]["date"].as_str())
                        .unwrap_or("")
                        .to_string()
                };
                format!(
                    "{} → {} · {}{}\n",
                    when("start"),
                    when("end"),
                    e["summary"].as_str().unwrap_or("(no title)"),
                    e["location"]
                        .as_str()
                        .map(|l| format!(" @ {l}"))
                        .unwrap_or_default()
                )
            })
            .collect())
    }

    async fn calendar_create(&self, args: &Value) -> Result<String, String> {
        let cal = opt_str(args, "calendar_id").unwrap_or("primary");
        let when = |k: &str| -> Result<Value, String> {
            let s = arg_str(args, k)?;
            Ok(if s.len() == 10 {
                json!({ "date": s })
            } else {
                let mut v = json!({ "dateTime": s });
                if let Some(tz) = opt_str(args, "time_zone") {
                    v["timeZone"] = json!(tz);
                }
                v
            })
        };
        let mut body = json!({
            "summary": arg_str(args, "summary")?,
            "start": when("start")?,
            "end": when("end")?,
        });
        for k in ["description", "location"] {
            if let Some(v) = opt_str(args, k) {
                body[k] = json!(v);
            }
        }
        let v = self
            .req(
                reqwest::Method::POST,
                format!("{}/calendars/{}/events", self.base("calendar/v3"), seg(cal)),
                &[],
                Some(body),
            )
            .await?;
        Ok(format!(
            "created: {}",
            v["htmlLink"].as_str().unwrap_or("the event")
        ))
    }
}

fn short_type(mime: &str) -> &str {
    match mime {
        "application/vnd.google-apps.document" => "Google Doc",
        "application/vnd.google-apps.spreadsheet" => "Google Sheet",
        "application/vnd.google-apps.presentation" => "Google Slides",
        "application/vnd.google-apps.folder" => "folder",
        "application/pdf" => "PDF",
        other => other,
    }
}

fn rfc3339_now() -> String {
    let secs = crate::store::now() as i64;
    let (y, m, d) = super::mime::civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[async_trait]
impl LocalServer for Google {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let s = |d: &str| json!({ "type": "string", "description": d });
        let mut t = vec![
            tool(
                "drive_search",
                "Find Google Drive files by text in them (`text`), or with Drive's query syntax (`q`, e.g. `name contains 'budget' and mimeType = 'application/vnd.google-apps.spreadsheet'`).",
                json!({ "text": s("Words to find"), "q": s("A Drive query"), "limit": { "type": "integer", "maximum": 100 } }),
                &[],
                true,
            ),
            tool(
                "drive_get",
                "A Drive file's details and text (Docs as text, Sheets as CSV).",
                json!({ "file_id": s("From drive_search or the file's link") }),
                &["file_id"],
                true,
            ),
            tool(
                "sheets_read",
                "Read cells from a Google Sheet (`range` in A1 notation, like `Sheet1!A1:D50`).",
                json!({ "spreadsheet_id": s("From the sheet's link"), "range": s("A1 notation") }),
                &["spreadsheet_id"],
                true,
            ),
            tool(
                "docs_get",
                "A Google Doc's text.",
                json!({ "document_id": s("From the doc's link") }),
                &["document_id"],
                true,
            ),
            tool(
                "calendar_list",
                "Upcoming Google Calendar events (from now, unless `from`; times in RFC 3339).",
                json!({ "calendar_id": s("`primary`, or a calendar's id (often an email)"), "from": s("RFC 3339"), "to": s("RFC 3339"), "text": s("Words to find"), "limit": { "type": "integer", "maximum": 100 } }),
                &[],
                true,
            ),
        ];
        if self.write {
            let rows = json!({ "type": "array", "items": { "type": "array" }, "description": "Rows of cells" });
            t.extend([
                tool(
                    "sheets_append",
                    "Add rows after the last filled row of a Google Sheet range.",
                    json!({ "spreadsheet_id": s(""), "range": s("Like Sheet1!A:D"), "values": rows }),
                    &["spreadsheet_id", "range", "values"],
                    false,
                ),
                tool(
                    "sheets_update",
                    "Overwrite cells in a Google Sheet range.",
                    json!({ "spreadsheet_id": s(""), "range": s("Like Sheet1!B2:C3"), "values": rows }),
                    &["spreadsheet_id", "range", "values"],
                    false,
                ),
                tool(
                    "calendar_create",
                    "Create a Google Calendar event (`start`/`end` as RFC 3339 times, or YYYY-MM-DD for all day).",
                    json!({ "calendar_id": s(""), "summary": s(""), "start": s(""), "end": s(""), "time_zone": s("Like Asia/Jerusalem"), "description": s(""), "location": s("") }),
                    &["summary", "start", "end"],
                    false,
                ),
            ]);
        }
        t
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "drive_search" => self.drive_search(&args).await,
            "drive_get" => self.drive_get(&args).await,
            "sheets_read" => self.sheets_read(&args).await,
            "docs_get" => self.docs_get(&args).await,
            "calendar_list" => self.calendar_list(&args).await,
            _ if !self.write => Err("this connection is read-only".into()),
            "sheets_append" => self.sheets_write(&args, true).await,
            "sheets_update" => self.sheets_write(&args, false).await,
            "calendar_create" => self.calendar_create(&args).await,
            other => Err(format!("no tool `{other}`")),
        }
    }
}
