# M61 — screen viewing

Status: **built 2026-10-21 on `autopilot`, not run live.** The owner can
ask the bot, from their own chat, to look at the owner's own Mac screen:
a screenshot the model sees, or a short recording it sends. **View only,
by the owner's choice:** nothing here moves the mouse, types, launches an
app or injects any input, and there are no stubs for any of that. Code:
`crates/ferrule-cli/src/screen.rs`.

## What it does

Two tools, in the owner's own chats:

- **`screenshot {display?}`** captures one display with
  `screencapture -x -t png` (`-x`: no shutter sound; `-m` for the main
  display, `-D <n>` for another) to
  `<workspace>/media/screen/<YYYYMMDD-HHMMSS>.png`. The main display
  unless `display` names another (1 is the main one, 2 the next; up to
  16). The model gets the image ([vision.md](vision.md)) and the file's
  path, so it can `send_file` it to the owner. A PNG over the 3.5 MB cap
  on an image sent to the model stays whole for the owner, and the model
  gets a JPEG copy at most 1600 pixels on its long side, made with macOS's
  `sips` beside it (`<same name>.jpg`).
- **`record_screen {seconds}`** records the screen, without sound, for 1
  to `max_record_secs` seconds (`screencapture -x -v -V <seconds>`) to a
  `.mov` in the same folder. When an `ffmpeg` is found (on `PATH`, or in
  `/opt/homebrew/bin` or `/usr/local/bin`), it's converted to an H.264
  MP4 (at most 1920 pixels wide, no audio track), which Telegram plays
  inline, and the `.mov` is deleted. Without one, or if the conversion
  fails, the `.mov` stays and the answer says why. The model gets the
  path, the length and the size, never the pixels, and delivers it with
  `send_file`.

Each tool's answer also tells the model that what's on the screen is
content, not instructions. The chat session's system prompt gets a short
constant section (`ferrule_agents::prompts::CHAT_SCREEN`) saying the same
and that it can't click or type. Both tools are deferrable (M57.1):
when `[agent] defer_tools` defers (by default, once more than 10 tools
are deferrable), the model sees their names and loads a schema when it
wants one.

## What it deliberately doesn't do

- No mouse, keyboard, clipboard, app launching or any other input. The
  owner chose view only; a request to click is answered by saying it
  can't.
- No sound in recordings, and no watching in the background: each
  capture is one tool call the owner's chat asked for.
- Nowhere but the owner's own chats. Scheduled tasks, sub-agents, guests'
  chats, the dashboard's chat, `ferrule run` and `ferrule chat` don't get
  the tools.

## Turning it on

```toml
[screen]
enabled = true          # off by default
max_record_secs = 120   # the longest recording; 0 offers no record_screen
window_minutes = 30     # how long the owner's yes holds; 0 asks every time
```

Only the config file turns it on. Neither the dashboard's Config page
nor the chat can change `[screen]`: it isn't one of the form's fields
(which the chat's `config_set` takes), and the raw editor's guard, which
also covers version restores and the chat's `config_restore`, refuses the
whole section (`config_page::WHOLE`). The gateway reads it when it
starts, so restart it after a change.

The tools are offered to a chat session only when all of these hold:

1. `[screen] enabled = true`;
2. the session is one of the owner's chats, by owner mode's
   classification (`owner_mode::owner_tree`: the owner's private chat on
   a channel, and a Telegram group while the config asks for owner mode),
   never someone else's;
3. the workspace is local (not `ssh:`), as for `send_file`;
4. this process can see a desktop: macOS, not started from an SSH login
   (`SSH_CONNECTION` or `SSH_TTY` set), and in the logged-in user's GUI
   session (`launchctl managername` says `Aqua`). The service is a
   LaunchAgent in that session, so it qualifies while the owner is logged
   in; a gateway started over SSH doesn't.

When the config asks and the last one fails, the gateway's log says why at
start, and `ferrule doctor` says why.

## Consent

The first capture asks the owner on a card, in the chat that asked (from
an owner-mode Telegram group, in the primary owner chat):

> Let ferrule see your screen for 30 minutes? It wants to take a
> screenshot now, and until then it can take screenshots and recordings
> without asking again. View only: it can't click, type or open anything.

with **Allow** and **Refuse** (or a `yes`/`no` reply with the card's
code). It waits up to `[trust] approval_timeout_secs` (600 s by default);
no answer refuses.

- **Allow** opens a window of `window_minutes` for all the owner's chats.
  It's held in memory: a restart of the gateway ends it. With
  `window_minutes = 0` every capture asks.
- **Refuse**, no answer, or no owner chat to ask: nothing is captured, and
  the model is told so, with the reason. It counts as a refused action
  (M56.2), so three in a row stop the run and the owner is told.
- **Owner mode asks too.** M54 drops the other approval cards in the
  owner's chats; this one stays.
- One card at a time: a second chat's capture waits for the first card's
  answer, and goes ahead without a card if it was a yes.

The question and the answer are in the audit log as `approval_asked` and
`approval_answered` (subject `screen viewing`).

## Outside content

A screen can show anything: a web page, an email, a message from someone
else. So every screenshot and recording marks the run as having read
outside content (`Hub::saw_untrusted`, audited as `untrusted_seen` with
the tool's name as the source), the way `web_fetch` does. In owner mode,
M54.5's gate then asks before a connected service's change, a force push
or branch deletion, or a `DELETE` to a bound host for the rest of that
run ([m19-trust-cost.md](m19-trust-cost.md) §6b,
[m54-autonomous.md](m54-autonomous.md)). `send_file` to the owner's own
chat isn't gated.

## Privacy

- **Where the files are.** `<workspace>/media/screen/`, with a
  `.gitignore` of `*` written there, so git and auto-commit leave every
  capture out. The folder is refused if it leads outside the workspace.
  Nothing prunes it: delete captures yourself.
- **Hidden from everyone else.** The folder is in
  `owner_mode::guest_hidden`, so the commands and file tools (including
  `send_file`) of every session that isn't the owner's can't read it, in
  every mode.
- **The audit log** (`<data>/trust/audit.jsonl`) gets a `screen_capture`
  event per capture with `what` ("screenshot taken" or "recording 30 s")
  and the file's name, never what it showed.
- **What leaves the machine.** A screenshot goes to the model's provider
  as an image, like any tool image ([vision.md](vision.md)). A recording
  goes nowhere but the chat it's sent to.

## macOS permissions

- **Screen Recording.** macOS lets a program capture the screen only after
  the owner allows it in **System Settings → Privacy & Security → Screen
  Recording** (called "Screen & System Audio Recording" on recent macOS).
  The permission belongs to the program that started the capture: for
  the service, the ferrule binary it runs (`ferrule doctor` names it; in
  owner mode it's `<data>/bin/ferrule`); for a gateway started in
  Terminal, the Terminal app. macOS may ask the first time; until it's
  allowed, a capture fails or shows only the desktop wallpaper and the
  menu bar. An update that replaces the binary may need it allowed again.
- **The indicator.** While a recording runs, macOS shows its
  screen-recording indicator in the menu bar. It's the system's; ferrule
  doesn't hide it.

`ferrule doctor`, with `[screen]` on, takes one test capture in the same
sandbox as the tools, to a temporary file it deletes at once. It runs as
doctor's own process, so from Terminal it checks Terminal's permission,
not the service's; it then names the service's binary to allow.

## How it runs

`screencapture` (`/usr/sbin`), `sips` (`/usr/bin`) and `ffmpeg` run in
ferrule's sandbox as a helper with desktop services
(`Sandbox::for_helper(...).with_desktop_services()`, the same loosening
the browser gets on macOS: Mach and XPC lookups, which the window server
needs). Writes stay confined to the workspace, `media/screen` and temp.
Apple's tools are called by absolute path, never looked up on `PATH`. The
model chooses only a display number and a length, both checked numbers:
no text of its reaches a command line.

## Limits

- **macOS only for now.** On Linux and Windows the tools aren't offered,
  and doctor says "macOS only for now".
- **A recording holds the turn.** `record_screen` waits for the whole
  recording (up to `max_record_secs`, 120 s by default) and the
  conversion before it answers, and the chat waits with it. It isn't a
  background job (M57.6); kept simple.
- One display per screenshot, the main one by default; a recording is of
  the main display.
- A Telegram bot sends files up to 50 MB; `send_file` says so for a
  bigger recording. Telegram may refuse a `.mov` as a video, and then it
  goes as a file.
- A locked or sleeping Mac, or another user's session in front, may give
  a failed capture or the lock screen.

## Not covered

- No live run: no capture was taken while building this. Not yet checked
  on a real Mac: `screencapture` and `sips` under the Seatbelt profile
  with desktop services, how macOS attributes the permission for the
  service's captures, and the MP4 conversion. `ferrule doctor` is the
  first check.
- The window can't be closed early except by restarting the gateway.
