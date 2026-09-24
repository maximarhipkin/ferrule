# The browser

The agent can drive a real Chrome for pages that need JavaScript, a login,
clicks or forms. For static pages and APIs it should keep using
`web_fetch`, which is faster and cheaper; the agent's prompt says so when
the browser is on.

The browser is [agent-browser](https://github.com/vercel-labs/agent-browser)'s
MCP server (`agent-browser mcp`) driving a Chrome or Chromium **that is
already installed**. ferrule looks for one and never downloads a browser.

## Turning it on

1. Install Chrome or Chromium, if there isn't one.
2. Install agent-browser 0.38 or newer: `npm install -g agent-browser`.
3. Run `ferrule setup` → **Browser**. It finds Chrome, starts it once
   headless the way the agent would run it, and turns the browser on only
   if that worked.

`ferrule doctor` shows what it found and why the browser can't run, if it
can't. The agent's tools are `mcp__browser__agent_browser_*`: open, snapshot,
click, fill, type, press, select, scroll, wait, get_text, get_url, get_title,
eval, screenshot, back/forward/reload and tabs.

## Config

```toml
[browser]
enabled = true
chrome = "/usr/bin/google-chrome"      # unset = the first one found
allowed_domains = ["example.com", "*.example.org"]   # empty = any site
chrome_sandbox = true                  # see "Chrome's own sandbox" below
# headed = false                       # show the window
# tools = "core"                       # or "all"
# timeout_secs = 120                   # per tool call
# idle_timeout_secs = 600              # close Chrome after this long idle
# command = "agent-browser"            # a name on PATH or a path
```

Chrome is looked for in the usual places: `google-chrome`, `chromium` and
friends on PATH on Linux, `/Applications` on macOS, Program Files and
LocalAppData on Windows.

## What confines it

- **ferrule's sandbox.** The browser server is an MCP server like any other
  and runs under the same sandbox as the agent's commands (Landlock and
  seccomp on Linux, Seatbelt on macOS), with network on. It can write only
  the workspace and its own state dir under ferrule's data dir
  (`mcp/browser/`), which holds the profile, cache, downloads, screenshots
  and agent-browser's socket. On Linux it may also write `/proc`, because
  Chrome's own sandbox maps its user id through `/proc/self/uid_map`; other
  processes' `/proc` files that matter need ptrace access, which Landlock
  refuses across its boundary. On macOS the browser's Seatbelt profile is
  looser than a command's in one respect: Chrome won't start without the
  system's Mach and XPC services, IOKit and shared memory, so the browser
  may talk to the window server, the pasteboard, the keychain daemon and
  the other per-user services any app can. Its file writes and the hidden
  paths are confined exactly as before, and the model's own commands never
  get this. Windows has no sandbox yet, so there the browser runs
  unconfined, like every MCP server.
- **The credential proxy.** With `[secrets]`, Chrome sends HTTPS through
  ferrule's credential proxy, authenticates to it, and trusts the proxy's
  CA by its key hash (`--ignore-certificate-errors-spki-list`), nothing
  else. A placeholder the agent types into a page or URL becomes the real
  key only on the hosts that key is allowed on, as with every other tool.
  Plain `http://` goes straight out: the proxy speaks CONNECT only. Without
  `[secrets]` there is no proxy and the browser connects directly.
- **Nothing the model can loosen.** Tool arguments that would pick another
  session or profile, add Chrome flags, trust another CA, change the domain
  list or show the window are taken out of the tool schemas, and
  agent-browser reads only ferrule's own empty config file, never an
  `agent-browser.json` in the workspace. That file sits beside the state
  dir (`mcp/browser.agent-browser.json`), not in it: Chrome can write the
  state dir, and a config file can name plugins to run and Chrome flags to
  add. `CI` and `AGENT_BROWSER_*` from
  ferrule's environment aren't passed on.
- **Page text is marked as untrusted.** agent-browser fences it as page
  content, and the prompt tells the model to treat it as data, not
  instructions.
- **`allowed_domains`.** With a list, the browser loads only those hosts.
  agent-browser won't use a saved profile then, so every start is a fresh
  one and logins don't stick. Empty means any site, with the profile kept in
  the state dir.

## Chrome's own sandbox

Chrome has its own sandbox for the processes that render pages. ferrule
keeps it, and never passes `--no-sandbox` on its own. Two cases stop it:

**Root, or a container.** agent-browser turns Chrome's sandbox off by
itself there, so ferrule won't start the browser and `doctor` says why.

**macOS, with ferrule's sandbox on.** macOS doesn't allow a process that is
already under Seatbelt to start a sandbox of its own, and Chrome's sandbox
is Seatbelt, so it can't start inside ferrule's. The choice is between
ferrule's sandbox around the whole browser, with Chrome's off, and Chrome's
sandbox with ferrule's off (`sandbox.mode = "off"`, for everything). The
first is what `setup` offers: it asks you to accept `chrome_sandbox =
false`, and until you do the browser stays off.

**Ubuntu 23.10+ and other AppArmor systems that restrict unprivileged user
namespaces.** Chrome's sandbox needs one. Google's Chrome package ships an
AppArmor profile that allows it; a Chrome or Chromium from elsewhere may
not have one. `doctor` recognises the failure and suggests, in this order:

1. An AppArmor profile for that Chrome which allows user namespaces, e.g.
   `/etc/apparmor.d/chrome-local`:

   ```
   abi <abi/4.0>,
   include <tunables/global>

   profile chrome-local /opt/chrome/chrome flags=(unconfined) {
     userns,
     include if exists <local/chrome-local>
   }
   ```

   then `sudo apparmor_parser -r /etc/apparmor.d/chrome-local`. This is the
   same thing the Chrome package does, for one binary.
2. `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`, which lifts
   the restriction for every program on the machine.
3. Last resort: `chrome_sandbox = false` in `[browser]`. Chrome then runs
   with `--no-sandbox`, still inside ferrule's sandbox and behind the proxy,
   but a bug in Chrome's renderer is no longer contained by Chrome. Use it
   for containers and root, where 1 and 2 don't apply.

## Limits

- Screenshots are saved as files in the state dir; the model doesn't see
  the image itself yet.
- `read` with a URL fetches it with agent-browser's own HTTP client rather
  than Chrome, so Chrome's proxy flags don't apply to it; it gets only the
  proxy environment every sandboxed process gets.
- The profile (cookies, saved logins) is in the state dir, which the
  agent's own commands can read. Anything the agent logs into in the
  browser, its shell can read too.
- With the `all` tool set the model can get Chrome's DevTools address and
  drive Chrome directly. That gives it what Chrome can do, not more: the
  same sandbox and the same proxy.
- Tool schemas come from agent-browser. A newer version could add an
  argument ferrule doesn't know to hide; the tested version is 0.38.
