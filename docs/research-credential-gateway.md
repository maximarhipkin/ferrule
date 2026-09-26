# Credential gateway: design, threat model, prior art, limits

Status: shipped 2026-09-24 as milestone M7, in the crate `ferrule-proxy`, and
wired into `ferrule-cli` through the `[secrets]` config section.

![Credential gateway flow](assets/credential-gateway.svg)

## 1. The problem

The agent's shell commands need credentials: `gh` and `git push` need a
GitHub token, and `curl` against an API needs its key. Since M6 the sandbox
strips secret-looking env vars from every command. The only alternative was
`env_passthrough`, which hands the command the real value. Once a command
has the real value, a prompt-injected model can print it, write it into a
file it can read later, or send it to any host on the internet.

Gateways such as OneCLI solve this with a connect flow per service. Max
asked for something simpler (msg 3052, 2026-09-24): one line of config per
secret, and no per-service integration.

## 2. Design: placeholders the proxy swaps

```toml
[secrets]
GITHUB_TOKEN = ["api.github.com", "*.githubusercontent.com"]
TELEGRAM_BOT_TOKEN = { hosts = ["api.telegram.org"], in_url = true }
```

The key is the env var ferrule reads, and the value lists the hosts allowed
to receive it. Host patterns are exact names or `*.suffix`. A bare `*`, a
scheme, a port or a path is rejected.

### The placeholder

Each sandboxed command gets `GITHUB_TOKEN=<placeholder>` instead of the
real value. The placeholder has the same shape as the real token, so client
libraries that validate a token's format still accept it:

- A known prefix is kept: `ghp_`, `github_pat_`, `glpat-`, `sk-ant-`,
  `sk-proj-`, `sk-`, `xoxb-`, `hf_`, `sk_live_` and a few others. It is kept
  only when at least 16 random characters remain after it.
- The rest is hex of `SHA-256(seed ‖ NAME ‖ counter)`. The seed is 32 random
  bytes created once in `<data_dir>/ferrule/proxy/keys/seed` (mode 0600;
  the directory is 0700).
- The placeholder is the same length as the real value when that leaves 16
  or more random characters. Otherwise it is longer: `passwd` becomes a
  16-character placeholder.
- It depends only on the seed, the name, the prefix and the length. So it
  stays the same across restarts, and across a token rotation that keeps the
  token's shape.

### The proxy

The proxy is a hyper HTTP/1 proxy on a random loopback port, running inside
the ferrule process:

- **Auth and methods.** It requires `Proxy-Authorization: Basic
  ferrule:<random token>` and otherwise answers 407 with
  `Proxy-Authenticate`, which git's anyauth flow understands. It accepts only
  `CONNECT`; anything else gets 400.
- **Connecting.** It connects upstream *before* answering `CONNECT`, so an
  unreachable host is a clean 502.
- **Unbound hosts** get a blind tunnel (`copy_bidirectional`). There is no
  TLS interception, so npm, cargo, pip and anything else pinning its own
  roots keep working untouched.
- **Bound hosts** get TLS interception. The CA is generated once under
  `<data_dir>/ferrule/proxy/keys` (key 0600, valid 10 years). Leaf
  certificates are minted per host (valid 1 year) and cached. ALPN is
  pinned to `http/1.1`. The proxy verifies the real server's certificate
  against Mozilla's roots (`webpki-roots`) plus the system CA bundle, so an
  upstream corporate or sandbox proxy that intercepts TLS keeps working.
- **Request checks on bound hosts.**
  - A `Host` header or absolute URI that doesn't match the `CONNECT` host
    gets 421.
  - `Upgrade` (websockets) gets 501.
  - Hop-by-hop headers, `Accept-Encoding` and `Expect` are stripped.
    `Accept-Encoding` goes so that responses come back uncompressed and can
    be scrubbed.
- **Upstream chaining.** The proxy reads `HTTPS_PROXY`, `https_proxy`,
  `ALL_PROXY` and `all_proxy` from ferrule's own environment (http:// only).
  Userinfo becomes `Proxy-Authorization`. `NO_PROXY` is honoured.

### Where the real value goes in (request)

On a bound host, only that host's own secrets are swapped, and only in:

1. **The `Authorization` header**, as Bearer, token or any other scheme.
   `Basic` credentials are base64-decoded, swapped and re-encoded, because
   git over HTTPS sends `x-access-token:<token>` that way.
2. **Credential-named headers.** These are headers whose lowercase name
   contains `auth`, `key`, `token`, `secret`, `password`, `passwd`,
   `credential` or `cookie`: `x-api-key`, `PRIVATE-TOKEN`, `x-goog-api-key`,
   `Cookie` and so on. The swapped value is marked sensitive.
3. **The URL path and query**, only for secrets with `in_url = true`. This
   is for APIs that put the key in the URL: Telegram's `/bot<token>/…`, or
   `?key=`. The real value is percent-encoded the way a client would have
   sent it.

Request bodies are never touched.

### Where it comes back out (response)

Response headers and identity-encoded bodies are scrubbed from real value
to placeholder. The body scrubber streams: it carries the last `maxlen-1`
bytes over, so a value split across two chunks is still caught. When the
lengths differ, `Content-Length` is dropped.

### The child's environment

Each command gets:

- `NAME=<placeholder>` for each secret that is set.
- `HTTPS_PROXY` and `https_proxy` set to
  `http://ferrule:<token>@127.0.0.1:<port>`, plus `NODE_USE_ENV_PROXY=1`
  so Node's built-in fetch uses the proxy.
- A CA bundle: the system bundle (or `SSL_CERT_FILE`) plus ferrule's CA.
  It goes in `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`,
  `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`, `DENO_CERT`, `CARGO_HTTP_CAINFO`
  and `AWS_CA_BUNDLE`. With no base bundle found, only `NODE_EXTRA_CA_CERTS`
  is set, and a warning says so.

`HTTP_PROXY` is left alone for commands. Since M26 ferrule's own clients
(`web_fetch`, MCP over HTTP) send plain HTTP through the proxy too; it
swaps secrets there only for loopback servers and answers 403 for a bound
remote host, since plain HTTP isn't something to put credentials on.

### Keeping the real value away from the command

The real value exists only in ferrule's memory. The secret's name is added
to the sandbox's `secret_vars`, so the real variable is scrubbed from the
child before the placeholder is set.

On Linux, Landlock's ptrace scoping stops a sandboxed command from reading
`/proc/<ferrule pid>/environ`. This was verified live: under the sandbox the
read fails with `Permission denied`, while the control outside the sandbox
reads it fine. `ferrule sandbox` checks this every time it runs.

### What the model is told

The system prompt gets a `[Credentials]` block listing each secret and its
hosts, marking the ones that are swapped in the URL too. The block says the
placeholders work in the `Authorization` header or a credential header, and
that anywhere else they stay a useless string.

## 3. Threat model

**Attacker:** the model, prompt-injected, running arbitrary commands in the
sandbox with network on.

**Goal:** learn the real value, or use it outside the hosts it is bound to.

What the design stops:

| Attempt | Result |
|---|---|
| `echo $GITHUB_TOKEN`, `env`, `/proc/self/environ` | placeholder only |
| `cat /proc/<ferrule pid>/environ` | denied by Landlock (Linux) |
| Send the placeholder to any unbound host | the host receives the useless placeholder |
| Ask a bound host to return the value | the response is scrubbed back to the placeholder |
| Put the placeholder in a file name, JSON header or message text on a bound host | not swapped: not a credential header, and the URL needs `in_url` |
| Put it in a request body | never swapped |

**A correction to the plan as first described to Max:** a placeholder sent
to an unbound host is *not blocked*. It passes through the blind tunnel as
the useless string it is. Blocking it would require intercepting TLS to
every host, which would break every tool that pins its own roots, and it
would protect nothing: the placeholder isn't the secret.

What it can still do:

- **Use the credential against the bound hosts.** That is the point of the
  gateway, so bind tokens with the smallest scope the task needs.
  Fine-grained GitHub tokens scoped to one repo beat classic `repo` tokens.
- **Reflection through a bound host.** If a bound host stores a request
  header or URL and shows it back in a *different encoding*, the scrubber
  can't recognise it. One example is an echo service returning Basic
  credentials still base64-encoded (httpbin's `/headers`). Another is a
  compressed body the host sends even though `Accept-Encoding` was
  stripped. The header-name filter and the URL opt-in shrink this surface
  to the places a credential normally goes. They don't close it. **Don't
  bind a secret to a host that echoes requests.**
  - An `in_url` secret is exposed to anything on that host that keeps URLs
    and lets the token's owner read them back: logs, webhooks, message
    history. Only opt in for APIs that require it.
  - fly.io's tokenizer documents the same echo attack for its design, and
    also relies on host binding as the mitigation (see §4).

Out of scope:

- A compromised ferrule process.
- Other users on the machine: the proxy is loopback-only and requires its
  per-run token.
- Anything running outside the sandbox.

## 4. Prior art

Every claim below links to the primary source. The research was done
2026-09-24.

- **Deno Sandbox secrets**
  ([security docs](https://docs.deno.com/sandbox/security/),
  [launch post](https://deno.com/blog/introducing-deno-sandbox)).
  - How it works: sandboxed code sees a placeholder per secret, and the
    real value is substituted on the wire only for approved hosts.
    Unapproved hosts are blocked at the VM boundary.
  - It is the closest design to ferrule's.
  - Not documented: the docs don't say whether injection is limited to
    headers or also covers the URL and body. They also don't mention
    response scrubbing.
- **fly.io tokenizer**
  ([README](https://github.com/superfly/tokenizer/blob/main/README.md),
  [user guide](https://github.com/superfly/tokenizer/blob/main/docs/UserGuide.md),
  [blog](https://fly.io/blog/tokenized-tokens/)).
  - How it works: the caller seals a secret to the proxy's public key and
    sends the sealed blob in a `Proxy-Tokenizer` header on each request. A
    processor per secret injects it into `Authorization: Bearer` by
    default, or into a named header, a query parameter or a body
    placeholder. It can also sign (HMAC, SigV4, JWT). `allowed_hosts` binds
    it to hosts.
  - Its README names the echo attack: route the sealed secret to a service
    that echoes the request and learn the plaintext. `allowed_hosts` is the
    only mitigation it gives. It also flags query strings as prone to
    ending up in logs.
  - Difference: tokenizer is caller-directed, per request. Ferrule's
    binding is fixed in config, and the command never addresses the
    injector.
- **Anthropic sandbox-runtime (`srt`)**
  ([README](https://github.com/anthropics/sandbox-runtime/blob/main/README.md)).
  - How it works: a local proxy plus an OS sandbox (bubblewrap/seccomp,
    Seatbelt) enforcing a domain allow/deny list.
  - No credential injection. Its proxy token "is not a secret from the
    sandbox itself". Whatever the agent authenticates with must already be
    in its own env.
  - Difference: ferrule has the same proxy-plus-sandbox shape and adds the
    injection layer.
- **OpenAI Codex CLI**
  ([approvals & security](https://learn.chatgpt.com/docs/agent-approvals-security),
  [repo](https://github.com/openai/codex/blob/main/AGENTS.md)).
  - How it works: Seatbelt or Landlock+seccomp, with an optional proxy that
    applies domain allow/deny rules.
  - No egress credential injection found in the published docs. That means
    it isn't documented, not that it's confirmed absent. Codex *cloud*
    removes setup-phase secrets before the agent phase, which is a
    different mechanism.
  - Ferrule's M6 sandbox follows Codex's design.
- **coder/httpjail**
  ([docs](https://coder.github.io/httpjail/introduction.html)).
  - How it works: an allow/deny filter proxy for a process tree. It doesn't
    rewrite requests.
- **Cloudflare Sandboxes outbound handlers**
  ([changelog](https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/)).
  - How it works: a per-host handler written by the developer, running
    outside the sandbox, sets the auth header. "No token is ever passed into
    the sandbox."
  - Difference: it takes code per service. Ferrule does it with a line of
    config.
- **GitHub Actions `::add-mask::`**
  ([docs](https://docs.github.com/en/actions/security-guides/using-secrets-in-github-actions)).
  - How it works: it masks by exact string in logs, only from the point it's
    registered. It has a documented bypass.
  - It is the analogue of ferrule's response scrubbing, not of injection.

Of these, only tokenizer publishes a reflection/exfiltration analysis. The
others are silent, which doesn't mean the risk doesn't apply to them.

## 5. Limits

- **No body substitution.** A credential that an API wants in a JSON body
  won't work (for example an OAuth `client_secret` POST).
- **Websockets and HTTP/2** aren't supported on bound hosts: `Upgrade` gets
  501, and ALPN is pinned to http/1.1. Unbound hosts are unaffected.
- **Any port on a bound host is intercepted,** not just 443.
- **Only the shell tool goes through the proxy.** `web_fetch` and the other
  in-process tools don't. MCP servers are spawned with ferrule's real
  environment and aren't sandboxed (an open M6 edge).
- **`[sandbox] network = false`** makes the proxy unreachable, so
  `[secrets]` does nothing. `ferrule sandbox` warns about this.
- **The guarantee needs an active OS sandbox.** Without one, a command can
  read ferrule's environment, and `ferrule sandbox` warns.
- **Reads are open under the sandbox,** so keep secrets out of files the
  agent can read (`.env` in the workspace, `~/.config/gh/hosts.yml`).
- **macOS:**
  - Go binaries use the keychain and ignore `SSL_CERT_FILE`, so `gh` needs
    ferrule's CA trusted in the keychain.
  - Whether Seatbelt stops a command from reading ferrule's environment
    (e.g. `ps eww`) is unverified, because the macOS backend hasn't been
    run on a real Mac yet.
- **Reflection** in another encoding (§3) and **compressed bodies** sent
  despite the stripped `Accept-Encoding` pass through unscrubbed.

## 6. Verification

- **Unit tests** (`ferrule-proxy`, 23) cover:
  - placeholder shape and stability
  - host patterns
  - Bearer, Basic and credential-header injection
  - non-credential headers left alone (`Dropbox-API-Arg`, `User-Agent`)
  - URL injection only for `in_url` secrets, with percent-encoding
  - another secret's placeholder left untouched
  - a scrubber value split across chunks
  - CA file modes
- **Integration tests** (4) run a local TLS origin through the real proxy
  with reqwest, and check:
  - Bearer, Basic and `x-api-key` arrive real
  - a non-credential `x-title` header arrives as the placeholder
  - the path and the `in_url` query parameter arrive real
  - the non-`in_url` query parameter stays a placeholder
  - `Accept-Encoding` is stripped
  - response headers and bodies (including a 200 KB body) are scrubbed
  - an unbound host gets a blind tunnel
  - a wrong proxy token gets 407, and a dead host gets 502
- **End to end** (`cargo test -p ferrule-proxy -- --ignored`): curl through
  this machine's real upstream proxy to httpbin.org with the placeholder
  as the Basic password returns 200 `authenticated: true`. httpbingo.org
  (unbound) returns 401.
- **Through the CLI** (`ferrule sandbox -- sh -c …`):
  - With the list form, `/basic-auth/user/$DEMO_PASSWORD` returns 401: the
    URL keeps the placeholder while the header gets the real value. The
    control, with the real value typed into the URL, returns 200.
  - With `in_url = true`, both return 200.
  - `cat /proc/$PPID/environ` is denied.
