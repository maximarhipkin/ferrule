//! JSON5 as OpenClaw writes `openclaw.json`, turned into plain JSON for
//! serde_json: `//` and `/* */` comments, trailing commas, unquoted keys
//! and single-quoted strings. Numbers stay as JSON has them (no hex, no
//! `Infinity`): a config doesn't use those for anything the importer reads.

use serde_json::Value;

pub fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str(&normalise(text)?).map_err(|e| e.to_string())
}

fn normalise(text: &str) -> Result<String, String> {
    let chars: Vec<char> = text.trim_start_matches('\u{feff}').chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                let (s, next) = string(&chars, i)?;
                out.push_str(&serde_json::to_string(&s).unwrap());
                i = next;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("a /* comment isn't closed".into());
                }
                i += 2;
            }
            '}' | ']' => {
                let trimmed = out.trim_end().len();
                if out[..trimmed].ends_with(',') {
                    out.truncate(trimmed - 1);
                }
                out.push(c);
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let mut k = i;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if chars.get(k) == Some(&':') {
                    out.push_str(&serde_json::to_string(&word).unwrap());
                } else {
                    out.push_str(&word);
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// The string starting at `chars[start]` (its quote), and the index after
/// its closing quote.
fn string(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let quote = chars[start];
    let mut s = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            c if c == quote => return Ok((s, i + 1)),
            '\\' => {
                let Some(&e) = chars.get(i + 1) else { break };
                i += 2;
                match e {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    '0' => s.push('\0'),
                    '\n' => {}
                    'u' => {
                        let hex: String = chars.iter().skip(i).take(4).collect();
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|_| format!("a bad \\u escape: \\u{hex}"))?;
                        s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    other => s.push(other),
                }
            }
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    Err("a string isn't closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openclaw_style_json5_reads_as_json() {
        let text = r#"{
          // the model
          agents: { defaults: { model: { primary: 'anthropic/claude-opus-4-6', }, }, },
          /* channels */
          channels: {
            telegram: { botToken: "${TELEGRAM_BOT_TOKEN}", allowFrom: [123456789, "987", ], },
            note: 'it\'s "quoted" // not a comment',
          },
          "$include": "./more.json5",
        }"#;
        assert_eq!(
            parse(text).unwrap(),
            json!({
                "agents": {"defaults": {"model": {"primary": "anthropic/claude-opus-4-6"}}},
                "channels": {
                    "telegram": {"botToken": "${TELEGRAM_BOT_TOKEN}", "allowFrom": [123456789, "987"]},
                    "note": "it's \"quoted\" // not a comment",
                },
                "$include": "./more.json5",
            })
        );
        assert_eq!(
            parse("{a: true, b: null}").unwrap(),
            json!({"a": true, "b": null})
        );
        assert!(parse("{a: 'open").is_err());
        assert!(parse("{a: 1 /* open").is_err());
    }
}
