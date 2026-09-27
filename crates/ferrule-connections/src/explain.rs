//! M37: what went wrong, in words. A sign-in error, a refused refresh, an
//! HTTP answer from a test or a tool: each becomes one or two sentences
//! that say what to do, with the exact domain or link where there is one.
//! Nothing here repeats a code, a `state`, a token or a server's text.

use crate::catalog::Service;

/// An explanation, and the simpler way in to offer instead, if there is
/// one (a service name: `jira` for Atlassian's sign-in).
#[derive(Debug, Clone, PartialEq)]
pub struct Explained {
    pub text: String,
    pub switch_to: Option<&'static str>,
    /// Which of [`symptoms`] it was, for the setup checklist.
    pub symptom: Option<&'static str>,
}

impl Explained {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            switch_to: None,
            symptom: None,
        }
    }

    fn tagged(mut self, symptom: &'static str) -> Self {
        self.symptom = Some(symptom);
        self
    }
}

pub fn is_google(s: &Service) -> bool {
    s.authorize_url
        .as_deref()
        .is_some_and(|u| u.contains("accounts.google.com"))
        || s.url.contains("googleapis.com")
        || matches!(s.native.as_deref(), Some("google") | Some("gmail"))
}

pub fn is_atlassian(s: &Service) -> bool {
    s.url.contains("atlassian.com") || s.native.as_deref() == Some("jira")
}

/// The host of a redirect URL, for "add this domain".
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "the callback address".into())
}

/// The pattern an Atlassian admin adds for `redirect`.
pub fn atlassian_pattern(redirect: &str) -> String {
    format!("https://{}/**", host_of(redirect))
}

const GOOGLE_TESTING: &str = "Google gives an app in Testing 7-day sign-ins: in Google Cloud → \
     Google Auth Platform → Audience, press Publish app (a personal app needn't be verified), \
     then reconnect.";

/// A sign-in that came back with `error` (and maybe a description) at
/// `redirect`.
pub fn oauth_error(
    service: &Service,
    error: &str,
    description: Option<&str>,
    redirect: &str,
) -> Explained {
    let title = service.title();
    let desc = description.unwrap_or("").to_ascii_lowercase();
    if is_atlassian(service)
        && (desc.contains("domain") || desc.contains("redirect") || desc.contains("not allowed"))
    {
        return Explained {
            text: format!(
                "Your Atlassian admin only allows sign-ins that come back to listed domains, and \
                 {host} isn't one. In admin.atlassian.com → Rovo → Rovo MCP server → Domains, \
                 add {pattern} — or use an API token, which needs no domain.",
                host = host_of(redirect),
                pattern = atlassian_pattern(redirect),
            ),
            switch_to: Some("jira"),
            symptom: Some("domain"),
        };
    }
    match error {
        "access_denied" if is_google(service) => Explained::plain(format!(
            "Google refused the sign-in. You pressed Cancel, or the app is in Testing and this \
             Google account isn't one of its test users: add it under Google Auth Platform → \
             Audience → Test users, or publish the app. Then try {title} again."
        ))
        .tagged("test-user"),
        "access_denied" => Explained {
            text: format!(
                "{title} refused the sign-in: you pressed Cancel, or an admin doesn't allow \
                 this app."
            ),
            switch_to: is_atlassian(service).then_some("jira"),
            symptom: is_atlassian(service).then_some("consent"),
        },
        "redirect_uri_mismatch" => {
            Explained::plain(redirect_mismatch(service, redirect)).tagged("redirect")
        }
        "org_internal" => Explained::plain(
            "This Google app is limited to its own Workspace organisation, and you signed in \
             with an account outside it. Sign in with an account from that organisation, or \
             set the app's audience to External.",
        ),
        "admin_policy_enforced" => Explained::plain(
            "Your Google Workspace admin blocks this app. Ask them to trust it (Admin console → \
             Security → API controls), or use a service account instead.",
        )
        .tagged("admin"),
        "invalid_scope" => Explained::plain(format!(
            "{title} doesn't offer one of the permissions ferrule asked for. If you changed the \
             service's scopes, check them against its docs."
        )),
        "unauthorized_client" | "invalid_client" => Explained::plain(format!(
            "{title} doesn't recognise ferrule's app. Check the client id and secret you saved \
             (and, for Google, that the client is a Web application)."
        )),
        "consent_required" | "interaction_required" | "login_required" => Explained::plain(
            format!("{title} needs you to finish signing in in the browser; try again."),
        ),
        "temporarily_unavailable" | "server_error" => Explained::plain(format!(
            "{title}'s sign-in is having trouble right now; try again in a few minutes."
        )),
        _ => Explained {
            text: format!("{title} didn't finish the sign-in. Try again, or use another option."),
            switch_to: is_atlassian(service).then_some("jira"),
            symptom: None,
        },
    }
}

fn redirect_mismatch(service: &Service, redirect: &str) -> String {
    if is_google(service) {
        format!(
            "Google doesn't have ferrule's callback address on its list. In Google Cloud → \
             Clients → your Web client → Authorized redirect URIs, add exactly {redirect} \
             (no trailing slash), save, wait a minute, and try again."
        )
    } else {
        format!(
            "{} doesn't accept ferrule's callback address {redirect}. Register it with the \
             service, or set up the relay so the address stays the same.",
            service.title()
        )
    }
}

/// The code exchange was refused with `code`.
pub fn exchange_refused(service: &Service, code: &str, redirect: &str) -> String {
    match code {
        "redirect_uri_mismatch" => redirect_mismatch(service, redirect),
        "invalid_grant" => format!(
            "{} said the sign-in code was already used or too old. Try again, and finish within \
             a few minutes.",
            service.title()
        ),
        "invalid_client" | "unauthorized_client" => format!(
            "{} doesn't accept ferrule's app credentials: check the client id and secret.",
            service.title()
        ),
        _ => format!(
            "{} didn't accept the sign-in code; try again.",
            service.title()
        ),
    }
}

/// The refresh token was refused with `code`: the grant is gone.
pub fn refresh_refused(service: &Service, code: &str) -> String {
    match code {
        "invalid_grant" if is_google(service) => format!(
            "Google no longer accepts the grant. It was revoked, the password changed, or the \
             app is in Testing, where sign-ins last 7 days. {GOOGLE_TESTING}"
        ),
        "invalid_grant" => "the service no longer accepts the grant (revoked or expired)".into(),
        "invalid_client" | "unauthorized_client" => {
            "the service no longer recognises ferrule's app (its client was deleted or changed)"
                .into()
        }
        _ => "the service refused to renew the grant".into(),
    }
}

/// An HTTP answer from a test or a tool, as words. `body` is the server's
/// answer: only a few known markers in it are looked for, never repeated.
pub fn http(service: &Service, status: u16, body: &str) -> Explained {
    let title = service.title();
    let b = body.to_ascii_lowercase();
    if is_google(service) {
        if b.contains("service_disabled") || b.contains("accessnotconfigured") {
            let api = google_api_in(&b);
            return Explained::plain(format!(
                "The {api} isn't turned on in your Google Cloud project. Open \
                 https://console.cloud.google.com/apis/library, turn it on, wait a minute and \
                 try again."
            ))
            .tagged("disabled");
        }
        if b.contains("storagequotaexceeded") {
            return Explained::plain(
                "A service account has no Drive space of its own, so it can't create files in \
                 your Drive. Share a folder with it (or use a shared drive) and create files \
                 there.",
            );
        }
        if b.contains("forbiddenforserviceaccounts") {
            return Explained::plain(
                "A service account can't invite attendees without domain-wide delegation. \
                 Create the event without attendees, or use the OAuth option.",
            );
        }
    }
    let text = match status {
        401 if is_atlassian(service) && service.native.is_none() => {
            return Explained {
                text: "Atlassian refused the token. Either the token or email is wrong, or \
                       your admin hasn't allowed API tokens for Rovo MCP (admin.atlassian.com \
                       → Rovo → Rovo MCP server → Authentication → API token). The \
                       email + token option works without that switch."
                    .into(),
                switch_to: Some("jira"),
                symptom: Some("token-off"),
            }
        }
        401 if is_atlassian(service) => "Atlassian refused the email and token pair. Check the \
             email is the one you sign in with, and make a new token if this one expired."
            .to_string(),
        401 => format!("{title} refused the credentials: check them, or make a new key."),
        403 if is_atlassian(service) => {
            "Atlassian accepted the token but won't allow this: your account lacks permission \
             for it on this site (or the token's scopes don't include it)."
                .to_string()
        }
        403 => format!(
            "{title} accepted the credentials but won't allow this: the key or account lacks \
             permission, or the item isn't shared with it."
        ),
        404 => format!("{title} has no such thing here: check the address, id or site name."),
        410 => format!("{title} retired this part of its API; ferrule needs an update."),
        429 => format!("{title} is rate-limiting ferrule; wait a minute and try again."),
        s if s >= 500 => format!("{title} is having trouble (it answered {s}); try again soon."),
        s => format!("{title} answered {s}, which ferrule didn't expect."),
    };
    Explained::plain(text)
}

fn google_api_in(b: &str) -> &'static str {
    for (marker, name) in [
        ("drive.googleapis.com", "Google Drive API"),
        ("sheets.googleapis.com", "Google Sheets API"),
        ("docs.googleapis.com", "Google Docs API"),
        ("calendar", "Google Calendar API"),
        ("gmail", "Gmail API"),
        ("mcp", "Google MCP API (a Developer Preview)"),
    ] {
        if b.contains(marker) {
            return name;
        }
    }
    "API"
}

/// A request that got no answer at all.
pub fn unreachable(service: &Service, what: &str) -> String {
    format!(
        "{} didn't answer at {what}: check the address, and that this machine can reach it.",
        service.title()
    )
}

/// What the owner may have seen on the vendor's page, for the wizard's
/// "what went wrong?" list: (id, what they saw, what to do).
pub fn symptoms(google: bool) -> Vec<(&'static str, &'static str, String)> {
    if google {
        vec![
            (
                "redirect",
                "Error 400: redirect_uri_mismatch",
                "Google doesn't have the callback address. Add the relay's /cb address, exactly \
                 as shown above, to your Web client's Authorized redirect URIs."
                    .into(),
            ),
            (
                "test-user",
                "Access blocked: the app has not completed verification / access_denied",
                "The app is in Testing and your account isn't a test user: add it under \
                 Audience → Test users, or publish the app."
                    .into(),
            ),
            (
                "unverified",
                "Google hasn't verified this app",
                "Normal for your own app. Press Advanced → Go to (your app) → Continue. \
                 Verification is only needed for apps other people use."
                    .into(),
            ),
            (
                "expired",
                "It worked, then stopped after a week",
                GOOGLE_TESTING.into(),
            ),
            (
                "disabled",
                "…API has not been used in project … or it is disabled",
                "Turn the API on: https://console.cloud.google.com/flows/enableapi?apiid=\
                 drive.googleapis.com,sheets.googleapis.com,docs.googleapis.com,\
                 calendar-json.googleapis.com"
                    .into(),
            ),
            (
                "admin",
                "Your administrator has blocked this app",
                "A Workspace admin must trust the app (Admin console → Security → API \
                 controls), or use a service account."
                    .into(),
            ),
        ]
    } else {
        vec![
            (
                "domain",
                "The site admin must allow this domain / redirect not allowed",
                "Your Atlassian admin must add the callback domain under Rovo → Rovo MCP \
                 server → Domains (the exact pattern is shown above), or use the email + token \
                 option."
                    .into(),
            ),
            (
                "token-off",
                "401 with an API token",
                "The admin hasn't turned on API-token sign-in for Rovo MCP (Rovo → Rovo MCP \
                 server → Authentication). The email + token option doesn't need it."
                    .into(),
            ),
            (
                "consent",
                "You don't have access / an admin must approve",
                "Your site's admin hasn't allowed Rovo MCP for your account. The email + token \
                 option uses your own Jira permissions instead."
                    .into(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    fn svc(name: &str) -> Service {
        Catalog::built_in().get(name).unwrap().clone()
    }

    #[test]
    fn the_symptom_lists_name_what_the_page_said_and_no_codes_of_ours() {
        let g = symptoms(true);
        let a = symptoms(false);
        for id in [
            "redirect",
            "test-user",
            "unverified",
            "expired",
            "disabled",
            "admin",
        ] {
            assert!(g.iter().any(|s| s.0 == id), "google lacks {id}");
        }
        for id in ["domain", "token-off", "consent"] {
            assert!(a.iter().any(|s| s.0 == id), "atlassian lacks {id}");
        }
        for (_, saw, fix) in g.iter().chain(a.iter()) {
            assert!(!saw.is_empty() && !fix.is_empty());
            assert!(!fix.contains("state=") && !fix.contains("code="), "{fix}");
        }
    }

    #[test]
    fn no_explanation_repeats_a_code_or_a_state() {
        let google = svc("google_oauth");
        let atl = svc("atlassian");
        let redirect = "https://ferrule-relay.acme.workers.dev/cb";
        for (s, code) in [
            (&google, "access_denied"),
            (&google, "redirect_uri_mismatch"),
            (&google, "org_internal"),
            (&google, "admin_policy_enforced"),
            (&atl, "access_denied"),
            (&atl, "weird_code_xyz"),
        ] {
            let e = oauth_error(s, code, Some("state=abc123"), redirect);
            assert!(!e.text.contains("weird_code_xyz"), "{}", e.text);
            assert!(!e.text.contains("abc123"), "{}", e.text);
        }
        let e = oauth_error(&google, "redirect_uri_mismatch", None, redirect);
        assert!(
            e.text.contains(redirect),
            "the exact URI to add: {}",
            e.text
        );
    }

    #[test]
    fn atlassians_domain_restriction_names_the_exact_pattern_and_offers_a_token() {
        let e = oauth_error(
            &svc("atlassian"),
            "access_denied",
            Some("The redirect domain is not allowed by your admin"),
            "https://ferrule-relay.acme.workers.dev/cb",
        );
        assert!(
            e.text.contains("https://ferrule-relay.acme.workers.dev/**"),
            "{}",
            e.text
        );
        assert_eq!(e.switch_to, Some("jira"));
    }

    #[test]
    fn googles_seven_day_testing_expiry_and_disabled_apis_are_named() {
        let t = refresh_refused(&svc("google_oauth"), "invalid_grant");
        assert!(t.contains("7 days") && t.contains("Publish app"), "{t}");
        let e = http(
            &svc("google"),
            403,
            r#"{"error":{"status":"PERMISSION_DENIED","details":[{"reason":"SERVICE_DISABLED","metadata":{"service":"sheets.googleapis.com"}}]}}"#,
        );
        assert!(e.text.contains("Google Sheets API"), "{}", e.text);
        let e = http(&svc("atlassian"), 401, "");
        assert_eq!(e.switch_to, Some("jira"));
        assert!(e.text.contains("Authentication"), "{}", e.text);
    }
}
