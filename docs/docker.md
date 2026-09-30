# Running ferrule in a container

One bot per container, run by a panel (or by you). The container is the
isolation boundary, the bot's whole state is one volume, and the panel
updates a bot by swapping the image tag. Design and threat model:
[m44-managed-mode.md](m44-managed-mode.md).

Two images, from one `Dockerfile` at the repo root:

| Tag | What's in it |
|---|---|
| `ghcr.io/maximarhipkin/ferrule:<version>` (and `:latest`) | ferrule, a real userland (bash, coreutils, git, curl), `tini` |
| `ghcr.io/maximarhipkin/ferrule:<version>-browser` (and `:latest-browser`) | the above, Chromium, fonts and agent-browser |

The image runs as user 10001, keeps everything in `/data`, and starts with
`FERRULE_MANAGED=1`: [managed mode](m44-managed-mode.md#3-managed-mode). A
person running it by hand can pass `-e FERRULE_MANAGED=0`.

## Run one bot

```sh
docker run -d --name b1 \
  --restart unless-stopped \
  --stop-timeout 30 \
  --memory 512m --cpus 1 --pids-limit 512 \
  --security-opt no-new-privileges --cap-drop ALL \
  --read-only --tmpfs /tmp \
  -v bot1:/data \
  -v /etc/ferrule/b1.toml:/etc/ferrule/policy.toml:ro \
  -e FERRULE_POLICY=/etc/ferrule/policy.toml \
  -e FERRULE_BOT_ID=b_4f2a \
  -e FERRULE_PANEL_SECRET="$(cat /etc/ferrule/b1.secret)" \
  -e FERRULE_PUBLIC_URL=https://bots.example.com/b/b_4f2a/ \
  -p 127.0.0.1:8080:8080 \
  ghcr.io/maximarhipkin/ferrule:latest
```

- **`--stop-timeout 30`**: Docker's default is 10 s, and a stopping bot
  gives running turns 20 s, then 5 s more to wind down (see
  [Health, busy and stopping](#health-busy-and-stopping)).
- **`--memory`, `--cpus`, `--pids-limit`**: the numbers are yours. Idle, a
  bot took about 24 MiB (see [Measured](#measured)); a turn that runs
  commands or builds a large context needs more.
- **`--read-only --tmpfs /tmp`**: the image writes only to `/data` and
  `/tmp`. The bot's `HOME` is `/data/home`.
- **`-p 127.0.0.1:8080:8080`**: the dashboard and `/healthz` (a reverse
  proxy reaches it there). Add `-p 127.0.0.1:8788:8788` for the HTTP API.
  Never publish either on a public address without a proxy in front.

The first start writes a starter config into the volume and runs with **no
channel and no model**: it serves the dashboard and waits to be set up. The
user opens the page from the panel (see [Panel sign-in](#panel-sign-in)),
adds a model key and a Telegram bot, and the gateway restarts itself in
place.

## Try it locally

No panel needed:

```sh
docker run -d --name b1 -v bot1:/data -p 8080:8080 \
  -e FERRULE_PUBLIC_URL=http://localhost:8080/ \
  -e FERRULE_POLICY=/etc/ferrule/policy.toml \
  -v "$PWD/policy.toml":/etc/ferrule/policy.toml:ro \
  ghcr.io/maximarhipkin/ferrule:latest
docker exec b1 ferrule dashboard link      # a one-use sign-in link
```

Without `FERRULE_PUBLIC_URL` the page refuses everything except `/healthz`
and `/busyz`, and says so. `FERRULE_MANAGED=1` needs no policy file to run,
but then nothing is locked.

## Environment

What the image reads. All are optional unless it says otherwise.

| Variable | Meaning | Image default |
|---|---|---|
| `FERRULE_MANAGED` | `1` turns managed mode on, `0` off even if the config says on | `1` |
| `FERRULE_POLICY` | path of the policy file; **set but missing or unreadable stops the bot** | unset |
| `FERRULE_BOT_ID` | this bot's id; the panel's sign-in token must name it | unset |
| `FERRULE_PANEL_SECRET` | the key panel sign-in tokens are signed with, at least 32 bytes; one per bot | unset (panel sign-in off) |
| `FERRULE_PUBLIC_URL` | the URL the page is opened at, path prefix included | unset |
| `FERRULE_DASHBOARD_BIND` | address the page listens on | `0.0.0.0` |
| `FERRULE_DASHBOARD_PORT` | port the page listens on | `8080` |
| `FERRULE_HTTP_BIND` | address the HTTP API listens on | `0.0.0.0` |
| `FERRULE_DATA_DIR` | where all state lives | `/data` |
| `FERRULE_CONFIG` | the config file; a missing one is created | `/data/ferrule.toml` |
| `HOME` | the bot's home | `/data/home` |
| `FERRULE_BROWSER` | `-browser` image: enables the browser in the starter config | `1` there |
| `FERRULE_BROWSER_CHROME_SANDBOX` | `0` runs Chrome with `--no-sandbox` inside ferrule's sandbox; `1` uses Chrome's own | `0` in `-browser` |
| `CHROME_PATH` | the Chromium to drive | `/usr/bin/chromium` in `-browser` |

The bind, port and public URL override the config at every load, so a
panel can move a bot without editing the user's file. Model keys and bot
tokens are the user's: they are saved on the dashboard into
`<data>/secrets.env`, not passed as env.

The image's port for the HTTP API is `8788` (`[gateway.http] port`).

## The policy file

Written by the panel, mounted read-only, **outside `/data`**. Every key is
optional; a missing key allows. Read once at start (a change comes with a
container restart). An unknown key is an error, so a misspelt `shel = false`
can't leave the shell on.

```toml
reason = "Closed beta: set by the Ferrule panel."   # shown next to every lock

shell = true            # the shell tool, hooks, verify_command, the linter, task gates
browser = false         # the browser tools
extensions = false      # installing MCP servers, plugins, skills; self-extension
sandbox = "os"          # "os": no OS sandbox means no shell; "container": the container is enough
providers = ["openai", "anthropic", "openrouter", "chatgpt"]   # unset: any but the Claude plan
max_usd_per_day = 5.0   # caps: the lower of this and the config's wins
max_usd_per_run = 1.0
max_tokens_per_day = 2000000
max_turn_minutes = 30
channels = ["telegram", "http"]   # which channels may be set up; unset: any
```

Two examples. **Where Landlock works** (Docker's default seccomp profile
included, see the table below):

```toml
reason = "Closed beta"
sandbox = "os"
browser = true
extensions = false
providers = ["openai", "anthropic", "openrouter"]
max_usd_per_day = 5.0
```

**Where it doesn't** (gVisor, a kernel without Landlock): the container is
the boundary, and the policy says so; without that line the `shell` tool is
off:

```toml
reason = "Closed beta"
sandbox = "container"
extensions = false
max_usd_per_day = 5.0
```

The Claude plan is refused in managed mode whatever the policy says: users
bring their own model access.

## The sandbox inside a container

Ferrule proves its sandbox works by running a command under it at start.
What managed mode does with the result:

- **It starts:** commands run under it.
- **It doesn't, and the policy says `sandbox = "container"`:** commands run
  in the container without ferrule's OS sandbox (the env scrubbing and the
  deny list still apply).
- **It doesn't, and the policy doesn't say that:** the `shell` tool, hooks
  and the browser are off. Nothing runs unsandboxed silently. `/healthz`
  is `degraded` with the reason.

What each runtime gives. Only the first row was **measured**, in the
container this milestone was built in (Docker's default seccomp profile,
kernel 7.0). The rest is **from the documentation and not verified**: there
was no Docker daemon to try them on.

| Runtime | Landlock | seccomp filter | Unix-socket allowlist | Chrome's own sandbox | Policy |
|---|---|---|---|---|---|
| Docker, default seccomp profile (**measured**) | works (ABI 8) | works | **not enforced**: the supervisor needs `CAP_SYS_PTRACE` for `pidfd_getfd` | no: user namespaces are blocked | `sandbox = "os"` |
| Docker, `--security-opt seccomp=unconfined` (not verified) | expected to work | expected to work | expected to work with `pidfd_getfd` allowed; not verified | expected to work; not verified | `"os"`. This drops a layer of protection; prefer a custom profile |
| Docker with a custom profile that allows `CLONE_NEWUSER` / `pidfd_getfd` (not verified) | expected to work | expected to work | expected to work | expected to work | `"os"`, and `FERRULE_BROWSER_CHROME_SANDBOX=1` |
| gVisor, `--runtime=runsc` (not verified) | **not implemented**, as documented by gVisor | its own syscall table decides | its own | its own | `"container"` |
| Rootless Podman (not verified) | depends on the host kernel; user namespaces exist | as Docker | as its capabilities decide | can work: the container has a user namespace | `"os"` if the start-up test passes, else `"container"` |
| A kernel without Landlock (< 5.13, or not in `lsm=`) | no | works | as the row for its runtime | as the row for its runtime | `"container"` |

`ferrule doctor` names what protects commands in one line, and any gap.

**Chrome.** Under Docker's default profile Chrome's own sandbox can't start,
so the `-browser` image runs Chrome with `--no-sandbox` **inside ferrule's
Landlock sandbox**. In this container agent-browser itself adds
`--no-sandbox` whenever it sees a container, so the flag made no difference
to a real run (both settings passed the end-to-end browser test).

**Never mount the Docker socket** (`/var/run/docker.sock`) into a bot. The
Unix-socket allowlist isn't enforced under the default profile, so a
sandboxed command could connect to any socket it can reach, and the Docker
socket is root on the host.

## Behind a proxy

The page and the HTTP API are plain HTTP. Put TLS and the panel's routing in
front of them. An nginx location for one bot:

```nginx
location /b/b_4f2a/ {
    proxy_pass         http://127.0.0.1:8080;      # keeps the prefix
    proxy_http_version 1.1;
    proxy_set_header   Host              $http_host;
    proxy_set_header   X-Forwarded-Proto $scheme;
}
```

- **The prefix**: the page accepts a request with or without
  `/b/b_4f2a/`, so a proxy that strips it (`proxy_pass http://…/;`) works
  too. `FERRULE_PUBLIC_URL` must name the URL the browser opens, prefix
  included; it adds that Host to the allow-list, is the Origin every POST
  must carry, and is the base path.
- **`Host` must be passed on** (`$http_host`, with the port if any).
- **`/healthz` and `/busyz`** answer on any Host, without signing in. Block
  them from the public side if you don't want strangers to see that a bot
  exists, and let the panel reach them on the container's own address.
- **Subdomains are recommended** (`b_4f2a.bots.example.com`). Bots under one
  domain with path prefixes share an origin: cookies are scoped by path,
  but a page of one bot could script another's in the same browser.

## Panel sign-in

The panel signs a short-lived token and sends the browser to
`<public_url>#<token>`; the page posts it to `/api/login`.

```
ferrule-panel.v1.<b64url(claims JSON)>.<b64url(HMAC-SHA256(secret, "ferrule-panel.v1." + b64url(claims)))>
claims = {"bot": "b_4f2a", "user": "u_91", "exp": 1790000000, "nonce": "<16 or more characters>"}
```

Base64url is without padding. `exp` is at most 5 minutes ahead (plus 30 s of
skew). Each nonce works once. In Python:

```python
import base64, hashlib, hmac, json, secrets, time

def b64(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()

def panel_token(secret: bytes, bot: str, user: str, ttl: int = 120) -> str:
    claims = json.dumps(
        {"bot": bot, "user": user, "exp": int(time.time()) + ttl,
         "nonce": secrets.token_urlsafe(16)},
        separators=(",", ":"),
    ).encode()
    body = "ferrule-panel.v1." + b64(claims)
    sig = hmac.new(secret, body.encode(), hashlib.sha256).digest()
    return f"{body}.{b64(sig)}"

url = "https://bots.example.com/b/b_4f2a/#" + panel_token(SECRET, "b_4f2a", "u_91")
```

**One secret per bot**, for example `HMAC(panel_master, bot_id)`. Under
`sandbox = "container"` (no Landlock) a command runs as the same user as the
gateway and can read `/proc/1/environ`, which holds the container's env. A
secret shared across bots would then sign in to every one of them. Ferrule
takes the secret out of its own environment before any child starts.

Every failure is a 401 with the same words; the log has the reason.

## Health, busy and stopping

- **`GET /healthz`** on the page's port, on any Host, without signing in:
  `{"status": "ok" | "degraded" | "failing", "reasons": […], "version",
  "managed", "busy", "turns", "queued", "uptime_secs"}`. `failing` is a 503
  (the dispatch loop is stuck, or every channel has a problem); `degraded`
  (no model or channel yet, a channel problem, the shell off because the
  sandbox didn't start) is a 200. Reasons never contain a secret.
  The image's `HEALTHCHECK` runs `ferrule health --probe`, which asks it on
  127.0.0.1 and fails on `failing` or no answer. Running `ferrule health`
  yourself prints it.
- **`GET /busyz`**: `200 {"busy": false}` when nothing runs, `409` with the
  counts when a turn runs or a message is queued. The panel should ask
  before it swaps an image. It's advice, not a lock.
- **`docker stop`** sends SIGTERM. The bot stops its channels and the
  scheduler, tells any chat with a queued message to send it again, lets
  running turns finish for `[gateway] stop_grace_secs` (default 20), stops
  the ones still running (their chat is told), waits up to 5 s for that, and
  exits 0. A second signal exits at once. **`--stop-timeout 30`** covers the
  20 s and the 5 s.

## How a change is picked up

| Change | Picked up |
|---|---|
| A model key, the default model | at once (the models registry and secrets are followed) |
| A Telegram token, an allowed chat, the HTTP API's on/off or port | after a gateway restart: **channels are built once at start** |
| `FERRULE_PUBLIC_URL`, the bind or the port | after a container restart (the panel replaces the container with the new env) |
| The policy file | after a container restart: it's read once |
| The image (a new version) | the panel swaps the tag and restarts; `/data` is kept |

The restart the dashboard offers after a save is a **clean re-exec** of the
same binary: same PID (so the runtime keeps watching it), about a second,
and `secrets.env` is read again, so a token just saved is in the environment
it starts with. No container restart is needed. It was chosen over adding a
channel to a running gateway because a live add would be a new path through
the router, the health monitor and the owner doors, while the restart path
already exists and is tested.

## Signal

The Signal channel needs a `signal-cli` daemon, and the image has no
`signal-cli` (nor Java) to start one. Run the daemon elsewhere and point
`[gateway.signal] url` at it ([channels.md](channels.md#signal)).

## Backup and restore

From the running container onto a mounted directory:

```sh
docker run -d … -v /srv/backup:/backup …            # the mount, when the bot is made
docker exec b1 ferrule backup -o /backup/b1.tar.gz
```

It reads `FERRULE_DATA_DIR` and `FERRULE_CONFIG`; a config inside the data
dir is stored once. Restore into a fresh volume: the image's entrypoint is
`ferrule`, so `restore …` is the command:

```sh
docker run --rm -v newvol:/data -v /srv/backup:/backup:ro \
  ghcr.io/maximarhipkin/ferrule:latest restore /backup/b1.tar.gz
```

A restore into an empty `/data` is done in place; into a data dir that has
something in it, it keeps the old one as a `.pre-restore-…` copy first.
The tests cover both.

## Measured

Dev container: 4 vCPU, kernel 7.0.0, Docker's default seccomp profile. The
script is `scripts/m44-measure.sh`; details and caveats in
[m44-managed-mode.md §9.2](m44-managed-mode.md#92-measurements-part-6-2026-09-30).

| | |
|---|---|
| Idle, Telegram polling and the HTTP API listening | **24.0 MiB** RSS |
| One turn that runs a shell command (mock model), peak | **26 MiB** |
| Release binary (glibc) | 33.2 MiB |
| Bots per 16 GiB, idle (1.5 GiB kept back, 8 MiB per container) | **462** |
| The same with a turn running in 10% of them | 459 |
| Browser: a real page open, everything under the bot, summed RSS | 1.36 GiB (an over-count: shared pages are counted per process) |
| Browser: 12 s after the last call, Chrome closed | 65 MiB (agent-browser's daemon and node) |

**Image sizes: estimated, not built.** No Docker daemon was available, and
the static musl binary couldn't be built there either (no `musl-gcc` for the
`ring` crate), so these use the glibc binary and an assumed 0.3
compression ratio for packages:

- plain: about **80 MiB** compressed, about 245 MiB on disk;
- `-browser`: about **330 MiB** compressed, about 1.0 GiB on disk (Chromium
  and what it needs is 722 MiB installed).
