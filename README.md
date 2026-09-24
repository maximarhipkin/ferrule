<p align="center">
  <img src="docs/branding/hero.png" alt="Ferrule" width="720">
</p>

<p align="center">
  <b>An AI agent runtime in one small Rust binary.</b><br>
  Chat, schedule, sandbox and hand out credentials without handing over the secret.
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#credential-gateway">Credential gateway</a> ·
  <a href="#roadmap">Roadmap</a> ·
  <a href="PLAN.md">PLAN.md</a>
</p>

---

Ferrule runs a coding and operations agent against any OpenAI-compatible
model. You can use it from the terminal, from Telegram, or on a cron
schedule. Its shell commands run in an OS sandbox, and API tokens reach
them only as placeholders that a built-in proxy swaps for the real value,
and only on the hosts you allow.

It's one binary of about 10 MB with nothing to install beside it; the Linux
release builds are fully static. Measured on Linux x86-64:

- `ferrule --version` starts in about 4 ms.
- The idle gateway daemon uses about 9 MB of RAM.

All state lives in files you can read: SQLite for memory and tasks, and
JSONL for transcripts and the cost ledger.

## Why

The harness, not the model, is the performance lever. In OpenAI's 2026
ARC-AGI-3 runs, the same model scored 13.3% with a default harness and 38.3%
with an engineered one, with about 6× fewer output tokens. Ferrule is built
around that result:

<p align="center">
  <img src="docs/assets/chart-harness.png" alt="Same model, different harness: 13.3% vs 38.3% on ARC-AGI-3, 6x fewer output tokens" width="820">
</p>

- **A harness profile per model**: context window, when to compact (at
  about 70–75% of the window, not 95%), whether reasoning is carried across
  turns, and the system-prompt dialect. Kimi K2's interleaved thinking is
  kept across turns, and unknown endpoints get a conservative profile.
- **Structured compaction**: tool results are deduplicated for free before
  a checklist summary spends any tokens.
- **The build is the judge**: with `[agent] verify_command = "cargo test"`,
  the agent can't finish until the command passes.

Research and sources are in [`docs/research-report.md`](docs/research-report.md).

## What's inside

<p align="center">
  <img src="docs/assets/architecture.svg" alt="Ferrule architecture" width="860">
</p>

| | |
|---|---|
| **Agent loop** | ReAct loop with typed lifecycle events, resumable JSONL transcripts, compaction, and reasoning retention. |
| **Providers** | One OpenAI-compatible driver: Kimi, OpenAI, DeepSeek, OpenRouter, Groq, Ollama, llama.cpp, vLLM. |
| **Tools** | File read, write and list (workspace-scoped), `shell`, `web_fetch`, `write_todos` and `log_diary`, `remember` and `recall`. |
| **MCP** | stdio MCP servers. Their tools register as `mcp__<server>__<tool>`. |
| **Skills** | Agent Skills (`SKILL.md` folders, Claude-compatible), loaded on demand. |
| **Memory** | One SQLite file: FTS5 BM25 with time decay and token-budgeted recall. |
| **Gateway** | A long-running daemon with Telegram and local channels, one session lane per chat, resumed across restarts. |
| **Scheduler** | Cron (with IANA timezone) and one-shot tasks, with gate scripts, no overlapping runs, and a truthful status per run. |
| **OS sandbox** | Every shell command runs under Landlock (+ seccomp) on Linux or Seatbelt on macOS. Writes are confined to the workspace, and secret env vars are stripped. Native Windows has no sandbox yet ([below](#windows)). |
| **Credential gateway** | Commands get a placeholder token. A local proxy swaps in the real one only for the hosts you allow. |
| **Ledger** | Every model call is logged: tokens, cache hits, latency, errors, cost. |
| **Context baseline** | `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` or `ferrule.md` in the workspace goes into the system prompt. |

## Install

**Linux and macOS:**

```bash
curl -fsSL https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.sh | sh
```

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.ps1 | iex
```

The script downloads the release for your machine, checks its SHA-256,
installs it and starts `ferrule setup`. Nothing to export, no file to edit.
Run it again to upgrade: your settings stay, and on Linux and macOS a
running background service is restarted on the new binary.

| | Installs to | Prebuilt for |
|---|---|---|
| Linux | `~/.local/bin/ferrule` | x86-64, arm64 (static, any distro) |
| macOS | `~/.local/bin/ferrule` | Apple silicon, Intel |
| Windows | `%LOCALAPPDATA%\Programs\ferrule\ferrule.exe`, added to your PATH | x86-64 (ARM64 runs it under emulation) |

Both scripts read `FERRULE_VERSION` (a tag such as `v0.2.0`; default the
latest), `FERRULE_INSTALL_DIR` and `FERRULE_NO_SETUP=1` (install only).

> **While the repository is private**, the scripts and the release both
> need a GitHub token that can read it:
>
> ```bash
> export GITHUB_TOKEN=github_pat_...
> curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github.raw" \
>   https://api.github.com/repos/maximarhipkin/ferrule/contents/install.sh | sh
> ```
>
> ```powershell
> $env:GITHUB_TOKEN = 'github_pat_...'
> irm -Headers @{ Authorization = "Bearer $env:GITHUB_TOKEN"; Accept = 'application/vnd.github.raw' } `
>   https://api.github.com/repos/maximarhipkin/ferrule/contents/install.ps1 | iex
> ```

### Setup

`ferrule setup` walks through everything, testing keys and tokens as you
enter them:

1. **Model provider**: OpenAI, Moonshot (Kimi), OpenRouter, DeepSeek,
   Anthropic, Ollama on this machine, or any OpenAI-compatible URL. Paste
   the key, pick a model from the ones the key can use.
2. **Telegram** (optional): paste the token from
   [@BotFather](https://t.me/BotFather), then message the bot. Your chat
   goes on the bot's allow-list; it ignores everyone else.
3. **Tool credentials** (optional): tokens the agent's commands may use,
   such as `GITHUB_TOKEN`, each bound to the hosts it's for (see the
   [credential gateway](#credential-gateway)).
4. **Sandbox**: the recommended policy, or your own.
5. **Background service**: the gateway as a systemd user service (Linux)
   or a launchd agent (macOS), started at login and restarted if it stops.

Run it again any time to change one part: it opens on a menu with what's
set now. Keys go into a private file (0600, in a directory the agent's
commands and file tools can't reach), never into the config.

```bash
ferrule doctor          # checks config, keys, Telegram, sandbox and service; says what to fix
ferrule config path     # where the config, the keys, the data and the service unit are
ferrule config edit     # open the config in $EDITOR, then check it still parses
ferrule config example  # every option, commented
```

### Windows

Native Windows has **no OS sandbox** in ferrule yet: shell commands the
agent runs have your own permissions. Saved keys still stay out of its file
tools and out of the commands' environment. For full isolation, run the
Linux build under [WSL2](https://learn.microsoft.com/windows/wsl/install)
(the `curl … | sh` line, inside WSL).

The agent's shell is Git Bash when [Git for Windows](https://gitforwindows.org)
is installed, else PowerShell, and the agent is told which one it has.
There's no background service on Windows yet; run `ferrule gateway` in a
terminal, or add it to Task Scheduler.

### Build from source

You need Rust 1.85 or newer ([rustup](https://rustup.rs)) and a C compiler
(`cc`/`clang`, or MSVC on Windows) for the bundled SQLite and `ring`. The
Linux sandbox needs kernel 5.13 or newer; on older kernels commands run
unsandboxed, or ferrule refuses to start if `[sandbox] require = true`.

```bash
git clone https://github.com/maximarhipkin/ferrule
cd ferrule
cargo install --locked --path crates/ferrule-cli   # puts `ferrule` in ~/.cargo/bin
ferrule setup
```

## Quick start

```bash
mkdir ~/ferrule-workspace && cd ~/ferrule-workspace
ferrule run "list the files here and summarise the project"
ferrule chat                         # interactive, Ctrl-D to exit
ferrule sandbox                      # what the shell sandbox allows here, tested live
```

The workspace is the directory the agent works in: its file tools stay
inside it, and sandboxed commands can write only there. Keep it apart from
ferrule's own data directory.

| | Linux | macOS | Windows |
|---|---|---|---|
| Config | `~/.config/ferrule/config.toml` | `~/Library/Application Support/ferrule/config.toml` | `%APPDATA%\ferrule\config.toml` |
| Data | `~/.local/share/ferrule/` | `~/Library/Application Support/ferrule/` | `%LOCALAPPDATA%\ferrule\` |

A `ferrule.toml` in the current directory, `--config FILE` or
`$FERRULE_CONFIG` takes the place of the global config.

The data directory holds:

- `memory.db`
- `sessions/` (transcripts)
- `tasks.db`
- `ledger.jsonl`
- `proxy/` (the credential gateway's CA and seed)
- `private/secrets.env` (the keys `ferrule setup` saved)

## Configuration

`ferrule setup` covers the common settings. For the rest, `ferrule config
edit`; `ferrule config example` lists every option. A key can live in the
saved-keys file or in the environment, where `export NAME=…` wins. The
main options:

**A local model** (any OpenAI-compatible endpoint):

```toml
default_provider = "local"

[providers.local]
base_url = "http://localhost:11434/v1"   # Ollama
api_key_env = "OLLAMA_API_KEY"           # any non-empty value
model = "qwen3-coder"
profile = "generic"
```

Pick a provider per run with `--provider NAME`. `run`, `chat` and `gateway`
also take `--workspace DIR` and `--max-iterations N`.

**Telegram:**

```toml
[gateway]
telegram_token_env = "TELEGRAM_BOT_TOKEN"
telegram_allowed_chats = [123456789]   # everyone else is ignored
```

```bash
ferrule gateway          # long-polls Telegram; one session per chat
```

Anyone can find a bot and message it, so only the chats in
`telegram_allowed_chats` reach the agent. While the list is empty, the bot
answers each new chat once with its chat id and forwards nothing.

**Scheduled tasks** run inside `ferrule gateway`, so it has to be running:

```bash
ferrule tasks add morning-brief --kind cron --schedule "0 9 * * *" \
  --timezone Asia/Jerusalem --channel telegram --chat-id 123456789 \
  --prompt "Summarise yesterday's commits in this repo"
ferrule tasks add remind --kind once --schedule 2026-10-01T09:00:00+03:00 \
  --channel local --chat-id local --prompt "Remind me to renew the domain"
ferrule tasks list                    # ids, schedules, next run
ferrule tasks runs <ID>               # truthful status for every run
```

`--gate "script"` runs before the agent wakes. If it prints
`{"wakeAgent": false}`, the run is skipped at zero token cost.

**MCP servers and skills:**

```toml
[[mcp.servers]]
name = "fs"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

Skills are found in `.ferrule/skills`, `.agents/skills` or `.claude/skills`
in the workspace, and in `~/.agents/skills` or `~/.claude/skills`.
`ferrule skills` lists what a workspace would load.

**Cost:** add `price_*_per_mtok` to a provider, then run
`ferrule ledger --since 7d`.

## Sandbox

Every command the agent runs through the `shell` tool is sandboxed:

- **Writes:** only the workspace, temp dirs and any `writable_roots` you
  add.
- **Network:** on by default, off with `network = false` (seccomp).
- **Env:** variables that look like secrets (`*KEY*`, `*TOKEN*`,
  `*SECRET*`…) and every provider's `api_key_env` are stripped.

```bash
ferrule sandbox                       # shows the policy and tests each promise
ferrule sandbox -- sh -c 'touch /etc/x'   # run anything the way the agent would
```

Reads are still open. Keep secrets out of files the agent can read, and use
the credential gateway instead.

## Credential gateway

Agents need tokens: `gh` needs `GITHUB_TOKEN`, and a `curl` to an API
needs its key. Handing the command the real token means a prompt-injected
model can print it or post it anywhere. Ferrule gives the command a
**placeholder** of the same shape instead, and swaps in the real value on
the wire, only for the hosts you name:

<p align="center">
  <img src="docs/assets/credential-gateway.svg" alt="Credential gateway: sandbox → local proxy → real token only to bound hosts" width="860">
</p>

```toml
[secrets]
GITHUB_TOKEN = ["api.github.com", "*.githubusercontent.com"]
# APIs that want the key in the URL need an explicit opt-in:
TELEGRAM_BOT_TOKEN = { hosts = ["api.telegram.org"], in_url = true }
```

```bash
ferrule sandbox -- gh api user              # works; the command never saw the token
ferrule sandbox -- sh -c 'echo $GITHUB_TOKEN'   # ghp_3f9c…, a placeholder
```

How it works:

- **Where the real value goes in.** It's swapped only in the
  `Authorization` header (Bearer or Basic) and credential-named headers
  such as `x-api-key` or `PRIVATE-TOKEN`. The URL is covered only with
  `in_url = true`. Bodies are never touched, so a hijacked model can't get
  a host to store the token in a file name or a message.
- **Where it comes back out.** Responses from those hosts are scrubbed back
  to the placeholder.
- **Other hosts.** Traffic to them goes through a plain tunnel, not
  decrypted. A placeholder sent there is just a useless string.
- **The environment.** On Linux the sandbox also stops commands from
  reading ferrule's own environment, where the real value lives.

Limits:

- Only the shell tool goes through the proxy.
- HTTP/2 and websockets aren't supported on bound hosts.
- Anything the bound host itself can do with the token, the agent can
  too, so scope tokens tightly.

The full design, threat model, prior art (Deno Sandbox, fly.io tokenizer,
Anthropic's sandbox-runtime and others) and the complete list of limits
are in [`docs/research-credential-gateway.md`](docs/research-credential-gateway.md).

## Project layout

```
crates/
  ferrule-core       agent loop, provider trait, harness profiles, compaction, transcripts
  ferrule-providers  OpenAI-compatible driver
  ferrule-tools      fs / shell / web_fetch / diary / memory tools
  ferrule-memory     SQLite + FTS5 memory with time decay
  ferrule-gateway    daemon: channels (Telegram, local), session router, scheduler
  ferrule-mcp        stdio MCP client
  ferrule-skills     Agent Skills discovery and loading
  ferrule-sandbox    OS sandbox for shell commands (Landlock + seccomp / Seatbelt)
  ferrule-proxy      credential gateway: placeholders, TLS-intercepting proxy, scrubbing
  ferrule-cli        the `ferrule` binary
```

## Development

```bash
cargo test --workspace                     # 180 tests
cargo test -p ferrule-proxy -- --ignored   # + a live end-to-end run through the real network
cargo clippy --workspace --all-targets
python3 tests_e2e/setup_wizard.py          # the wizard in a real terminal (Linux, needs pexpect)
python3 tests_e2e/hidden_keys.py           # the agent can't reach the saved keys
```

CI runs the tests on Linux, macOS and Windows. A `v*` tag builds the
release archives for every platform and publishes them with the install
scripts.

Please don't run `cargo fmt` over the whole tree. Format only the files
you touch (`rustfmt --edition 2021 path/to/file.rs`), so diffs stay
reviewable. [`PLAN.md`](PLAN.md) is the shared working log: current
state, open gaps and a dated entry for every session.

## Roadmap

<p align="center">
  <img src="docs/assets/roadmap.svg" alt="Ferrule roadmap" width="860">
</p>

**Shipped**

- [x] M1–M2: gateway daemon, Telegram and local channels, session lanes
- [x] M3: cron and one-shot scheduler with gate scripts
- [x] M4: stdio MCP client
- [x] Phase 0: per-call cost and latency ledger
- [x] M5: Agent Skills
- [x] M6: OS sandbox for the shell tool
- [x] M7: credential gateway
- [x] M8: one-line install and a setup wizard, Windows support, Telegram
      allow-list

**Next** (designed, waiting on a decision)

- [ ] Multi-provider routing, Phase 1: rule-based routing between providers
      by task shape, fed by the ledger
      ([research](docs/research-routing-and-local-models.md))
- [ ] Multi-agent orchestration: planner, implementer and verifier
      subagents with isolated contexts that return summaries only
- [ ] Codex Responses-API and Claude drivers
- [ ] Sandbox the open edges: file reads, and MCP servers (they get the
      real environment today)
- [ ] A sandbox for native Windows (AppContainer or a restricted token)
- [ ] Code-extension plugins

**Planned**

- [ ] Vector recall (local embeddings) merged with BM25
- [ ] Streaming SSE responses
- [ ] WASM tool plugins
- [ ] Tree-sitter semantic code search
- [ ] More channels

To replace a full agent platform such as OpenClaw or NanoClaw, Ferrule
still needs the biggest three: multi-agent orchestration, provider
routing, and a plugin system. They're tracked in [`PLAN.md`](PLAN.md).

## Docs

- [`docs/research-report.md`](docs/research-report.md): why the harness
  matters, and the design behind Ferrule
- [`docs/research-routing-and-local-models.md`](docs/research-routing-and-local-models.md):
  multi-provider routing and local fine-tuning, phases and open decisions
- [`docs/research-credential-gateway.md`](docs/research-credential-gateway.md):
  the credential gateway's design, threat model and limits
- [`PLAN.md`](PLAN.md): current state and session log
