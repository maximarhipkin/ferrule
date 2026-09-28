# M39 — more channels: WhatsApp, Matrix, email, Signal, Mattermost, an HTTP API

**Status:** design, 2026-09-28 (branch `m39-channels`). User guide:
[`channels.md`](channels.md).

Ferrule talks through Telegram (M2), Discord and Slack (M31). Max, 27.09:
"add more channels". M39 adds six more and holds each one to M31's standard:
- the owner is recognized;
- strangers never reach the model;
- a dead connection is never silent;
- approvals, the 👀 receipt and the owner's commands all work;
- the tokens never reach the model.

On top of that standard, every new channel gets:
- a `ferrule setup` step;
- a dashboard card (icon, status, key form with **Test**, a guide with direct links);
- a `ferrule doctor` line;
- approvals in the channel;
- files and images in and out where the platform allows;
- hermetic tests against a mock on 127.0.0.1.

The order below is the build order. Each channel ships whole before the
next starts.

| # | Channel | Reaches ferrule by | Approvals | Files | Streaming |
|---|---|---|---|---|---|
| 1 | WhatsApp (Cloud API) | the M20 relay Worker's webhook mailbox, or a local listener behind your own tunnel | reply buttons | in and out | no (no edits) |
| 2 | Matrix | `/sync` long-poll | 👍/👎 reactions or a reply | in and out | yes (`m.replace`) |
| 3 | Email | IMAP IDLE, polling fallback; SMTP out | a reply keyword | in and out | no |
| 4 | Signal | `signal-cli` daemon, JSON-RPC + SSE | a reply keyword | in and out | no |
| 5 | Mattermost | WebSocket + REST | 👍/👎 reactions or a reply | in and out | yes (post patch) |
| 6 | HTTP API | `POST /v1/messages` on 127.0.0.1 | an answer message | out as links (§8) | yes (SSE) |

Teams, Google Chat, Twilio SMS and Zulip are follow-ups (§12). Each gets a
paragraph there on what it needs.

## 0. What stays as it is

- **Telegram, Discord and Slack don't change.** Every existing test passes
  as it is. Strings, audit fields and session ids stay the same.
- **One daemon, one funnel, one lane per chat.** A new channel is another
  `Channel` on `Gateway::run`'s funnel. Sessions are
  `session_id(channel, chat)`, so `whatsapp__972501234567` resumes from its
  JSONL transcript like `telegram__42` does.
- **The agent reads text.** M39 doesn't add vision. It does start saving what
  people send (§2): the agent gets the file's path and can open it with its
  tools. That applies only to the new channels. Telegram, Discord and Slack
  keep M31's "I can only read text" reply, and moving them over is a
  follow-up.
- **M38 instances.** Everything a channel keeps belongs to one instance:
  - its config (`[gateway.<channel>]` in that instance's `ferrule.toml`);
  - its secrets (that instance's `secrets.env`);
  - its state (`<data>/gateway/<channel>/…`);
  - its relay mailbox (that instance's Worker, `ferrule-relay-<name>`).

  Nothing is shared between instances. §10 lists the collision checks.

## 1. Shape

```
crates/ferrule-gateway/src/channels/
  files.rs       inbound files → <workspace>/inbox/<channel>/…, the note the agent reads
  hmac.rs        HMAC-SHA256 + hex/constant-time compare on ring (no new crate)
  whatsapp/      WhatsAppChannel: Graph API sends, the mailbox/listener inbound, 24h window
  matrix/        MatrixChannel: /sync, sends, edits, reactions, media
  email/         EmailChannel: IMAP IDLE loop, SMTP sends, threading, loop guards
  signal/        SignalChannel: signal-cli daemon (spawned or given), JSON-RPC + SSE
  mattermost/    MattermostChannel: WebSocket events + REST v4
  http_api/      HttpApiChannel: the local server, keys, SSE, outbox, webhooks
crates/ferrule-gateway/src/tools/send_file.rs   the agent's send_file tool (§2)
crates/ferrule-cli/src/channels/                config → channels, doctor lines, setup steps, cards
crates/ferrule-cli/src/dashboard/channels.rs    the cards' API
relay/worker.js                                 + the WhatsApp mailbox routes (§3.2)
```

**No new crates** except what the lockfile already has. There's no `hmac`
or `sha2`, and `ring` does HMAC-SHA256 already. There are no mail crates
either, because M37 hand-rolled IMAP, SMTP and MIME in
`ferrule-connections::native` over the same rustls stack.
`ferrule-gateway` takes `ferrule-connections` as a dependency to reuse them.
That creates no cycle: connections doesn't depend on the gateway. IMAP gets
`IDLE`, `UID FETCH`/`STORE` and `APPEND`-free flag handling added there.

**Config.** Each channel is a sub-table, `[gateway.whatsapp]`,
`[gateway.matrix]` and so on. M31's flat `slack_*` keys would have meant
about 60 more fields on `GatewayConfig`. A sub-table that's present
switches its channel on. Token values never go in the config: each
sub-table names env vars, the way `slack_bot_token_env` does, and setup
writes their values to `secrets.env`.

**Owners.** `[trust]` gets `whatsapp_owner`, `matrix_owner`,
`email_owner`, `signal_owner`, `mattermost_owner` and `http_owner`.
`OWNER_CHANNELS` grows to nine, and `channel_title` gets the proper names
("WhatsApp", "HTTP API"). Every place M31 hard-coded three channels moves
to one table, `CHANNELS: &[ChannelInfo]`, that has for each channel:
- its name and title;
- its owner key and allow-list keys;
- its secret env names;
- whether it polls;
- its doctor/setup/card hooks.

Those places are:
- `trust::route_for`
- `health::owner`
- `setup::forget_secret`
- `setup/channels::add_user`
- `ConnectionsDoor::actor`
- the dashboard's doctor fixes
- `sandbox_policy`
- the redactor

A new channel then can't be half-wired silently.

## 2. Files in and out (channel-neutral)

**In.** An adapter that receives a file downloads it with its own
credentials, capped at `max_file_mb` (default 20). `files::save` writes it
to `<workspace>/inbox/<channel>/<yyyy-mm-dd>/<id>-<safe name>`. Names are
sanitized, and nothing ever lands outside `inbox/`. The agent's text gets
one line per file:

`[The sender attached a photo: inbox/whatsapp/2026-09-28/wamid-3f…-IMG_0042.jpg (image/jpeg, 184 KB). Open it with your tools if you need it; you can't see images.]`

A file that is too big, or whose download fails, is named and not saved,
and the sender is told why in words. The inbox is the workspace's, so the
agent's file tools, sandbox and hidden-file rules apply unchanged.

**Out.** There is a new tool, `send_file {path, caption?}`, registered only
in chat sessions: the gateway factory knows the session's channel and chat
from the router. It resolves `path` inside the workspace, and hidden files
are refused. It then sends an `OutboundMessage` with one
`Attachment{kind, url: <local path>, name}` to the session's own chat. It
can't send anywhere else: there is no `to` argument. A channel without
attachments answers the tool with "this channel can't send files", which
covers Telegram, Discord and Slack for now. The approval gate treats
`send_file` like `write_file`, since it sends a workspace file out.

## 3. WhatsApp (Cloud API)

### 3.1 What the owner needs (the guide links each)
1. A Meta developer app of type **Business** with the WhatsApp product:
   <https://developers.facebook.com/apps/>.
2. The **phone number id**, from the app's WhatsApp → API Setup page. This
   is not the phone number itself.
3. A **permanent token**: a system user in Business Settings → Users →
   System users (<https://business.facebook.com/settings/system-users>),
   with `whatsapp_business_messaging` and `whatsapp_business_management`
   and the app assigned. The 24-hour test token from API Setup works for a
   day, and doctor says so when it expires (error 190).
4. The **app secret**: App settings → Basic.
5. The webhook, which the setup step prints in full:
   - a callback URL;
   - a verify token;
   - "subscribe to `messages`".

   Paste them in the app's WhatsApp → Configuration page.

Sending needs no public URL; receiving does. There are two ways in.

### 3.2 Inbound: the relay mailbox (default) or your own tunnel

**Decision: extend the M20 relay Worker into a webhook mailbox.** The owner
already deploys it for M37, per instance, with one click from the
dashboard. It has a Durable Object with strong consistency. Re-deploying it
adds the new routes and bindings, so "the M37 relay deploy covers WhatsApp".

New Worker routes:

| Route | Who calls it | Auth | What it does |
|---|---|---|---|
| `GET /wa/<box>` | Meta, once | `hub.verify_token` == `WA_VERIFY_TOKEN` | returns `hub.challenge` as text, 403 otherwise |
| `POST /wa/<box>` | Meta, per event | `X-Hub-Signature-256` == HMAC-SHA256(`WA_APP_SECRET`, raw body) | stores `{seq, body, sig}`, 200. 401 on a bad or missing signature, 413 over 256 KiB |
| `POST /wa/<box>/take` | ferrule | `Bearer RELAY_KEY` | `{after: n}` deletes every event with `seq ≤ n` and returns the rest (≤ 100) |

- `<box>` is `b64url(SHA-256("wa:" + RELAY_KEY))`, so only someone who
  holds the relay key can compute it, and a leaked mailbox URL doesn't
  reveal the key.
- The verify token and app secret are Worker secrets (`secret_text`
  bindings, like `RELAY_KEY`). The deploy adds them when the instance has
  WhatsApp set up. **The app secret is on Cloudflare.** That's what lets
  the Worker drop forged posts before they take space.
- ferrule verifies every body's HMAC again with its own copy of the secret
  before reading it. A Worker that stored something bad still can't inject
  a message.
- Storage is one Durable Object, `Mailbox`, keyed by `<box>`. Events are
  deleted as soon as ferrule's next `take` acknowledges them. Anything
  never taken expires after **24 hours**, by alarm. The mailbox holds at
  most 1000 events or 5 MiB, and above that a post gets 503, so Meta
  retries it later (Meta retries a failed webhook for up to 7 days).
- ferrule polls `take` every 3 s. That's about 29 k requests a day, well
  inside the Workers free plan's 100 k. The M20 OAuth slot routes don't
  change.

**What the relay sees, said plainly in channels.md:** for up to 24 hours,
and normally for the ~3 s until ferrule takes them, Cloudflare holds the
webhook bodies in plaintext. Those bodies contain:
- message text;
- sender numbers and profile names;
- media *ids*. The media itself stays on Meta, and fetching it needs the
  token, which only ferrule has.

The Worker also holds the app secret and the verify token. It never holds
the WhatsApp access token. Someone who controls the Cloudflare account can
read messages and forge webhook events. They can't send as the business
number.

**Alternative: `inbound = "listen"`.** ferrule serves the same `GET`/`POST`
contract on `127.0.0.1:<listen_port>` (default 8787), and the owner points
their own named tunnel or reverse proxy at it. ferrule verifies the
challenge and the HMAC itself, and nothing sits in between. A **named**
tunnel is needed, because a quick `trycloudflare.com` URL changes at every
restart and Meta's callback URL is fixed. Ferrule doesn't manage the named
tunnel: it needs the owner's domain and Cloudflare login, so
channels.md links Cloudflare's guide. The mock tests use this mode and the
mailbox, both.

### 3.3 Sending
- The base URL is `https://graph.facebook.com/<api_version>`, where
  `api_version` defaults to `v23.0` and is configurable (Meta retires a
  version about two years after release). Requests go to `POST
  /<phone_number_id>/messages` with `Authorization: Bearer <token>`.
- **Text:** `type: text`, 4096 characters. A longer text is split on
  paragraph and line boundaries by the shared `stream::chunks`. Markdown
  is mapped to WhatsApp's `*bold*`, `_italic_`, `~strike~` and
  `` ```mono``` ``.
- **Buttons (approvals):**
  - `type: interactive`, `interactive.type: button`, up to **3** reply
    buttons.
  - A title is ≤ 20 characters (longer ones are cut with "…"), and the id
    is the command (≤ 256).
  - A press arrives as `interactive.button_reply.id`, which becomes the
    inbound text, so `yes a1b2` reaches the OwnerDoor exactly as typed.
  - More than 3 buttons, or any URL button, falls back to text.
  - The interactive body is ≤ 1024 characters: a longer approval text goes
    as a text message first, then a short body with the buttons.
- **👀:** `type: reaction` on the inbound message id.
- **Read receipts:** `{"status":"read","message_id":…}` when the turn
  starts. The sender sees blue ticks once the message is in ferrule's hands,
  not merely received.
- **Files out:**
  - `POST /<phone_number_id>/media` (multipart: `file`, `type`,
    `messaging_product`) returns an id.
  - The file then goes as `image`, `video`, `audio` or `document`, with
    the caption on image, video and document.
  - Meta's size limits, checked before upload: image 5 MB, audio/video
    16 MB, document 100 MB.
- **Files in:** `GET /<media-id>` gives `{url, mime_type, file_size}`, then
  `GET url` with the same bearer token. The token never goes anywhere else.
- **No edits, so no streaming.** `capabilities().edits = false`, so the
  router sends the finished answer.

### 3.4 The 24-hour window (never silent)
WhatsApp accepts a free-form message only within 24 hours of the user's
last message to the business. Ferrule tracks each chat's last inbound time
in `<data>/gateway/whatsapp/windows.json`. On a send:
- **Window open:** send.
- **Window closed, and a template is configured** (`template = "name"`,
  `template_language = "en_US"`, a *utility* template whose body has one
  `{{1}}`):
  - Ferrule sends the template, with `{{1}}` = the first 200 characters of
    the text, flattened to one line (template parameters refuse newlines).
  - It keeps the full text in the chat's **held** queue (≤ 20 messages,
    ≤ 7 days).
  - The next inbound message reopens the window. The held messages go out
    first, headed "Held while WhatsApp's 24-hour window was closed:".
- **Window closed, and no template:**
  - The message is held the same way, and `send` returns an error:
    "WhatsApp's 24-hour window for this chat is closed; the message is held
    until they write again (set a template to reach them first)".
  - The caller logs it. The router/notifier puts it in `/status` via
    `problem()`.
  - Doctor warns while anything is held.
  - If the chat is the owner's, the notice goes to the owner's next
    channel.
- **Error `131047` on a send, or a `failed` status webhook with `131047`:**
  - The window is marked closed.
  - The message is held and treated as above.
  - Nothing is dropped silently.

Status webhooks (`statuses[]`) are read for `failed` only. Each failure is
logged with Meta's code and title. The first failure of each code in an
hour goes to `problem()`.

### 3.5 Rate limits and errors
| Code | Meaning | What ferrule does |
|---|---|---|
| 130429 | throughput limit | back off 2 s, 4 s, 8 s (3 tries), then `RateLimited` |
| 131056 | pair rate limit (too many to one number) | wait 6 s and retry, 3 tries |
| 131047 | 24 h window | §3.4 |
| 131030 | recipient not in the test number's allowed list | error: "add them under API Setup → To, or use a real number" |
| 190 | token expired or invalid | `problem()` = "the WhatsApp token was refused (expired?)", doctor fails |
| 131026 | undeliverable | logged, error to caller |

Sends to one chat are paced at ≥ 1 s apart, which keeps a split long answer
under the pair limit.

### 3.6 Who gets in
DMs only: Cloud API numbers can't be added to groups. `allowed_users`
holds `wa_id` digits (`972501234567`). The stranger rule, pairing by code
and the owner's `whatsapp_owner` are M31's `Access`, unchanged. A stranger
is told nothing, even during setup. Replying to a stranger opens a
conversation that Meta bills, so "Tell" is off for WhatsApp.

## 4. Matrix

- **Login:** an access token (`access_token_env`), or `user` + a
  `password_env`.
  - With a password, ferrule calls `POST /_matrix/client/v3/login`
    (`m.login.password`, device name "ferrule") once and keeps the returned
    token and device id in `<data>/gateway/matrix/session.json`, 0600.
  - Setup turns a password into a token and stores only the token.
- **Receiving:** `GET /_matrix/client/v3/sync?timeout=30000&since=…`, with
  a filter that asks only for room timelines and invites.
  - The first sync (no `since`) is a catch-up: its events are skipped, so
    a restart never answers old messages.
  - `next_batch` is kept in the state file after each successful sync.
  - 429 `M_LIMIT_EXCEEDED` honours `retry_after_ms`. Other errors back off
    1 → 60 s.
  - `polls() = true`, and `last_ok_poll` is the last good sync.
- **Rooms and DMs:** a room with two joined members is a DM, and its chat
  id is the room id.
  - DMs follow the user allowlist (`@max:example.org`).
  - Other rooms need the room in `allowed_rooms` **and** a mention: the
    bot's user id in `m.mentions.user_ids`, or its id or display name in
    the body. The mention is stripped.
  - Invites are joined only when the inviter is allow-listed. Everything
    else is left pending and logged once an hour.
- **Sending:**
  - `PUT /rooms/{room}/send/m.room.message/{txn}` with `m.text`. The body
    is plain Markdown, plus `format: org.matrix.custom.html` rendered by a
    small Markdown→HTML pass (bold, italic, code, links, lists).
  - The limit is 16 000 characters per event (the event cap is 64 KiB).
  - **Edits** (`m.replace` with `m.new_content`) make streaming work, as on
    Discord.
  - **👀** is an `m.reaction` annotation. **Read receipts** are
    `POST /rooms/{room}/receipt/m.read/{event}`.
- **Approvals:**
  - The approval message lists its answers as text ("reply `yes a1b2`").
  - Ferrule also reacts to it with 👍 and 👎, so one tap on either sends
    that answer.
  - The mapping `event id → {👍: "yes …", 👎: "no …"}` lives in memory for
    the approval's life, and a reaction from anyone but an allowed user is
    ignored.
- **Files:**
  - Uploads go through `POST /_matrix/media/v3/upload` and are sent as
    `m.image`/`m.file`.
  - Downloads use authenticated media first,
    `GET /_matrix/client/v1/media/download/{server}/{id}`. If that returns
    404 or `M_UNRECOGNIZED` from an older server, ferrule falls back to
    `/_matrix/media/v3/download`.
- **E2EE: not implemented, and said so.**
  - Ferrule has no Olm/Megolm. A room with `m.room.encryption` in its
    state is **refused**. When ferrule meets one (joining, or the first
    message), it posts one plaintext notice there, once per room: "This
    room is end-to-end encrypted, and ferrule can't read encrypted
    messages. Make a room with encryption off (Element: Room settings →
    Security → Encrypted, before the first message) and invite me there."
  - `m.room.encrypted` events are never read.
  - Doctor lists the encrypted rooms it knows of.
  - Element turns encryption on by default for new DMs, so channels.md
    says how to make an unencrypted one.
  - Why not implement it: `vodozemac` + device verification + key backup
    is a milestone on its own. A half-working E2EE that silently can't
    decrypt some messages is worse than a clear refusal.

## 5. Email

- **Receiving:** IMAP over TLS (993) or STARTTLS (143).
  - `SELECT INBOX`.
  - At start, remember `UIDNEXT`: nothing older is answered.
  - Then `UID SEARCH UNSEEN UID <last+1>:*`, fetch, handle, `UID STORE
    +FLAGS (\Seen)`.
  - Between rounds: `IDLE` when the server has the capability, with
    re-IDLE every 25 minutes (RFC 2177 says servers may drop at 30).
    Without it, `NOOP` + search every `poll_secs` (default 60).
  - The last UID and `UIDVALIDITY` are kept in the state file. A changed
    `UIDVALIDITY` resets to `UIDNEXT`, never replays.
- **Sending:** SMTP with implicit TLS (465) or STARTTLS (587), `AUTH PLAIN`.
- **Credentials, one ask:**
  - `use_connection = "gmail"` reuses the M37 Gmail connection's address
    and app password from the sealed connections store.
  - Otherwise: `address`, `imap_host`, `smtp_host`, ports, `username`
    (defaults to `address`) and `password_env`.
  - Gmail's defaults are filled in when the address is `@gmail.com` or
    `@googlemail.com`.
- **Threading:**
  - A chat is one **sender address**, so the session follows the person.
  - Replies set `In-Reply-To` to the message's `Message-ID`, and
    `References` to its `References` plus its `Message-ID`. The subject is
    `Re: <subject>`, without piling up `Re: Re:`.
  - A new message from ferrule (a notice, a task result) starts a thread
    with the subject "ferrule: <first line>".
- **Who gets in:**
  - `allowed_senders` holds addresses and `@domain` entries.
  - `email_owner` is an address.
  - The `From:` address is checked. Spoofing it takes nothing more than
    typing it, so the check also demands the receiving server's own
    `Authentication-Results` to show `dmarc=pass` for the From domain, or
    `spf=pass` with a matching header-from domain, when
    `require_auth_results = true`. That is the default for Gmail, which
    always adds the header.
  - A stranger is silently dropped. Answering unknown mail invites spam
    and backscatter.
- **Loops and lists (never auto-reply):** no reply at all, not even the 👀
  equivalent, when any of these is true:
  - `Auto-Submitted` is anything but `no`.
  - There is a `List-Id`, `List-Unsubscribe` or `List-Post` header.
  - `Precedence` is `bulk`, `list` or `junk`.
  - `Return-Path: <>` is empty.
  - The sender is `MAILER-DAEMON@`, `postmaster@` or `noreply`/`no-reply`
    local parts.
  - There is an `X-Autoreply`, `X-Autorespond` or
    `X-Auto-Response-Suppress: All` header.
  - The message is ferrule's own, marked by its `Message-ID` domain or by
    an `X-Ferrule-Loop` header.

  Ferrule marks its own mail `Auto-Submitted: auto-replied` and
  `X-Ferrule-Loop: <instance>`. A per-sender budget (10 replies an hour)
  stops a loop no header catches.
- **Bodies:**
  - `text/plain` is preferred. Otherwise `text/html` goes through M37's
    `html_to_text`.
  - Quoted history is cut at the first `On … wrote:` line or `>` block,
    and at `-- ` signatures.
  - Approvals are the reply keyword (`yes a1b2`), the first line of the
    reply.
- **Files:**
  - Attachments in are saved (§2).
  - Attachments out go as `multipart/mixed` with base64 parts, built by
    M37's `mime::build`, extended with parts.
- **No 👀, no streaming.** Email has no reactions. The answer is one mail.

## 6. Signal

- **Needs** Java 21+ and `signal-cli` ≥ 0.13, with a number that is
  registered or linked as a secondary device. Ferrule doesn't vendor or
  download them.
  - Setup and doctor detect them: `signal-cli --version` on PATH or at
    `signal_cli`, and `java -version`.
  - When they're missing, setup and doctor say exactly what to install,
    with links to <https://github.com/AsamK/signal-cli> (releases, and the
    wiki's linking guide).
- **Two modes:**
  - `url = "http://127.0.0.1:7583"` connects to a daemon the owner runs.
  - Otherwise ferrule **spawns** `signal-cli -a <account> daemon --http
    127.0.0.1:<port> --receive-mode on-start` as a child. The child is
    killed with the gateway and restarted with backoff if it dies. Its
    stderr goes to `<data>/gateway/signal/daemon.log`.
- **Receiving:** SSE `GET /api/v1/events`.
  - A `receive` notification's `envelope.dataMessage` gives:
    - `message`: the text;
    - `groupInfo.groupId`: a group chat;
    - `attachments[]`: ids of files in signal-cli's attachments folder,
      which ferrule copies into the inbox;
    - `mentions[]`: the addressing check in groups.
  - `polls() = true`, and `last_ok_poll` is the last event or keepalive.
- **Sending:** JSON-RPC `POST /api/v1/rpc`.
  - `send` takes `recipient` or `groupId`, `message`, and `attachments` as
    local paths.
  - `sendReaction` is 👀. `sendReceipt` with `type: read` is the read
    receipt. `sendTyping` runs while the turn runs.
- **Chats:**
  - A DM's chat id is the sender's number, or their ACI uuid when the
    number is hidden. `allowed_users` accepts both.
  - Groups need `allowed_groups` and a mention of the account.
- **Approvals** are the reply keyword. Signal has no bot buttons.
- **No edits, so no streaming.** Signal has edits, but signal-cli's edit
  support is recent and clients show them oddly, so they stay off.

## 7. Mattermost

- **Needs:** the server URL and a **bot account's token** (System Console →
  Integrations → Bot Accounts), or a personal access token.
- **Receiving:** WebSocket `wss://<server>/api/v4/websocket`.
  - It first sends `authentication_challenge` with the token, then reads
    `posted` events. The post comes as a JSON string in `data.post`.
    `data.channel_type` is `D` for a DM, and `O`, `P` or `G` otherwise.
    `data.mentions` lists who was mentioned.
  - Keepalive: a ping every 30 s. Reconnects back off 1 → 60 s.
  - `polls() = true`.
- **Chats:**
  - The chat id is the Mattermost channel id.
  - DMs follow `allowed_users` (user ids; usernames are resolved once at
    start).
  - Other channels need `allowed_channels` plus a mention (`@botname`),
    which is stripped.
- **Sending:**
  - `POST /api/v4/posts`. Threads use `root_id`, so a reply in a thread
    stays there.
  - The limit is 16 383 characters. **Streaming** works by
    `PUT /posts/{id}/patch`.
  - 👀 is `POST /api/v4/reactions` (`eyes`).
  - A DM to the owner goes to the channel from
    `POST /api/v4/channels/direct`.
- **Approvals:**
  - Mattermost's interactive buttons call a URL on *ferrule*, which a
    local gateway doesn't have.
  - So Mattermost works like Matrix: the answers are listed as text, and
    ferrule reacts `+1`/`-1` on the approval post. The `reaction_added`
    event from an allowed user sends that answer.
- **Files:**
  - Out: `POST /api/v4/files` (multipart, `channel_id`), then `file_ids`
    on the post.
  - In: `GET /api/v4/files/{id}` with the token.
- **429:** honours `X-Ratelimit-Reset`.

## 8. HTTP API

For programs (scripts, shortcuts, other services) rather than people.

- **Off by default. When on, it binds `127.0.0.1:<port>` (default 8788)
  only.** `public = "tunnel"` opens M20's quick tunnel to it and prints the
  URL in `/status` and on the card. Nothing else ever binds a non-loopback
  address.
- **Keys:**
  - A client is `{name, key hash, created, last_used, webhook?}` in
    `<data>/gateway/http/clients.json` (0600).
  - A key is `frk_<43 b64url chars>` (32 random bytes). It is shown
    **once**, at creation, and stored only as its SHA-256.
  - Keys are created, listed and revoked from the dashboard card or with
    `ferrule channels http keys add|list|revoke <name>`.
  - A revoked key fails at once. The gateway re-reads the file when it
    changes.
- **`POST /v1/messages`** (`Authorization: Bearer frk_…`,
  `{"text": "...", "conversation": "optional id"}`):
  - The chat id is `<client>` or `<client>/<conversation>`.
  - The default response waits for the answer:
    `{"id","conversation","text","files":[]}` (up to the turn limit).
  - With `Accept: text/event-stream` it's **SSE**: `event: delta` lines
    while the answer grows (the channel supports edits, and each edit is a
    delta), then `event: done` with the full message.
  - The router's reply carries `reply_to` = the request's message id,
    which is how the answer finds its request.
  - Files the agent sends come as `{"name","url"}`. The `url` is
    `/v1/files/<token>`, valid for an hour, and served with the client's
    key.
- **`GET /v1/events?after=<n>`** is the chat's **outbox**: everything
  ferrule sent that no request was waiting for, such as approval asks,
  notices and task results. It keeps the last 200 per client. `wait=25`
  long-polls.
- **The owner:** `http_owner = "<client name>"`. Notices and approvals
  addressed to the owner go into that client's outbox and its webhook.
- **Approvals:** the ask arrives as a message with
  `"choices":[{"label":"Allow","reply":"yes a1b2"},…]`. The client answers
  by posting the reply text.
- **Outbound webhook (task results):**
  - A client may have a `webhook` URL.
  - Every outbox message is `POST`ed to it as JSON, with
    `X-Ferrule-Signature: sha256=<hex HMAC(key, body)>`.
  - The HMAC key is a per-client webhook secret, shown once alongside the
    API key. The receiver can check it without holding the API key.
  - Delivery retries 3 times (1 s, 5 s, 30 s). A failed delivery stays in
    the outbox and is noted in `problem()`.
  - Webhook URLs must be `https://` or loopback `http://`.
- **Limits:**
  - Bodies ≤ 64 KiB. Each client gets 30 requests a minute (429 with
    `Retry-After`).
  - One running turn per chat: that's the router's lane, and a second
    request queues behind it.
  - A 401 says only "unknown or revoked key".
- **No 👀:** `polls() = false`. It's a server, and never stale.

## 9. Every channel: setup, card, doctor

| | Setup step | Card fields (Test = probe) | Doctor line |
|---|---|---|---|
| WhatsApp | token, phone number id, app secret, inbound mode; prints the callback URL + verify token; deploys/updates the relay | token, phone number id, app secret, template | `GET /<pnid>?fields=display_phone_number,verified_name` → "+972 50… (Name) · mailbox ok · N held" |
| Matrix | homeserver, token or user+password → token; pairing by code | homeserver URL, token | `GET /account/whoami` → "@bot:server · 2 rooms, 1 encrypted (refused)" |
| Email | Gmail connection or host/user/password; sends a test to the owner | address, password (or "use Gmail connection") | IMAP login + `SELECT` + SMTP `EHLO/AUTH` → "max@… · IDLE" |
| Signal | detects Java + signal-cli; account; `listAccounts` | account, daemon URL | `signal-cli --version`, JSON-RPC `version` → "+972… · daemon 0.13.x" |
| Mattermost | server URL, bot token; pairing by code | server URL, token | `GET /users/me` → "@ferrule on chat.example.com" |
| HTTP API | port, public or not, the first key | clients (add/revoke), port | "127.0.0.1:8788 · 2 keys" |

- **Setup** follows M31's step shape:
  - probe an existing token first;
  - keep, remove or replace;
  - ask, probe, save;
  - pair by a one-time code where the channel has DMs.
- **Cards:**
  - A new "Channels" section on the dashboard home, one card each, in M37
    style: icon, status dot (on/stale/problem/off), fields with **Test** and
    **Save**, and an ordered guide with direct links.
  - Save writes `secrets.env` and the `[gateway.<channel>]` table. It then
    offers the existing "Restart the gateway" action, because channels are
    built at start.
  - Secret fields are write-only: the page shows "set" and never the value.
  - The API is `GET channels`, `POST channels/test`, `channels/save`,
    `channels/remove`, `channels/http/keys`, and `channels/http/key/add`
    and `…/revoke`. They sit behind the same session, CSRF and origin
    checks as every other POST.
- **Doctor:** one line per configured channel. "off" is a note. A failed
  probe is a fail with a hint. Doctor also runs the §10 collision checks.

## 10. Instances: collisions

`instances.rs`'s `Facts` gains one non-secret account id per channel. Two
instances with the same id **fail** in doctor and are warned about in
setup, as two instances on one Telegram bot are today:

| Channel | Account id | Why two can't share it |
|---|---|---|
| WhatsApp | `phone_number_id` | one webhook per app; both would answer, or neither receive |
| Matrix | `@user:server` | two `/sync` loops, double answers |
| Mattermost | server + bot user (from the token's fingerprint) | double answers |
| Signal | account number | signal-cli locks its data dir; the second daemon fails |
| Email | `username@imap_host` | both mark the same mail seen; each sees half |
| HTTP API | port | the second can't bind |

## 11. Threat model

| Asset | Threat | Mitigation |
|---|---|---|
| Channel tokens | the model reads or prints them | secrets.env only, in the sandbox's scrub list, the redactor (new shapes: `syt_…`, `EAA…`, `frk_…`, Mattermost 26-char tokens), never in errors (`explain()` style) |
| WhatsApp webhook | forged events | HMAC at the Worker **and** in ferrule; mailbox path derived from the relay key; bearer on `take` |
| Relay | Cloudflare account compromised | can read message bodies for ≤ 24 h and forge events, not send; documented (§3.2) |
| Email | spoofed `From:` of the owner | `Authentication-Results` DMARC/SPF check (default on Gmail); approvals by email are refused unless auth passed |
| Email | reply loops, backscatter | §5 list: headers, bounces, own mail, per-sender budget |
| Matrix | E2EE rooms leaking plaintext | refused with a notice, never half-read |
| HTTP API | key theft | hashed at rest, loopback by default, revocable, per-client limits; keys shown once |
| HTTP API webhook | SSRF to internal hosts | https or loopback only; a redirect is not followed |
| Files in | path traversal, huge files | names sanitized, fixed inbox dir, size cap before and during download |
| Files out | exfiltrating secrets | `send_file` is workspace-only, respects hidden files, approval-gated like writes, only to the session's own chat |
| Signal | spawned daemon | runs as the user, bound to 127.0.0.1, ferrule never passes secrets on its command line |

## 12. Failure modes

| Failure | What the owner sees |
|---|---|
| WhatsApp token expired (190) | `problem()` on the card and `/status`; doctor fails; owner notice on another channel |
| Relay not deployed or wrong key | card: "mailbox unreachable (401)"; doctor fails; no silent polling |
| Mailbox full (1000 events) | Meta retries (503); `problem()` "mailbox full — is ferrule polling?" |
| 24 h window closed | §3.4: template or held + `problem()` + doctor warn |
| Matrix sync 401 (token revoked) | `problem()` and stale; doctor fails with "log in again with `ferrule setup`" |
| Matrix encrypted room | one notice in the room, doctor lists it |
| IMAP IDLE unsupported or dropped | falls back to polling, logged once; `/status` shows "polling" |
| SMTP refuses | the send errors; the owner's notice goes elsewhere; `problem()` |
| signal-cli missing / dies | setup/doctor say what to install; a dead child restarts with backoff, `problem()` after 3 fails |
| Mattermost socket drops | reconnect with backoff; stale after `poll_stale_secs` like Discord |
| HTTP port taken | the channel's `run` fails at start with the port, the gateway runs the rest |

## 13. Out of scope

- Matrix E2EE (Olm/Megolm). The refusal is explicit, see §4.
- WhatsApp groups, payments, flows, calling, and anything outside the
  Cloud API. On-Premises was sunset. Unofficial libraries (Baileys,
  whatsmeow) risk a ban and need a Node or Go sidecar.
- Managing named Cloudflare tunnels. Ferrule prints what to point where.
- Reading images (vision). The agent gets the file path.
- Attachments for Telegram, Discord and Slack. They keep M31's behaviour,
  and that's a follow-up.
- Teams, Google Chat, Twilio SMS and Zulip, as follow-ups:
  - **Teams:** Azure Bot registration (app id + secret), Bot Framework
    activities POSTed to a public HTTPS endpoint with JWT validation
    against Microsoft's OpenID keys. Needs the relay to forward rather
    than store (Teams expects a reply to the POST within 15 s), or a
    tunnel.
  - **Google Chat:** a Chat app in a Google Cloud project, with events by
    HTTP endpoint or Pub/Sub. Workspace accounts only. The Pub/Sub pull
    subscription avoids a public URL, and it needs a service account key.
  - **Twilio SMS:** account SID + auth token, a number, and an inbound
    webhook signed with `X-Twilio-Signature` (HMAC-SHA1 over the URL +
    sorted params). It fits the WhatsApp mailbox pattern: the Worker
    verifies with the auth token and ferrule polls.
  - **Zulip:** a bot's email + API key. Events come by
    `POST /api/v1/register` then long-poll `GET /api/v1/events`, with
    streams and topics as chats. It's the most direct of the four, but
    there was no time to build it well.

## 14. Tests

- **Hermetic tests**, each channel against its own mock on 127.0.0.1:
  - **Graph API + mailbox** (Rust mock). The mailbox uses the Worker's
    contract. `relay/worker.test.js` covers the Worker side, and **CI now
    runs `node --test relay/`**.
  - **Matrix:** an HTTP mock with a scripted `/sync`.
  - **Email:** IMAP (with IDLE) and SMTP TCP mocks.
  - **Signal:** an HTTP mock with SSE.
  - **Mattermost:** HTTP plus WebSocket.
  - **HTTP API:** a real listener.
- **What the tests cover:**
  - allowlist, stranger, pairing and mention rules;
  - owner notices and approvals end to end: the ask, the press or reply,
    and the OwnerDoor;
  - files in and out;
  - rate limits and retries;
  - the 24 h window;
  - loop guards;
  - E2EE refusal;
  - redaction;
  - doctor lines and collisions.
- **Live tests:** each channel has `#[ignore]`d live tests that run only
  when their env vars are set (`FERRULE_LIVE_WHATSAPP_TOKEN`, …). They
  send one message and read one back.
- **CI:** all three OSes.

## 15. Parts

0. The Windows `live_fixes` flake: the 409 test's clock, silent dispatch
   errors, the `tasks.db` busy timeout, and stderr in failed waits.
1. This document.
2. Seams:
   - the `CHANNELS` table and every hard-coded list moved onto it;
   - `files.rs`;
   - `send_file`;
   - `hmac.rs`;
   - the card framework on the dashboard.
3. WhatsApp: the adapter, relay routes and tests, setup, card, doctor, and
   collisions.
4. Matrix.
5. Email.
6. Signal.
7. Mattermost.
8. HTTP API.
9. `channels.md`, PLAN, roadmap.

## As built

(Filled in at the end of the milestone.)
