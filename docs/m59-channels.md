# M59: personal WhatsApp and iMessage, through bridges you run

Ferrule's WhatsApp channel (M39) uses Meta's Cloud API and needs a
WhatsApp Business number. Many owners' contacts are on a personal
WhatsApp number or on iMessage instead. M59 adds two opt-in channels for
them. Neither one speaks WhatsApp's or Apple's protocol itself. Each
talks to a **bridge** that the owner installs and runs:

| Channel | Config table | Bridge | Runs on |
|---|---|---|---|
| WhatsApp (personal) | `[gateway.whatsapp_personal]` | [WAHA](https://waha.devlike.pro) (WhatsApp HTTP API) | any machine with Docker |
| iMessage | `[gateway.imessage]` | [BlueBubbles Server](https://bluebubbles.app) | a Mac signed in to Messages |

Neither channel was run against a live bridge while it was built. The
code follows each bridge's published API (sources at the end), and the
first real check is `ferrule doctor` and the dashboard card's **Test**.

## Read this first: the risks

**Personal WhatsApp through WAHA is unofficial automation.** WAHA drives
WhatsApp Web, a linked device of your phone. WhatsApp's terms of service
don't allow automated or bulk messaging, or access through unauthorised
clients ([WhatsApp Terms of Service](https://www.whatsapp.com/legal/terms-of-service)).
WhatsApp can restrict or ban a number it believes breaks them, and that
includes a number that only answers its owner. Ferrule can't make this
risk go away. Use a number you can afford to lose, keep the allow-lists
short, and don't use it for bulk or unsolicited messages. A
WhatsApp-Web-based library has also had its own security flaws (NanoClaw
patched a Baileys message-spoofing issue; the gap analysis cites it).
For a channel with no such risk, use the M39 WhatsApp channel and a
Business number from Meta ([channels.md](channels.md#whatsapp)).

**iMessage needs a Mac that stays on.** BlueBubbles Server reads the
Mac's Messages database and sends through the Messages app, so it needs
macOS, a signed-in Apple ID, Full Disk Access and Accessibility
permissions, and the Mac awake. Apple publishes no iMessage API, and
BlueBubbles isn't an Apple product. Some features (typing, quoted
replies) need BlueBubbles' Private API, which needs System Integrity
Protection partly turned off on that Mac. Ferrule works without it.

## Why bridges, and why these two

Ferrule doesn't build a WhatsApp Web client into its Rust code. That
protocol changes without notice, and the risk above would then live
inside ferrule. Instead the gateway talks to a bridge over HTTP, the
way the Signal channel talks to signal-cli (M39 §6). The bridge holds
the WhatsApp or Apple session; ferrule holds an API key for the bridge.

**WAHA** was chosen for WhatsApp because:

- it is a server with a documented REST API (`/api/sendText`,
  `/api/sendImage`, `/api/sendFile`, `/api/sessions/{session}`) and two
  documented ways to receive: webhooks and a WebSocket
  ([events](https://waha.devlike.pro/docs/how-to/events/));
- the WebSocket lets ferrule connect **out** to WAHA, so nothing on
  ferrule's machine has to listen, and WAHA in Docker doesn't need to
  reach the host;
- it ships as one Docker image, with a dashboard where you start a
  session and scan its QR code. Pairing stays the bridge's job;
- it has an API key (`X-Api-Key`) and several engines (WEBJS, NOWEB,
  GOWS) behind one API.

The other candidates were weaker fits for this job. Baileys and
whatsapp-web.js are libraries, so ferrule would have to run JavaScript
of its own. Evolution API is a larger server with a database. A
Matrix bridge (mautrix-whatsapp) needs a Matrix homeserver; if you
already run one, the M39 Matrix channel works with it unchanged.

**BlueBubbles** was chosen for iMessage because it is the widely used
open-source iMessage server, with a REST API
([REST API & Webhooks](https://docs.bluebubbles.app/server/developer-guides/rest-api-and-webhooks))
whose message query lets ferrule poll, so again nothing has to listen
on ferrule's side, and the Mac can be another machine on the LAN.

## WhatsApp (personal)

### Set up WAHA

1. Run WAHA with an API key of your own, bound to this machine only:

   ```sh
   docker run -d --name waha -p 127.0.0.1:3000:3000 \
     -e WAHA_API_KEY=<a long random key> devlikeapro/waha
   ```

   WAHA's quick start: <https://waha.devlike.pro/docs/overview/quick-start/>.
   Leave `WAHA_EVENTS_DOWNLOAD_MEDIA` on (WAHA's default) if you want the
   agent to get the photos and files people send.
2. Open WAHA's dashboard (`http://127.0.0.1:3000/dashboard`), start the
   session `default`, and scan its QR code with WhatsApp → Settings →
   Linked devices on your phone.
3. `ferrule setup` → **WhatsApp (personal)**, or the dashboard's
   **WhatsApp (personal)** card. Setup asks WAHA's address, the session
   and the API key, checks them against WAHA, and waits for the session
   to be `WORKING` (scan the QR code meanwhile). Then it asks which
   numbers may write.

By hand, in `config.toml`:

```toml
[gateway.whatsapp_personal]
url = "http://127.0.0.1:3000"        # WAHA's address (the default)
# session = "default"
# api_key_env = "WAHA_API_KEY"        # the default; the value goes in secrets.env
allowed_users = ["972501234567"]      # digits with the country code
# allowed_groups = ["120363025246125486@g.us"]
# max_file_mb = 20
```

and `WAHA_API_KEY=<the key>` in `<data>/private/secrets.env`
(`ferrule setup` writes it there).

### Whose number is it?

WAHA links one WhatsApp account, the one whose phone scanned the QR
code. Two ways to use it:

- **A spare number for the bot.** Link a second phone or SIM, and
  message it from your own number, which you list in `allowed_users`.
  This is the clearest setup, and your own account isn't the one at risk.
- **Your own number.** Then the bot answers **as you**. List your own
  number in `allowed_users`, and your "Message yourself" chat reaches the
  agent. Every number you list is answered by the agent in your name, so
  list only people who know that.

### Who gets an answer

- A one-to-one chat with a number on `allowed_users` (or
  `[trust] whatsapp_personal_owner`). Numbers are compared as digits:
  `+972 50-123-4567` is `972501234567`. A contact WhatsApp shows only by a
  hidden id comes as `<id>@lid`; list it like that.
- Your "Message yourself" chat, when the linked account's own number is
  allowed. What ferrule sent there itself is recognised by its id (and
  its text) for ten minutes, so the bot doesn't answer its own replies.
- A group on `allowed_groups` (`…@g.us`) when a message @mentions the
  linked number or replies to one of its messages. The mention is taken
  out of the text.
- Anyone else, and status updates and channels, get nothing. A stranger
  is never told anything: this is a personal number.

### What works

- Text in and out. Replies are cut at 4,096 characters a message, and
  Markdown goes out as WhatsApp's `*bold*`, `_italic_` and `~strike~`.
- Photos and files in, when WAHA downloaded them: they are fetched from
  WAHA with the API key and saved under `<workspace>/inbox/whatsapp_personal/`,
  capped at `max_file_mb`. The key is only ever sent to WAHA's own
  address; a file address outside it is refused.
- Files out (`send_file`): JPEG and PNG through `/api/sendImage`,
  anything else through `/api/sendFile`. Older WAHA Core builds refuse
  media sends; the error says so.
- "typing…" while a turn runs (`/api/startTyping`, `/api/stopTyping`).
- Quoted replies (`reply_to`).
- No buttons (approvals are typed), no edits (no streaming), and no 👀
  reaction on arrival: on a personal account it would come from you.

### How it connects

At start and after every drop, the gateway asks WAHA for the session
(`GET /api/sessions/{session}`). While it isn't `WORKING` (waiting for a
QR scan, starting, stopped), the channel's problem says which, in
`/status`, on the dashboard card and in `ferrule doctor`, and it tries
again with backoff. Once it works, the gateway opens WAHA's WebSocket
(`/ws?session=…&events=message.any&events=session.status`), pings it
every 30 seconds, and reopens it when it goes quiet or closes.

## iMessage

### Set up BlueBubbles

1. On the Mac, sign in to Messages. A separate Apple ID for the bot is
   the clearest setup: you message it from your own iPhone. With your
   own Apple ID, the bot answers your contacts as you.
2. Install BlueBubbles Server (<https://bluebubbles.app/install/>), give
   it the Full Disk Access and Accessibility permissions it asks for,
   and set a server password. Keep the Mac from sleeping.
3. `ferrule setup` → **iMessage**, or the dashboard's **iMessage** card:
   the server's address (`http://<the Mac>:1234` on your network), the
   password, then who may write.

By hand:

```toml
[gateway.imessage]
url = "http://192.168.1.20:1234"
# password_env = "BLUEBUBBLES_PASSWORD"   # the default; the value goes in secrets.env
allowed_users = ["+15551234567", "max@icloud.com"]
# private_api = false     # true when BlueBubbles' Private API is set up
# poll_secs = 3
# max_file_mb = 20
```

### Who gets an answer

- A one-to-one conversation with an address on `allowed_users` (or
  `[trust] imessage_owner`): a phone number (compared as `+` and its
  digits) or an Apple ID email (compared in lower case).
- Group chats, the Mac's own messages and tapbacks are ignored. Strangers
  are never answered.

### What works

- Text in and out (4,000 characters a message; iMessage shows Markdown
  as typed).
- Attachments in, downloaded from the server into
  `<workspace>/inbox/imessage/`, capped at `max_file_mb` (an iPhone's
  photos are often HEIC, which most models can't see). Files out through
  `/api/v1/message/attachment`.
- With `private_api = true`: "typing…" and quoted replies.

### How it connects

Ferrule polls `POST /api/v1/message/query` every `poll_secs` (3 s),
oldest first. Each poll looks two minutes behind the newest message it
has seen, so a message Messages files late still comes in, and skips
what it has already seen by its guid. The position is kept in
`<data>/gateway/imessage/cursor`, so messages that arrived while the
gateway was stopped are answered after a restart, up to a day back;
nothing older is replayed.

## Both channels

- **Opt-in.** Nothing runs until the table is in the config.
- **Secrets.** WAHA's API key and the BlueBubbles password live in
  `secrets.env`, named by `api_key_env` and `password_env`. Like every
  channel token, they are scrubbed from commands' environments, redacted
  from logs and `/status`, and taken out of tool results. They travel
  only from the gateway to the bridge: WAHA's key in the `X-Api-Key`
  header (and, for the WebSocket, its documented `x-api-key` query
  parameter), the BlueBubbles password as its documented `password` query
  parameter. No address that holds one is logged. Keep both bridges on
  `127.0.0.1` or your LAN; use `https://` if the traffic crosses a
  network you don't trust.
- **Owner and guests.** The owner's chat is `[trust]
  whatsapp_personal_owner` / `imessage_owner`, else the first allowed
  address, as on every channel. The owner's messages run in owner mode;
  everyone else's are guests' (M54).
- **The rest is shared.** The paste guard (M52.5, "Keys pasted into a
  chat"), steering and the busy notice (M56.1, "Messages sent while it
  works"), chat commands, voice transcription and `send_file` work
  through the gateway's router as on every channel. Neither channel can
  delete a pasted message, so the reply asks you to.
- **Doctor and the dashboard.** `ferrule doctor` has a line for each:
  whether WAHA's session is `WORKING` (and a note on the terms-of-service
  risk), whether BlueBubbles answers (and whether `private_api` matches
  the server), and who is allowed. The dashboard's Channels section has
  a card for each, with Test.
- **Instances.** Two instances on one WAHA session, or one BlueBubbles
  server, would both answer every message; `ferrule instances` and setup
  warn.

## What is open

- **No live run.** Neither bridge was available while this was built.
  Unverified: WAHA's WebSocket with two `events` parameters and the key
  in its query; the send responses' id shapes across WAHA's engines (the
  echo check falls back to the text); BlueBubbles' `after` being
  milliseconds and its `handle` on query results (the address falls back
  to the conversation's guid).
- WAHA's `@lid` ids: a contact who appears only by a hidden id must be
  listed by it, and the "Message yourself" chat is found by the account's
  `@c.us` id only.
- iMessage group chats aren't supported.
- No pairing by code in setup, unlike the M39 channels: numbers are
  typed.

## Sources

- WAHA: [events and WebSocket](https://waha.devlike.pro/docs/how-to/events/),
  [sending messages](https://waha.devlike.pro/docs/how-to/send-messages/),
  [sessions and QR](https://waha.devlike.pro/docs/how-to/sessions/),
  [receiving messages and media](https://waha.devlike.pro/docs/how-to/receive-messages/).
- BlueBubbles: [REST API & Webhooks](https://docs.bluebubbles.app/server/developer-guides/rest-api-and-webhooks)
  (the `guid`/`password` query parameter), and the server's routes and
  validators in
  [bluebubbles-server](https://github.com/BlueBubblesApp/bluebubbles-server)
  (`/api/v1/message/query`, `/message/text`, `/message/attachment`,
  `/attachment/:guid/download`, `/chat/:guid/typing`, `/server/info`).
- [WhatsApp Terms of Service](https://www.whatsapp.com/legal/terms-of-service).
- The adapters: `crates/ferrule-gateway/src/channels/waha.rs` and
  `bluebubbles.rs`; the CLI side in `crates/ferrule-cli/src/channels/bridges.rs`
  and `setup/bridges.rs`.
