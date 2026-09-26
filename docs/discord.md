# Discord

Ferrule can answer you on Discord as well as, or instead of, Telegram. It
connects out to Discord's Gateway over a WebSocket, so there is no public
URL, no webhook and no port to open. The same daemon runs every channel
that has a token. It's text only: an attachment without text gets "I can
only read text". The design, and the reasons behind it, are in
[m31-channels.md](m31-channels.md).

## Setup

The short way is **`ferrule setup` → Discord**. It walks you through the
steps below, checks the token, registers the slash commands, prints the
invite link and pairs you. By hand:

1. **Create the app.** Open <https://discord.com/developers/applications>
   and click **New Application**. Then, on the **Bot** tab:
   - click **Reset Token** and copy it. That is the **bot token**, three
     dot-separated parts. It's shown once.
   - Under **Privileged Gateway Intents**, turn on **Message Content
     Intent**. It's only needed in server channels, for a reply to the bot
     that doesn't @mention it. DMs, and messages that @mention the bot,
     carry their text without it. With it off, Ferrule keeps working for
     DMs and mentions, and `ferrule doctor` and `/status` say what's lost.
   - Leave **Public Bot** off unless you want others to be able to invite
     it. Nobody who isn't allowed reaches the model either way.
2. **Save the token.** `ferrule setup` stores it sealed in
   `private/secrets.env` (owner-only, hidden from the sandbox), the way it
   stores the Telegram token. By hand, export it and name the variable in
   the config:

   ```toml
   [gateway]
   discord_token_env = "DISCORD_BOT_TOKEN"
   ```

3. **Invite the bot to a server.** Discord only lets you DM a bot you
   share a server with, so even a DM-only setup needs one (a private
   server of your own is fine). The link is:

   ```
   https://discord.com/oauth2/authorize?client_id=<application id>&scope=bot+applications.commands&permissions=274877975552
   ```

   The application id is on the app's **General Information** tab;
   `ferrule setup` fills it in. The permissions are View Channels, Send
   Messages, Send Messages in Threads, Read Message History and Add
   Reactions. Nothing else, so the bot can't moderate, manage or ping
   roles.
4. **Allow yourself** (next section).
5. **Run it:** `ferrule gateway`, or the background service that setup
   offers once any chat channel is on.

## Who gets in

```toml
[gateway]
discord_allowed_users = ["123456789012345678"]    # DMs from these users
discord_allowed_channels = ["234567890123456789"] # server channels (and their threads)
```

- A **DM** reaches the agent only when its author is in
  `discord_allowed_users`.
- In a **server channel** listed in `discord_allowed_channels`, anyone who
  @mentions the bot, or replies to one of its messages, reaches it. A
  thread counts when it or its parent channel is listed. The bot never
  reads the rest of the channel, and never speaks in a channel that isn't
  listed.
- Bots, webhooks and the bot itself never get in.
- **Strangers:** while `discord_allowed_users` is empty, a DM is told once
  "This bot is private. Your Discord user id is …", so you can add it.
  After that, strangers get silence and the log gets one warning per chat
  per hour.

**Pairing.** Setup's **Allow a user** shows a six-digit code and listens
for two minutes. DM the bot exactly that code: you're answered "Paired.
…", and your id is saved. The code is new each run and only works then;
the daemon has no pairing mode. You can also type an id by hand: turn on
**Settings → Advanced → Developer Mode** in Discord, then right-click a
user or channel → **Copy ID**.

**The owner** gets approvals, cap warnings and the restart notice, and may
use the owner commands. On Discord it's `[trust] discord_owner`, else the
first allowed user. With more than one chat channel, `[trust]
owner_channel = "discord"` makes Discord the one that gets them; unset, it's
Telegram, else Discord, else Slack.

## Using it

- **DMs** are one conversation per user. In a server channel, each channel
  or thread is its own conversation, and the bot answers as a Discord
  *reply* to the message that asked.
- Replies are Markdown, which Discord renders. A long one is split at 2000
  characters. They stream as edits when `discord_stream` (unset: `[agent]
  stream`) is on. The bot never pings @everyone or a role.
- 👀 on your message means it was received.
- Approvals come with **Allow** and **Refuse** buttons. Typing `yes`/`no`
  works too.
- **Slash commands:** `/status /stop /resume /model /dashboard /skills
  /undo /plan /caps /mcp /connections`, each with an optional `args`.
  `ferrule setup` registers them (the daemon never does). Global commands
  can take a while to appear in the client. Typed commands always work:
  `/status` in a DM, `@Ferrule /status` in a channel.

## Health

- `ferrule doctor` reads, and changes nothing. It checks the bot
  (`/users/@me`), the Gateway and its identify budget (`/gateway/bot`),
  and whether the Message Content intent is on (`/applications/@me`).
- `/status`, `ferrule status`, the heartbeat, the watchdog and the
  dashboard show `discord` with its last good frame. A dead or stale
  socket shows there, and so does a stop. A rejected token (401 / close
  4004) stops Discord only; the other channels keep running.
- The redactor hides the token, and anything shaped like one, in
  `/status`, logs and the heartbeat. The sandbox scrubs `*TOKEN*`
  variables from commands. For a skill that must call Discord from the
  shell, bind it in `[secrets]`: `DISCORD_BOT_TOKEN = ["discord.com"]`.

## Live test

The hermetic tests run against a mock Discord. One test talks to the real
one, so it's ignored by default:

```sh
FERRULE_LIVE_DISCORD_TOKEN=<bot token> \
FERRULE_LIVE_DISCORD_USER=<your user id> \
cargo test -p ferrule-gateway --test discord -- --ignored discord_live_round_trip
```

It DMs you "ferrule live test: reply to this DM" and waits two minutes for
your reply.
