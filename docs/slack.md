# Slack

Ferrule can answer you on Slack as well as, or instead of, Telegram. It
uses **Socket Mode**: it connects out to Slack over a WebSocket, so there is
no public URL, no request URL and no port to open. The same daemon runs
every channel that has a token. It's text only: a file without text gets
"I can only read text". The design, and the reasons behind it, are in
[m31-channels.md](m31-channels.md).

## Setup

The short way is **`ferrule setup` → Slack**. It checks both tokens, says
which one is wrong, and pairs you. By hand:

1. **Create the app from a manifest.** Open
   <https://api.slack.com/apps> → **Create New App** → **From a manifest**,
   pick the workspace, and paste:

   ```yaml
   display_information:
     name: Ferrule
   features:
     app_home:
       messages_tab_enabled: true
       messages_tab_read_only_enabled: false
     bot_user:
       display_name: Ferrule
       always_online: true
     slash_commands:
       - command: /ferrule
         description: Talk to Ferrule's gateway
         usage_hint: status | stop | resume | model | dashboard | undo | plan …
         should_escape: false
   oauth_config:
     scopes:
       bot:
         - app_mentions:read
         - chat:write
         - commands
         - im:history
         - im:read
         - im:write
         - reactions:write
   settings:
     event_subscriptions:
       bot_events:
         - app_mention
         - message.im
     interactivity:
       is_enabled: true
     socket_mode_enabled: true
     org_deploy_enabled: false
     token_rotation_enabled: false
   ```

2. **Install it:** **Install App** → **Install to Workspace**. Copy the
   **Bot User OAuth Token** (`xoxb-…`).
3. **Make the app-level token:** **Basic Information** → **App-Level
   Tokens** → **Generate Token and Scopes**, with the scope
   `connections:write`. Copy it (`xapp-…`). Socket Mode connects with it.
4. **Save both.** `ferrule setup` stores them sealed in
   `private/secrets.env` (owner-only, hidden from the sandbox), the way it
   stores the Telegram token. By hand, export them and name the variables:

   ```toml
   [gateway]
   slack_bot_token_env = "SLACK_BOT_TOKEN"   # xoxb-
   slack_app_token_env = "SLACK_APP_TOKEN"   # xapp-
   ```

   Slack runs only with both. Setup and doctor catch a swapped pair.
5. **Allow yourself** (next section), then run `ferrule gateway`, or the
   background service that setup offers once any chat channel is on.

If you add a scope later, reinstall the app: the token only gets the
scopes it was installed with.

## Who gets in

```toml
[gateway]
slack_allowed_users = ["U0123ABCD"]     # DMs from these members
slack_allowed_channels = ["C0123ABCD"]  # channels where an @mention reaches it
```

- A **DM** reaches the agent only when its author is in
  `slack_allowed_users`.
- In a channel listed in `slack_allowed_channels`, anyone who @mentions
  the bot reaches it. Invite the bot to the channel first (`/invite
  @Ferrule`). It answers in a thread under the mention, and each thread is
  its own conversation. It never reads the rest of the channel, and never
  speaks in a channel that isn't listed.
- Bots and the bot itself never get in.
- **Strangers:** while `slack_allowed_users` is empty, a DM is told once
  "This bot is private. Your Slack user id is …", so you can add it.
  After that, strangers get silence and the log gets one warning per chat
  per hour.

**Pairing.** Setup's **Allow a user** shows a six-digit code and listens
for two minutes. DM the bot exactly that code (App Home → **Messages**):
you're answered "Paired. …", and your id is saved. The code is new each run
and only works then; the daemon has no pairing mode. To type an id by hand:
open a profile → **⋯** → **Copy member ID** (`U…`). A channel's id is at the
bottom of its **About** tab (`C…`).

**The owner** gets approvals, cap warnings and the restart notice, and may
use the owner commands. On Slack it's `[trust] slack_owner`, else the first
allowed user. With more than one chat channel, `[trust] owner_channel =
"slack"` makes Slack the one that gets them; unset, it's Telegram, else
Discord, else Slack.

## Using it

- Replies are converted from Markdown to Slack's mrkdwn (bold, italics,
  strikethrough, links, headings as bold, bullets). Code is left alone.
  They stream as edits when `slack_stream` (unset: `[agent] stream`) is on,
  at most one edit every 1.5 s.
- 👀 (`eyes`) on your message means it was received.
- Approvals come with **Allow** and **Refuse** buttons. Typing `yes`/`no`
  works too.
- **The slash command** is `/ferrule <command> [args]`, e.g. `/ferrule
  status` or `/ferrule model use …`. Slack command names are
  workspace-wide, so there's one command rather than a bare `/status`.
  From a listed channel it's answered there; from anywhere else it's
  answered in your DM, if you're allowed. Typed commands work too: `/status`
  isn't usable as text in Slack, so write `@Ferrule /status` in a channel,
  or use `/ferrule status`.

## Health

- `ferrule doctor` reads, and changes nothing. `auth.test` checks the bot
  token, names the workspace and bot, and compares the token's scopes with
  the list above, naming any that are missing. `apps.connections.open`
  checks the app token and that Socket Mode is on (the URL it returns is
  thrown away).
- `/status`, `ferrule status`, the heartbeat, the watchdog and the
  dashboard show `slack` with its last good frame. A dead or stale socket
  shows there, and so does Socket Mode being turned off
  (`link_disabled`). A rejected token stops Slack only; the other channels
  keep running.
- The redactor hides both tokens, and anything shaped like a Slack token
  (`xox…-`, `xapp-`), in `/status`, logs and the heartbeat. The sandbox
  scrubs `*TOKEN*` variables from commands. For a skill that must call
  Slack from the shell, bind it in `[secrets]`: `SLACK_BOT_TOKEN =
  ["slack.com"]`.

## Live test

The hermetic tests run against a mock Slack. One test talks to the real
one, so it's ignored by default:

```sh
FERRULE_LIVE_SLACK_BOT_TOKEN=xoxb-… \
FERRULE_LIVE_SLACK_APP_TOKEN=xapp-… \
FERRULE_LIVE_SLACK_USER=U0123ABCD \
cargo test -p ferrule-gateway --test slack -- --ignored slack_live_round_trip
```

It DMs you "ferrule live test: reply to this DM" and waits two minutes for
your reply.
