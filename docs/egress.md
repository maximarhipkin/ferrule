# Egress: what the agent can reach

Ferrule's credential proxy also enforces a **network policy**: which hosts the
model's tools may reach, and which local Unix sockets a sandboxed command may
connect to. This guide covers the `[egress]` table, what a refusal looks like,
the Unix-socket allowlist, and what each OS enforces. The design and its
as-built notes are in [m33-ops.md](m33-ops.md) §1. The sandbox itself is in
[sandbox.md](sandbox.md).

| | Linux | macOS | Windows |
|---|---|---|---|
| `web_fetch`, `web_search`, MCP over HTTP, plugins | enforced (the proxy) | enforced | enforced |
| Shell commands that honour `HTTPS_PROXY` | enforced once there are rules or secrets (or `enforce`) | same | same |
| Shell commands that ignore the proxy | **not stopped** (advisory); refused with `enforce = true` | same; refused with `enforce = true` | **not stopped**; `enforce` is refused |
| Where requests went | the audit log (`ferrule egress log`) | same | same |
| Unix-socket allowlist | enforced (seccomp user notification) | Seatbelt profile (checked by the macOS CI test, not by hand) | not implemented |

## The default: public yes, private no

With no `[egress]` table, tools reach any public host, and **private
destinations are refused**:

- loopback (`127.0.0.0/8`, `::1`) for ferrule's own tools;
- the LAN: `10/8`, `172.16/12`, `192.168/16`, `fc00::/7`;
- CGNAT `100.64/10` (Tailscale too), link-local `169.254/16`, `fe80::/10`;
- the cloud metadata addresses `169.254.169.254` and `fd00:ec2::254`;
- the reserved, multicast and broadcast ranges.

That is the server-side request forgery class: a page that tells the model to
fetch `http://169.254.169.254/latest/meta-data/iam/…` gets refused, not the
instance's credentials. Mapped and NAT64 forms (`::ffff:127.0.0.1`,
`64:ff9b::7f00:1`) and odd spellings (`0x7f.1`, `2130706433`) are judged by
the address they mean.

Shell commands may still reach loopback: the dev server a command just started
is the common case, and a command can reach `localhost` directly anyway.

**Your own endpoints stay reachable.** Every server your config names is let
through on its host *and port*, whatever the private guard says: each
`[providers.*] base_url` (Ollama on `localhost:11434`, llama.cpp on `:8080`),
`[memory]` embedding URLs, MCP servers' `url`s, the web search backend and the
`[telemetry]` endpoint. An explicit `deny` still wins over them.

## `[egress]`

```toml
[egress]
default = "allow"        # "deny": only what `allow` lists goes
allow = []               # host patterns, IPs or CIDRs
deny = []                # always wins, over allow and your own endpoints
private = "block"        # "allow" turns the private-range guard off
private_allow = []       # private hosts or ranges reachable anyway
```

**Rules** use the `[secrets]` host syntax: `api.github.com`,
`*.githubusercontent.com` (a leading `*.` is the only wildcard, and it needs
two labels after it). IP literals (`203.0.113.7`, `2001:db8::1`) and CIDRs
(`10.0.0.0/8`) work too, and an optional `:port` narrows an entry to one port.

They are checked in this order; the first match decides:

1. `deny` matches the host name or the address it resolved to: refused.
2. The address is private, `private = "block"`, and neither `private_allow`
   nor one of your endpoints covers it: refused.
3. `default = "deny"` and nothing in `allow` matches: refused.
4. Otherwise it goes.

Under `default = "deny"` a bare IP needs its own entry: otherwise a host
allowlist is bypassed by resolving the name yourself.

**The metadata address** opens only with its exact IP in `private_allow`
(`"169.254.169.254"`). `private = "allow"` or a `169.254.0.0/16` entry
doesn't open it.

Some starting points:

```toml
# A NAS and a LAN Ollama, nothing else private
[egress]
private_allow = ["nas.local", "192.168.1.20:11434"]

# Only the package registries and GitHub
[egress]
default = "deny"
allow = ["github.com", "*.github.com", "*.githubusercontent.com",
         "pypi.org", "files.pythonhosted.org", "registry.npmjs.org",
         "crates.io", "*.crates.io"]

# Never this one, whatever else says
[egress]
deny = ["*.pastebin.com"]
```

`ferrule setup` → **Network policy** writes the first two for you ("open" and
"package hosts only", which also seeds GitLab, RubyGems and Maven Central).

## DNS: resolved once

The proxy resolves the name itself, checks **every** address it got back, and
connects to one of those addresses, never to the name again. A name that
resolves to a public and a private address at once is refused, since whoever
runs its DNS picks the order. TLS still verifies the certificate against the
name.

**Behind a corporate proxy** (`HTTPS_PROXY` set for ferrule itself), the
upstream proxy connects, not ferrule. Ferrule resolves the name locally only to
check it, and sends the name on. Two consequences:

- a name that resolves differently on the upstream (split-horizon DNS) is
  judged by the local answer;
- when the name doesn't resolve locally at all, which is common when only the
  corporate proxy can resolve, the request is **allowed** and noted in the
  debug log. IP literals are always checked.

## Which traffic it covers

- **`web_fetch`, HTTP `web_search` backends, MCP servers reached by URL, M32
  plugins:** always through the proxy, always checked, and loopback counts as
  private for them.
- **Shell commands:** they get `HTTP_PROXY`/`HTTPS_PROXY` when a `[secrets]`
  entry is live *or* `[egress]` has rules (`default = "deny"`, or any
  `allow`, `deny` or `private_allow` entry). With neither, nothing changes
  from before M33: commands connect directly.
- **Stdio MCP servers** get the same proxy variables as a command.
- **Model providers aren't proxied.** Their keys are ferrule's, not the
  model's.
- **Ferrule's own updates aren't either**: the hosts they reach are in
  [updates.md](updates.md#network).

**The proxy is advisory for shell commands, unless you bind them.** `curl`,
`pip`, `npm`, `git` and most HTTP clients honour `HTTPS_PROXY`; a program that
ignores it (or `ssh`, a database client, anything over UDP) connects directly
while `network = true`. `[egress] enforce = true` stops that (next section);
`[sandbox] network = false` stops every connection.

## Binding commands to the proxy (`enforce`)

M54.4. Opt-in, off by default:

```toml
[egress]
enforce = true
```

With it, a sandboxed command's IP connections can reach **only the
proxy's port on `127.0.0.1`**, and Unix sockets only from the
[allowlist](#unix-sockets). Everything a command sends out goes through the
proxy, where the rest of `[egress]` applies and every destination is logged.
The policy itself stays allow-by-default: `enforce` doesn't refuse any host,
it only removes the way around the proxy. Commands always get the proxy's
settings (`HTTPS_PROXY` and the CA bundle) while it's on, and the shell
tool's description tells the model that direct connections are refused.

**How each OS binds them:**

- **macOS:** two Seatbelt rules appended to the profile: `(deny
  network-outbound (remote ip "*:*"))`, then `(allow network-outbound
  (remote ip "localhost:<port>"))`. Unix sockets keep their own rules, so
  name lookups through mDNSResponder still work.
- **Linux:** the connect supervisor that enforces the Unix-socket allowlist
  (seccomp user notification) also checks every IP `connect(2)`: only
  `127.0.0.0/8` or `::1` on the proxy's port goes, and ferrule makes that
  connection itself with the address it checked. The seccomp filter refuses
  every socket but Unix ones and IPv4/IPv6 streams (so no UDP, raw or packet
  sockets), and `MSG_FASTOPEN` on `sendto`, `sendmsg` and `sendmmsg`, which
  would open a TCP connection without a `connect`. It needs what the
  supervisor needs (Linux 5.6+, `pidfd_getfd`; see [When it can't
  run](#when-it-cant-run)). Landlock's network rules (ABI 4) aren't used:
  they name a port, not an address, so any host on the proxy's port would
  pass, and they don't cover UDP. This code is compiled and tested by
  Linux CI only.
- **Windows:** refused. The config doesn't load with `enforce = true`, and
  says why: the restricted-token sandbox can't limit a command's
  connections to one port.

**When it can't be applied** (no sandbox, `mode = "off"`, or on Linux a
supervisor that can't run, as inside most Docker containers), every
command is **refused** with the reason, rather than run unbound. `ferrule
doctor`'s `egress` line and `ferrule sandbox` (`egress   bound: …`) say
which.

**Not bound:** a helper whose config turns its sandbox off (`sandbox =
false` on an MCP server; in owner mode it keeps the sandbox, and so the
binding), and the browser: Chrome and agent-browser talk to
each other over a local debugging port, and Chrome is pointed at the proxy
already. Ferrule's own requests (models, channels, updates) aren't
commands and aren't bound; SSH workspaces run `ssh` from ferrule itself, so
they keep working. Listening on a port and accepting connections stay as
they were.

### What breaks under `enforce`, and how to allow it

| What | Why | What to do |
|---|---|---|
| `ssh`, `scp`, `rsync` over ssh | a direct TCP connection | tunnel it through the proxy (below), or use HTTPS |
| `git` over ssh (`git@github.com:…`) | ssh | an `https://` remote, with a token in `[secrets]` bound to the host |
| Database clients over TCP (`psql -h db.example.com`, `redis-cli`) | direct TCP | a Unix socket on the allowlist (local PostgreSQL and MySQL sockets are in the defaults), or a tunnel through the proxy |
| A local server on TCP (`curl localhost:3000`, a test suite's own server) | only the proxy's port is reachable on loopback | a Unix socket, or run that check outside ferrule |
| DNS lookups over UDP (Linux), `ping`, anything UDP or ICMP | no UDP or raw sockets | HTTP clients don't need them behind a proxy: the proxy resolves the name |
| Programs that ignore `HTTPS_PROXY` | they connect directly | set their own proxy option to the value of `HTTPS_PROXY` |

**A tunnel through the proxy.** The proxy accepts `CONNECT host:port` for
any port, checks it against `[egress]` like any request, logs it, and
passes the bytes through. `HTTPS_PROXY` holds its address and credentials
(`http://ferrule:<token>@127.0.0.1:<port>`), so a client that can use an
HTTP proxy with basic auth works. For ssh, a `ProxyCommand` that speaks
`CONNECT`, for example with socat:

```sh
p=${HTTPS_PROXY#http://}; auth=${p%@*}; hp=${p#*@}
ssh -o ProxyCommand="socat - PROXY:${hp%:*}:%h:%p,proxyport=${hp##*:},proxyauth=$auth" user@host
```

There is no per-host exception for direct connections by design: an
exception would be a way around the proxy and its log. To run without the
binding, set `enforce = false` (the default).

**Still open:** name lookups through the system resolver (mDNSResponder on
macOS; nscd or systemd-resolved's socket on Linux, which are on the default
socket allowlist) still happen, so a command can leak a little data in DNS
queries. The Linux binding and the Seatbelt rules haven't been run on a
real machine yet (no test was added for them); `ferrule sandbox -- curl -sS
https://example.com` (goes) and `ferrule sandbox -- nc -z example.com 22`
(refused) are the first checks.

## Where requests went: the audit log

M54.4. In every mode, each destination the proxy lets a request go to is
written to the trust audit log (`<data>/trust/audit.jsonl`) as an `egress`
event: who asked (`command` for a sandboxed command or helper, `tool` for
ferrule's own clients acting for the model), the method (`CONNECT`, or the
plain-HTTP method), the host and the port. **Never** a path, a query, a
header or a body, and never a key. A destination is written at most once a
minute per source, host and port, so a busy client doesn't flood the log.

```bash
ferrule egress log                # the last 24 h, by host
ferrule egress log --since 7d
ferrule trust audit --since 1h    # every event, refusals (egress_denied) included
```

`ferrule egress log` lists each `host:port` with who asked, how many times
it was logged and refused, and when last. `ferrule doctor` counts the hosts
reached in the last 24 h.

What the log covers is what goes through the proxy: ferrule's own tools for
the model always, and commands when they are proxied (`[secrets]`, `[egress]`
rules or `enforce`). Without `enforce`, a command that ignores
`HTTPS_PROXY` connects directly and isn't in the log.

## What a refusal looks like

**To the model:** a `403` with the header `x-ferrule-egress: denied` and a
body that explains itself:

```
ferrule egress policy: blocked https://169.254.169.254/ (private address 169.254.169.254: cloud metadata).
This is the owner's network policy, not a network error; retrying won't help.
If this host is needed, ask the owner to add it to [egress] private_allow (see docs/egress.md).
```

For HTTPS, the proxy accepts the `CONNECT`, terminates TLS with its own CA and
answers the request inside the tunnel with that 403, so `curl` prints the text
instead of `CONNECT tunnel failed`. Every ferrule tool and every proxied
command trusts that CA; a client that doesn't gets a TLS error.

`web_fetch` fails the call with that text. Since M33 it also marks any other
non-2xx page: the text starts with `HTTP <status>` rather than passing a 404
page off as the content.

**To you:**

- a ledger row, `call_kind = "egress_denied"`, with the host, the reason
  (`denied_rule`, `private_address`, `not_allowed`) and who asked (`ferrule` for a command,
  `ferrule-tool` for the model's tools). Only the host and port are kept,
  never the path or query. Cost and call counts skip these rows;
- a trust audit event `egress_denied`: `ferrule trust audit --since 24h`;
- an "egress refused" line on the dashboard overview, with the top hosts;
- `ferrule doctor`'s `egress` section: the policy, what it applies to, and the
  last 24 h of refusals.

A retry loop doesn't flood them: one report per (source, host, reason) per
10 seconds. Every request is still refused.

To open a host: add it to `allow` (under `default = "deny"`), to
`private_allow` (a private address), or remove the `deny` entry that matched.
The body names which one.

## Unix sockets

A sandboxed command can connect to a Unix socket even with `network = false`,
and some sockets are a way out of any sandbox: `/var/run/docker.sock` (`docker
run -v /:/host …`), podman and containerd, the systemd user bus
(`systemd-run --user`), your tmux server, an editor's server socket. So only
an allowlist of sockets is reachable:

```toml
[sandbox]
unix_sockets = ["/var/run/docker.sock"]   # added to the defaults
unix_sockets_default = true               # false drops the defaults
# unix_sockets = ["*"]                    # turns the allowlist off
```

An entry is a path, a directory (trailing `/`), `~/…`, or an abstract socket
as `@name`.

**The defaults:** `$SSH_AUTH_SOCK` (git over ssh), gpg-agent's sockets
(`~/.gnupg/`, `$GNUPGHOME/`, `/run/user/<uid>/gnupg/`), the name-service
helpers (nscd, `/run/systemd/resolve/`, `/run/systemd/userdb/`), journald and
`/dev/log`, and the PostgreSQL and MySQL socket directories. On macOS:
mDNSResponder, syslog, launchd's per-user directories, `/private/tmp/mysql.sock`
and `/private/tmp/.s.PGSQL.*`.

**Not in them:** Docker, podman, containerd, `/run/systemd/private`, the system
and session D-Bus, and **X11**, since an X connection can type into any other
window, a terminal included. `unix_sockets = ["/tmp/.X11-unix/"]` puts it back.

**Sockets the command made itself** are allowed without a listing. On Linux,
ferrule asks the socket who is listening, and allows it when that process
belongs to the command. A test suite's local server or a dev server's socket
just works; your tmux server in the same `/tmp` doesn't. Two limits: a
datagram socket has no listener to ask, so it needs a listing, and a socket
with more than one hard link is only allowed by its exact path (a hard link to
`docker.sock` in an allowed directory doesn't ride in on the directory). On
macOS, Seatbelt can't ask, so sockets under the workspace are allowed by path.

### How each OS does it

- **Linux:** a seccomp filter hands every `connect(2)` to a supervisor thread
  in ferrule. It copies the command's socket, resolves the path against the
  command's own cwd and root, opens it without following it past the real
  inode, checks that inode's real path against the allowlist, and **connects
  the socket itself**. What was checked is what gets connected, so symlinks,
  renames, `..` and a second thread rewriting the address change nothing.
  `io_uring` is refused, since it can connect around seccomp. Datagram
  `sendto` with a destination path isn't covered; none of the escape sockets
  are datagram sockets.
- **macOS:** `network = false` already denies every socket. With the network
  on, the Seatbelt profile denies outbound connections to any path and allows
  the listed ones. IP traffic is untouched. The macOS CI enforcement test runs it on
  a real Mac (a listed socket connects, an unlisted one and a symlink to it
  get `EPERM`); it hasn't been tried by hand.
- **Windows:** not implemented. Docker Desktop listens on a named pipe, and
  the restricted token ferrule runs commands under can't open it (its DACL
  grants `docker-users`, which a restricted token holds deny-only), but that
  isn't claimed as a guarantee.

### When it can't run

The supervisor needs `pidfd_getfd` (Linux 5.6) and seccomp user notification.
**Docker's default seccomp profile refuses `pidfd_getfd`** without
`CAP_SYS_PTRACE`, so inside most containers the allowlist can't run. Ferrule
checks once per process, by connecting to one allowed and one refused socket
under the filter. When the check fails:

- `ferrule doctor` and `ferrule sandbox` warn: `Unix sockets: not enforced
  (<why>)`;
- commands run without the allowlist, unless `[sandbox] require = true`, in
  which case they fail and the message names `unix_sockets = ["*"]` as the way
  to run without it.

`ferrule sandbox` prints the state as `sockets`, and doctor as
`Unix sockets:`:

- `allowlist (N entries, plus sockets the command makes itself)`;
- `not enforced (<why>)`;
- `none (network off)` (macOS);
- `any (unix_sockets = ["*"])`;
- on Windows, a note: not covered.

## Testing it

```bash
ferrule doctor                       # the egress and Unix sockets lines
ferrule sandbox                      # includes the sockets state
ferrule sandbox -- curl -sS http://169.254.169.254/        # the 403 text (with rules or secrets set)
ferrule sandbox -- curl --unix-socket /var/run/docker.sock http://x/_ping   # refused on Linux and macOS
```

With `[egress] enforce = true`, `ferrule sandbox` shows `egress    bound:
IP connections only to the proxy (127.0.0.1:<port>)`, or why it can't be
applied.

The tests: `cargo test -p ferrule-proxy --test it policy::` (the policy, DNS,
denial pages, the source split) and `cargo test -p ferrule-sandbox --test
enforcement` (real Unix sockets: allowed, refused, a symlink, an abstract
socket, a socket the command made). The socket tests skip where the
supervisor can't run, except on CI (`GITHUB_ACTIONS` set), where they fail
instead, so a broken supervisor can't pass as "not supported here".
