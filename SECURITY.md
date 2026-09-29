# Security

Ferrule is an agent runtime: it runs shell commands and holds API tokens on
your behalf. The boundaries described below are the security contract — if
you find a way across one, we want to hear about it.

## Reporting a vulnerability

Report vulnerabilities **privately** through GitHub:

**https://github.com/maximarhipkin/ferrule/security/advisories/new**

(the "Report a vulnerability" button on the repository's Security tab).
Please don't open a public issue or pull request for a vulnerability.

Include:

- the ferrule version (`ferrule --version`) or the commit you built from,
  and the platform (Linux / macOS / Windows, and the kernel for Linux),
- steps to reproduce, or a proof of concept,
- the impact — in particular, which of the boundaries below it crosses.

The maintainer will investigate, keep you posted in the advisory thread,
and credit you in the fix's release notes unless you'd rather not be named.
There is no bug bounty program.

## Supported versions

Only the latest release is supported; fixes ship as new releases and are not
backported. Releases are signed (SHA-256 plus a minisign signature against a
key built into the binary), and a running service updates itself
([docs/updates.md](docs/updates.md)).

## The security model

Four layers stand between the model and your machine and tokens, all on by
default. The design, threat model and honest limits of each:

- **OS sandbox.** Every command the agent runs through the `shell` tool, and
  every stdio MCP server, runs under Landlock + seccomp on Linux, Seatbelt on
  macOS, or a restricted token inside a job object on Windows. Writes are
  confined to the workspace (plus temp dirs and any `writable_roots`);
  secret-looking environment variables (`*KEY*`, `*TOKEN*`, `*SECRET*`…) and
  every provider's `api_key_env` are stripped; reads of ferrule's own
  secrets, the usual credential dirs (`~/.ssh`, cloud CLIs, browser
  profiles) and any `deny_read` path are denied. `network = false` is
  enforced with seccomp on Linux. `ferrule sandbox` tests each promise live.
  ([docs/sandbox.md](docs/sandbox.md),
  [docs/windows-sandbox.md](docs/windows-sandbox.md))
- **Credential gateway.** Commands see placeholders of the same shape, never
  the real token. A TLS-intercepting loopback proxy (per-run auth token,
  CONNECT-only) swaps in the real value on the wire, only for the hosts you
  bound the key to, and only in `Authorization` or credential-named headers
  (the URL only with `in_url = true`, bodies never); responses are scrubbed
  back to placeholders and every other host gets a blind tunnel. Saved keys
  live in `secrets.env` (0600, in a 0700 directory) that the agent's file
  tools and sandboxed shell can't reach. (README § Credential gateway,
  [docs/research-credential-gateway.md](docs/research-credential-gateway.md))
- **Egress policy.** Private networks, loopback and cloud metadata addresses
  are blocked for the model's tools by default, checked after DNS so
  rebinding can't get around it, with `allow`/`deny` lists and a
  unix-socket allowlist on top. ([docs/egress.md](docs/egress.md))
- **Trust & audit.** Token and dollar caps per run, per day and per task;
  a kill switch; approval gates on destructive commands, answered by the
  owner; plan mode. Decisions are auditable: trust events (approvals, caps,
  the kill switch, plan mode) land in `<data dir>/trust/audit.jsonl`, hook
  runs in `<data dir>/hooks/runs.jsonl`, and every model call in
  `<data dir>/ledger.jsonl`; an egress refusal adds a ledger row and an
  audit event. `ferrule doctor` re-checks the whole posture — sandbox
  promises, key file permissions, the proxy path — and names the fix.

## What's in scope

Examples of reports we take seriously:

- a sandboxed command writing outside the workspace, reading a `deny_read`
  path, or recovering a stripped environment variable or a saved key,
- a real token reaching a command, leaving the machine to a host it isn't
  bound to, or being swapped into a request body,
- the agent answering its own approval prompt, or bypassing the kill switch
  or a spending cap,
- an egress bypass (DNS rebinding, an unexpected socket, …),
- a WASM plugin exceeding the capabilities it was granted at install,
- the update mechanism installing an unsigned or mismatched release.

## Known limits (documented, not bugs)

These are design limits, called out in the docs; reports about them are
welcome only with a way to do better:

- On Windows, Git Bash commands run **unsandboxed** behind a warning
  (set `FERRULE_SHELL=powershell` for the sandbox), and `network = false`
  isn't enforced — see [docs/windows-sandbox.md](docs/windows-sandbox.md).
- The proxy covers HTTPS only (it speaks CONNECT); plain HTTP is left alone,
  and HTTP/2 and websockets aren't supported on bound hosts.
- Anything a bound host itself allows the token to do, the agent can do —
  scope tokens tightly.
- stdio MCP servers always have the network; most need it.
- With the sandbox off (`[sandbox] mode = "off"` or an unsupported host),
  restrictions rest on the tool set and the prompt, and `ferrule doctor`
  says so.
- The model itself is assumed to be prompt-injectable; that's the threat
  the layers above are built for. Content that stays inside the boundaries
  (a bad edit inside the workspace, a rude message sent to an approved
  chat) is a reliability issue, not a security boundary crossing.
