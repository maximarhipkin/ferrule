//! The claims ferrule reads from the ChatGPT id and access tokens. Read
//! without checking the signature, as Codex does: the token came over TLS
//! from the issuer into this process, and the claims only pick a header
//! value and what status shows; nothing is authorised on them.

use ferrule_connections::seal::unb64;
use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Claims {
    pub exp: Option<u64>,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan: Option<String>,
    pub fedramp: bool,
}

const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// What a JWT says, or nothing when it isn't one.
pub(crate) fn claims(token: &str) -> Claims {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return Claims::default();
    };
    let Some(doc) = unb64(payload)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Claims::default();
    };
    let auth = &doc[AUTH_CLAIM];
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Claims {
        exp: doc["exp"].as_u64(),
        email: text(&doc["email"])
            .or_else(|| text(&doc["https://api.openai.com/profile"]["email"])),
        account_id: text(&auth["chatgpt_account_id"]),
        plan: text(&auth["chatgpt_plan_type"]),
        fedramp: auth["chatgpt_account_is_fedramp"]
            .as_bool()
            .unwrap_or(false),
    }
}

/// Claims from the id token, with the access token filling what it lacks.
pub(crate) fn merged(id_token: &str, access_token: &str) -> Claims {
    let id = claims(id_token);
    let access = claims(access_token);
    Claims {
        exp: access.exp,
        email: id.email.or(access.email),
        account_id: id.account_id.or(access.account_id),
        plan: id.plan.or(access.plan),
        fedramp: id.fedramp || access.fedramp,
    }
}

#[cfg(test)]
pub(crate) fn fake(claims: &Value) -> String {
    use ferrule_connections::seal::b64;
    format!(
        "{}.{}.sig",
        b64(b"{\"alg\":\"none\"}"),
        b64(claims.to_string().as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_account_and_plan_come_from_the_auth_claim() {
        let id = fake(&json!({"email": "max@example.com", AUTH_CLAIM: {
            "chatgpt_account_id": "acct_1", "chatgpt_plan_type": "pro"}}));
        let access = fake(
            &json!({"exp": 1_790_000_000u64, AUTH_CLAIM: {"chatgpt_account_is_fedramp": true}}),
        );
        let c = merged(&id, &access);
        assert_eq!(c.account_id.as_deref(), Some("acct_1"));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert_eq!(c.email.as_deref(), Some("max@example.com"));
        assert_eq!(c.exp, Some(1_790_000_000));
        assert!(c.fedramp);
        assert_eq!(claims("not a jwt"), Claims::default());
        assert_eq!(claims("a.b.c"), Claims::default());
    }
}
