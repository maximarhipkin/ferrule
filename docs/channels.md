# Channels

Ferrule answers you where you already talk. One gateway daemon runs every
channel that has credentials. Every channel keeps the same rules:

- **You are the owner.** The first person you pair becomes the channel's
  owner, and only the owner can approve a tool call or a change to
  Ferrule itself (on Telegram, with a button).
- **Strangers are ignored.** Only people on `allowed_users` (or paired with
  a one-time code from `ferrule setup`) get an answer. Anyone else gets
  nothing.
- **Mention-only in shared rooms.** In a group or room, ferrule answers when
  it is named or replied to.
- **Tokens stay out of the model's reach.** They live in the secrets file,
  never in the config, the prompt or a tool's environment.
- **Setup, the dashboard, doctor.** `ferrule setup` has a step for each
  channel. The dashboard's Channels section has a card with a form, a Test
  button and a guide. `ferrule doctor` has a line per channel.

Telegram is set up by `ferrule setup` and described in the README; Discord
and Slack in [discord.md](discord.md) and [slack.md](slack.md). This page covers the
channels added in M39. The design, and the reasons behind it, are in
[m39-channels.md](m39-channels.md). What works in every chat, whichever
channel it's on, is at the end: [chat commands](#chat-commands),
[voice messages](#voice-messages) and [the typing
indicator](#typing-indicator).

## Which channel to pick

| You want | Pick |
|---|---|
| The quickest start, on your phone | Telegram |
| Ferrule in the chat app your customers or family already use | WhatsApp (a business number, from Meta) |
| An open, self-hostable chat, or a room shared with a team | Matrix (unencrypted rooms only) |
| Nothing new to install: write to it like a colleague, or get task results in your inbox | Email (a mailbox of its own) |
| End-to-end encryption to your phone, with no company account or webhook | Signal (a spare number, and signal-cli on the machine) |
| The team chat your company already runs itself | Mattermost (a bot account on your server) |
| A script, n8n, Zapier or another service talking to it, or task results pushed to a URL | The HTTP API (a key per program) |

## WhatsApp

Ferrule talks to WhatsApp through Meta's official **Cloud API**, with a
WhatsApp Business number. It never logs into a personal WhatsApp account,
and it doesn't use unofficial libraries: those break Meta's terms and get
numbers banned. WhatsApp is opt-in; nothing happens until you set it up.

### What you need

1. A Meta developer app of type **Business** with the WhatsApp product:
   <https://developers.facebook.com/apps>. Meta gives you a free test
   number to start with. A real number is added under WhatsApp → API
   Setup.
2. The **phone number ID**, from WhatsApp → API Setup → From. It's a long
   number, and it is *not* the phone number itself.
3. A **permanent access token**. Go to Business settings → Users → System
   users (<https://business.facebook.com/settings/system-users>), add a
   system user, assign it the app, and generate a token with
   `whatsapp_business_messaging` and `whatsapp_business_management`. The
   temporary token on the API Setup page works for 24 hours only, and
   doctor says so when it expires.
4. The **app secret**: App settings → Basic. Ferrule checks every
   incoming webhook's signature with it.
5. A way for Meta's webhooks to reach ferrule: your **relay** (the default)
   or your own **tunnel**. See below.

While you use the test number, WhatsApp only delivers to numbers listed
under API Setup → To. Add your own number there first.

### Steps

The short way is **`ferrule setup` → WhatsApp**, or the dashboard's
WhatsApp card. Setup:

1. asks the phone number ID and the token, and checks them against Meta
   right away (a wrong token or id is named plainly);
2. asks the app secret;
3. makes a **verify token** for you (a random word Meta uses once, to check
   the callback URL is yours);
4. asks where webhooks come in: the relay or a local port;
5. asks an optional **template** for the 24-hour window (below);
6. prints what to paste into Meta, at WhatsApp → Configuration → Webhook
   → Edit (the guide:
   <https://developers.facebook.com/docs/whatsapp/cloud-api/guides/set-up-webhooks>):
   - **Callback URL**;
   - **Verify token**;
   - then **Verify and save**, and subscribe to the **`messages`** field;
7. pairs you: send the code it shows to the business number from your own
   WhatsApp. If that doesn't arrive, type your number instead.

On the dashboard, fill in the card, **Save**, then **Test**. With the
relay, Test also prepares the relay's mailbox and shows the callback URL
to paste into Meta.

By hand, in `config.toml`:

```toml
[gateway.whatsapp]
phone_number_id = "123456789012345"
# token_env = "WHATSAPP_TOKEN"            # the defaults; the values go in secrets.env
# app_secret_env = "WHATSAPP_APP_SECRET"
# verify_token_env = "WHATSAPP_VERIFY_TOKEN"
inbound = "relay"                          # or "listen"
# listen_port = 8787
# template = "ferrule_update"
# template_language = "en_US"
allowed_users = ["972501234567"]           # digits with the country code, no +
```

### Inbound: the relay, or your own tunnel

Sending needs no public address. Receiving does, because Meta posts every
message to a fixed https URL.

**The relay (default).** The same Cloudflare Worker that `ferrule
connections relay deploy` puts up for sign-ins (M20/M37) doubles as a
webhook **mailbox**. Meta posts to it, and ferrule collects from it every
3 seconds with the relay key. Nothing runs on your machine, and nothing
listens on it. A relay deployed before M39 has no mailbox: deploy it again
(doctor says when). The Worker:

- answers Meta's one-time `hub.challenge` only with your verify token;
- refuses every post whose `X-Hub-Signature-256` isn't the app secret's
  HMAC of the body;
- keeps events until ferrule collects them, normally a few seconds, and 24
  hours at most. If the mailbox is full, it tells Meta to retry later.

The mailbox's address is derived from the relay key, so only someone who
holds the key can find or empty it. Ferrule checks every event's signature
again before reading it.

**What the relay sees.** Be clear about this before choosing it:

- Cloudflare holds the **webhook bodies in plaintext** until ferrule takes
  them: message text, sender numbers and profile names, and media *ids*.
  The files themselves stay on Meta, and fetching one needs the access
  token.
- The Worker holds the **app secret** and the **verify token**. It never
  holds the **access token**.
- So whoever controls your Cloudflare account can read incoming messages
  and forge incoming events, but can't send as your number.

**Your own tunnel (`inbound = "listen"`).** Ferrule listens on
`127.0.0.1:8787` (`listen_port`) and checks the challenge and signatures
itself. Point a **named** tunnel or reverse proxy at it, and give Meta its
https address as the callback URL. A quick `trycloudflare.com` URL
changes at every restart, and Meta's callback URL is fixed, so it only
suits a test. Cloudflare's guide to named tunnels:
<https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/get-started/create-remote-tunnel/>.

### The 24-hour window

WhatsApp lets a business send a free-form message only within **24 hours
of the person's last message**. After that, only an approved **template**
goes through. Ferrule never drops a message silently because of it:

- **With a template** (`template = "name"`): ferrule sends the template,
  with `{{1}}` set to the start of the message. It keeps the full message
  and sends it as soon as the person writes again. The template must be an
  approved *utility* template whose body has exactly one `{{1}}`. Create
  it in WhatsApp Manager → Message templates
  (<https://business.facebook.com/wa/manage/message-templates/>).
- **Without a template:** the message is held, the send reports "the
  24-hour window is closed", `/status` and doctor show it, and it goes out
  when the person writes again.

Held messages are kept for 7 days, and at most 20 per chat. A scheduled
task that reports to WhatsApp needs a template, unless you write to the
number every day.

### What works

- **Text** both ways, up to 4096 characters per message; a longer answer
  is split. Markdown becomes WhatsApp's `*bold*`, `_italic_`, `~strike~`
  and monospace.
- **Approvals** as up to three reply buttons. With more choices, they
  arrive as text, and you answer with the keyword (`yes a1b2`).
- **👀** on your message when ferrule starts on it, and **blue ticks** (a
  read receipt) at the same time.
- **Files in:** images, documents, audio and video are saved to the
  workspace's inbox, up to `max_file_mb` (default 20).
- **Files out:** images (JPEG and PNG), documents, audio and video, within
  Meta's limits (images 5 MB, audio and video 16 MB, documents 100 MB).
- **No streaming.** WhatsApp can't edit a sent message, so the answer
  arrives whole.
- **DMs only.** Cloud API numbers can't join groups.

### Limits and errors

- **Rate limits.** When Meta says the number is sending too fast (130429),
  ferrule waits and retries three times. Sends to one person are spaced a
  second apart.
- **An expired or revoked token** (error 190) shows in `/status` and
  fails doctor. Make a permanent system-user token.
- **A test number** delivers only to the numbers under API Setup → To;
  the error says so.
- **Strangers get nothing.** Answering a stranger would open a
  conversation Meta bills you for.
- **Pairing while the gateway runs.** With the relay, setup and a running
  gateway collect from the same mailbox, and the gateway may take the
  pairing code first (it is ignored). Stop the gateway while pairing, or
  type your number when setup asks.
- **One number, one instance.** Two instances (M38) can't share a phone
  number ID, because Meta sends its webhooks to one callback URL. Doctor
  and setup say which instance already has it.

### Checking it

`ferrule doctor` checks the token and the number against Meta, and checks
that the relay has the mailbox or that `listen` has a port. It warns when
messages are held for a closed window, and when nobody is allowed yet.
`ferrule doctor --offline` skips the network.

A live round trip, not run in CI:

```sh
FERRULE_LIVE_WHATSAPP_TOKEN=EAA… \
FERRULE_LIVE_WHATSAPP_PHONE_ID=123456789012345 \
FERRULE_LIVE_WHATSAPP_TO=972501234567 \
cargo test -p ferrule-gateway --test it whatsapp:: -- --ignored
```

It sends a message with buttons to `…_TO`. With
`FERRULE_LIVE_WHATSAPP_RELAY_URL`, `FERRULE_LIVE_RELAY_KEY`,
`FERRULE_LIVE_WHATSAPP_APP_SECRET` and `FERRULE_LIVE_WHATSAPP_VERIFY_TOKEN`
set too, it also waits for your reply through the relay.

## Matrix

Ferrule joins Matrix as an ordinary user account: its own bot account on
any homeserver (matrix.org, your own Synapse, Conduit or Dendrite). It
reads with the client-server API's `/sync` long-poll, so nothing has to
reach your machine: no webhook, no relay, no open port.

**Encrypted rooms are refused.** Ferrule doesn't do end-to-end encryption,
so it can't read an encrypted room. When it's invited to one, or a message
arrives encrypted, it says once, in the room, that it won't answer there
and how to make an unencrypted room. Then it stays quiet. Doctor lists
those rooms. See [m39-channels.md](m39-channels.md) §4 for why.

### What you need

1. **A separate account for the bot.** Register one at
   <https://app.element.io/#/register> (matrix.org), or on your own
   homeserver. Don't use your own account: ferrule answers from it.
2. **An access token, or the bot's password.** Setup can log in with the
   password for you, and then keeps only the token. To get a token by
   hand, sign in to Element as the bot: Settings → Help & About → Access
   token. Then close the tab **without logging out**, because logging out
   ends that token.
3. **Your own Matrix account**, to talk to it.

### Steps

The short way is **`ferrule setup` → Matrix**, or the dashboard's Matrix
card. Setup:

1. asks the homeserver. `matrix.org`, `@bot:matrix.org` or a URL all work.
   It follows the server's `.well-known`, so `matrix.org` becomes
   `https://matrix-client.matrix.org`;
2. logs in with the bot's user id and password, or takes a pasted token,
   and checks it right away. It shows the account and its rooms, and warns
   about encrypted ones;
3. saves the token to the secrets file and the bot's user id to the
   config. The password is never stored;
4. pairs you: start a direct chat with the bot from your own account. The
   bot joins by itself. Then send it the code setup shows. If that
   doesn't arrive, type your user id instead.

**Allow a room** in setup adds a room where a mention reaches ferrule. To
find its id in Element: Room settings → Advanced → Internal room ID
(`!abc123:matrix.org`). Invite the bot to the room first. To make a room
for it, turn off **Enable end-to-end encryption** when you create the
room. Element turns it on by default for private rooms, and it can't be
turned off afterwards.

On the dashboard, fill in the card (homeserver, token, users and rooms),
**Save**, then **Test**.

By hand, in `config.toml`:

```toml
[gateway.matrix]
homeserver = "https://matrix-client.matrix.org"
access_token_env = "MATRIX_ACCESS_TOKEN"   # the value goes in secrets.env
user = "@mybot:matrix.org"                 # whose token it is
# or, instead of a token, a password login (the session is kept):
# user = "@mybot:matrix.org"
# password_env = "MATRIX_PASSWORD"
allowed_users = ["@max:matrix.org"]        # whose DMs reach it
allowed_rooms = ["!abc123:matrix.org"]     # where a mention reaches it
# stream = true                            # default: [agent] stream
# max_file_mb = 20
```

With `password_env`, ferrule logs in once, keeps the session in the data
directory (`gateway/matrix/session.json`, readable only by you), and logs
in again if the server ends it.

### Who gets an answer

- **DMs.** An allowed user's direct chat. The chat is known by their user
  id (`@max:matrix.org`), not the room's, so it stays the same chat if
  the DM room changes. If there's no DM yet (a scheduled task's report,
  say), ferrule opens an unencrypted one, unless the homeserver forces
  encryption on every room.
- **Rooms.** In an allowed room, anyone in it can reach ferrule, but only
  by mentioning the bot: a pill (Element's @-completion), its user id, or
  its name at the start of the message. A reply to one of ferrule's
  messages counts too. This is Slack's rule, too.
- **Invites.** Ferrule joins an invite from an allowed user, and ignores
  the rest. While setup is pairing, it joins any invite.
- **Strangers get nothing.** A room that isn't allowed is ignored, even
  with a mention.

### What works

- **Text** both ways, up to 16 000 characters per message; a longer answer
  is split. Markdown is sent as Matrix HTML (bold, italic, code, links,
  lists, quotes, headings), with the Markdown itself as the plain body.
- **Streaming**: the answer is edited in place every 3 seconds while it's
  written (`stream = false` turns it off).
- **Approvals** as reactions. The message lists each choice with its
  reaction (👍 allow, 👎 refuse, then 1️⃣ 2️⃣ …), and ferrule reacts with
  each one so you only tap. Only an allowed user's reaction counts. You
  can also reply with the keyword (`yes a1b2`).
- **👀** on your message when ferrule starts on it, and a read receipt at
  the same time.
- **Files in:** images, audio, video and files, into the workspace's inbox,
  up to `max_file_mb` (default 20). An image's caption is its text.
- **Files out:** uploaded to the homeserver's media store and sent as an
  image, audio, video or file.

### Limits and errors

- **No end-to-end encryption**, as above. That includes encrypted DMs:
  Element encrypts a new DM by default, so let ferrule open the DM
  (pairing does), or turn encryption off when you start it.
- **Rate limits.** When the homeserver says `M_LIMIT_EXCEEDED`, ferrule
  waits as long as it asks (up to 30 seconds) and retries three times.
- **A token that stops working** (`M_UNKNOWN_TOKEN`: logged out in
  Element, or revoked) shows in `/status` and fails doctor with "log in
  again". With a password login, ferrule logs in again by itself once.
- **Messages sent while the gateway was off.** On a fresh start (no saved
  sync position), ferrule skips what was said before and only answers new
  messages. After a restart, it picks up where it stopped.
- **One bot account, one instance.** Two instances (M38) on one bot account
  would both answer and move each other's position. Doctor and setup name
  the instance that already has it.

### Checking it

`ferrule doctor` logs in, lists the joined rooms and checks which are
encrypted. It also warns when nobody is allowed. `ferrule doctor
--offline` skips the network.

A live round trip, not run in CI:

```sh
FERRULE_LIVE_MATRIX_URL=https://matrix-client.matrix.org \
FERRULE_LIVE_MATRIX_TOKEN=syt_… \
FERRULE_LIVE_MATRIX_TO=@you:matrix.org \
cargo test -p ferrule-gateway --test it matrix:: -- --ignored
```

It prints the probe, then sends `…_TO` a message, edits it and reacts 👀.

## Email

Ferrule reads a mailbox over IMAP and answers over SMTP, like any mail
program. It works with Gmail, iCloud, Yahoo, Fastmail, a company server,
or anything else that speaks IMAP and SMTP with a password. It never
needs a webhook or an open port.

**Give it a mailbox of its own** (`yourname.agent@gmail.com`). Ferrule
only reads mail from the senders you allow and leaves everything else
unread, but a separate mailbox keeps your own mail out of its reach
entirely.

### What you need

1. **A mailbox**, best a new one just for the agent.
2. **An app password** for it, not the account's normal password. Most
   providers refuse a normal password over IMAP.
   - **Gmail:** turn on 2-Step Verification, then make one at
     <https://myaccount.google.com/apppasswords>. Google shows 16 letters
     in four groups; paste them with or without the spaces. IMAP is on
     for new accounts; to check, see Settings → Forwarding and POP/IMAP
     (<https://mail.google.com/mail/u/0/#settings/fwdandpop>).
   - **iCloud:** <https://support.apple.com/en-us/102654>.
   - **Yahoo:** Account security → Generate app password
     (<https://login.yahoo.com/account/security>).
   - **Fastmail:** Settings → Privacy & Security → App passwords
     (<https://app.fastmail.com/settings/security/devicekeys>).
   - **Anything else:** your provider's help pages name its IMAP and SMTP
     servers and ports.
3. **Your own address**, to write from.

If you already connected Gmail as a tool (the dashboard's Connections,
M37), setup offers to use that connection: the same address and app
password, stored once.

### Steps

The short way is **`ferrule setup` → Email**, or the dashboard's Email
card. Setup:

1. offers the Gmail connection if there is one. Otherwise it asks the
   address and fills in the servers for Gmail, iCloud, Yahoo and Fastmail.
   For any other address it asks for the IMAP and SMTP servers;
2. asks the app password and checks it right away. It logs in to both
   servers, opens the inbox read-only and says whether the server has
   IDLE (push);
3. saves the settings to the config and the password to the secrets file;
4. asks your own address to allow (`@example.com` allows a whole domain),
   then offers to send you a test mail. Reply to it and the agent answers
   in the thread.

On the dashboard, fill in the card (address, app password, allowed
senders, and the servers if your provider isn't one of the four),
**Save**, then **Test**.

By hand, in `config.toml`:

```toml
[gateway.email]
address = "max.agent@gmail.com"
password_env = "EMAIL_PASSWORD"          # the value goes in secrets.env
allowed_senders = ["max@example.com", "@mycompany.com"]
# For providers other than Gmail, iCloud, Yahoo and Fastmail:
# imap_host = "imap.example.com"
# imap_port = 993                        # 993 is TLS; 143 is STARTTLS
# smtp_host = "smtp.example.com"
# smtp_port = 465                        # 465 is TLS; 587 is STARTTLS
# username = "max"                       # default: the address
# Or instead of address and password_env, M37's Gmail connection:
# use_connection = "gmail"
# require_auth_results = true            # see "Who gets an answer"
# poll_secs = 60                         # without IDLE: 10 to 240
# max_file_mb = 20
```

The first address on `allowed_senders` is the owner, the one who can
approve tool calls. To name another, set `email_owner = "max@example.com"`
under `[trust]`.

### Who gets an answer

- **Allowed senders only.** A mail from an address on `allowed_senders`,
  or from a domain listed as `@domain`, reaches the agent. Everyone
  else's mail is left unread, without a word back.
- **A chat is one sender.** The conversation follows the person's address,
  whatever the subject, so the agent remembers what you wrote last time.
- **Only new mail.** On its first start, ferrule remembers where the
  inbox ends and answers only what arrives afterwards. After a restart it
  picks up where it stopped, and nothing is answered twice.
- **Never a list, a bounce or a robot.** Ferrule never answers mail that
  looks automatic: mailing lists (`List-Id`, `List-Unsubscribe`),
  `Precedence: bulk`, bounces (an empty `Return-Path`, `MAILER-DAEMON`),
  out-of-office replies (`Auto-Submitted`, `X-Autoreply`), `noreply@`
  senders, and its own mail (`X-Ferrule-Loop`). This holds even when the
  sender is allowed. On top of that, it sends at most 10 mails an hour to
  one address. Past that it stops and says so in `/status`, in case
  something loops anyway.
- **Forged senders.** A mail's `From:` can say anything. The server
  that received the mail checks it (DMARC, SPF, DKIM) and writes the
  verdict in `Authentication-Results`. With `require_auth_results` on,
  ferrule drops mail whose sender wasn't vouched for. It is on by default
  for Gmail, iCloud, Yahoo and Fastmail, which always write that header.
  For other servers it is off unless you turn it on, because some
  servers don't write the header at all.

### What works

- **Text** both ways. HTML mail is read as text. Quoted history (`On …
  wrote:`, `>` lines, signatures, Outlook's "Original Message") is cut, so
  the agent sees only what you wrote. A new thread's subject is passed
  along as `Subject: …`.
- **Threads.** Answers go in your thread (`In-Reply-To`, `References`,
  `Re: <your subject>`). Mail that ferrule starts itself, such as a
  scheduled task's result, opens a new thread with the subject
  `ferrule: <first line>`.
- **Approvals by reply.** The question lists the choices ("send `yes a1`
  to allow"). Reply with the keyword as the first line; the quoted
  question below it doesn't matter. An approval only counts when the
  receiving server vouched for your address, even with
  `require_auth_results` off. Otherwise ferrule says it can't take an
  approval from that mail.
- **Files in:** attachments are saved to the workspace's inbox, up to
  `max_file_mb` (default 20) each. A mail too big to take gets a short
  answer instead of being downloaded.
- **Files out:** the agent's files are attached to its answer.

### Limits and errors

- **No push without IDLE.** Most servers have IDLE, so mail is seen within
  seconds. Without it, ferrule looks every `poll_secs` (default 60).
- **A refused login** (a wrong or revoked app password, or IMAP turned
  off) shows in `/status` and fails doctor. Ferrule then waits 10 minutes
  before it tries again, so that a wrong password doesn't get the
  account locked.
- **Outlook.com and Microsoft 365** no longer accept passwords over IMAP;
  they need OAuth, which this channel doesn't do yet.
- **No reactions, no streaming, no buttons.** Email has none of them. The
  answer is sent once, when it's done.
- **One mailbox, one instance.** Two instances (M38) reading the same
  mailbox would each take about half of the mail. Doctor and setup name
  the instance that already has it.

### Checking it

`ferrule doctor` logs in to IMAP and SMTP, reads nothing and marks
nothing read. It says whether the server has IDLE and how many senders
are allowed, and warns when nobody is. `ferrule doctor --offline` skips
the network.

A live round trip, not run in CI:

```sh
FERRULE_LIVE_EMAIL_ADDRESS=max.agent@gmail.com \
FERRULE_LIVE_EMAIL_PASSWORD='abcd efgh ijkl mnop' \
FERRULE_LIVE_EMAIL_IMAP=imap.gmail.com:993 \
FERRULE_LIVE_EMAIL_SMTP=smtp.gmail.com:465 \
FERRULE_LIVE_EMAIL_TO=you@example.com \
cargo test -p ferrule-gateway --test it email:: -- --ignored
```

It logs in, prints the probe and sends `…_TO` one mail.

## Signal

Ferrule talks to Signal through [signal-cli](https://github.com/AsamK/signal-cli),
an unofficial command-line client, running as a daemon on the same machine.
Messages stay end-to-end encrypted between your phone and that machine.
Signal has no bot API, so the agent needs a Signal account of its own, just
like a person.

signal-cli is not part of ferrule, and ferrule never downloads it. Setup
and doctor find it and say what's missing.

### What you need

1. **signal-cli**, from its releases page:
   <https://github.com/AsamK/signal-cli/releases>.
   - **Linux:** the native build (`signal-cli-<version>-Linux-native.tar.gz`)
     needs nothing else. Unpack it and put `bin/` on your `PATH`.
   - **macOS:** `brew install signal-cli`.
   - **Windows, or the JVM build anywhere:** it needs Java 21 or newer, for
     example Temurin from <https://adoptium.net/temurin/releases/>.
2. **An account for it.** Pick one:
   - **A separate number (best).** Any number that can receive an SMS or
     a call once. Register it with signal-cli: get a captcha token as the
     wiki describes (<https://github.com/AsamK/signal-cli/wiki/Registration-with-captcha>),
     then `signal-cli -a +972501234567 register --captcha <token>` and
     `signal-cli -a +972501234567 verify <code>`. You then write to the
     agent like any contact.
   - **Your own number, linked.** signal-cli becomes a linked device, like
     Signal Desktop (<https://github.com/AsamK/signal-cli/wiki/Linking-other-devices-(Provisioning)>).
     `ferrule setup` → Signal does the linking. You talk to the agent in
     **Note to Self**. Be aware that the linked device can read all of
     your new messages, although ferrule answers only the chats you
     allow.

### Steps

The short way is **`ferrule setup` → Signal**. Setup:

1. asks whether ferrule should start signal-cli's daemon (recommended)
   or use one you run yourself. If signal-cli isn't found, it explains how
   to install it and waits;
2. lists the accounts signal-cli already holds, or links a new one. It
   runs `signal-cli link -n ferrule` and shows the link as a QR code if
   `qrencode` is installed; scan it in Signal → Settings → Linked
   devices;
3. saves `[gateway.signal]`. There's no token: signal-cli keeps the
   account's keys in its own data folder;
4. allows you: pair with a code sent from your phone, type your number,
   or, on a linked number, allow Note to Self.

Groups are allowed later: **`ferrule setup` → Signal → Allow a group**
lists the account's groups while the daemon runs.

On the dashboard, fill in the Signal card (the number, allowed numbers and
groups, and optionally the daemon URL or signal-cli's path), **Save**, then
**Test**.

By hand, in `config.toml`:

```toml
[gateway.signal]
account = "+972501234567"                # the agent's number, as signal-cli holds it
allowed_users = ["+972541112233"]        # numbers, or Signal uuids
# allowed_groups = ["R3JvdXAtaWQ…="]     # group ids: a mention there reaches it
# signal_cli = "/opt/signal-cli/bin/signal-cli"   # default: the one on PATH
# port = 7583                            # the daemon ferrule starts, on 127.0.0.1
# url = "http://127.0.0.1:8080"          # a daemon you run instead (see below)
# max_file_mb = 20
```

The first number on `allowed_users` is the owner, the one who can approve
tool calls. To name another, set `signal_owner = "+972541112233"` under
`[trust]`.

### The daemon

Without `url`, the gateway starts
`signal-cli -a <account> daemon --http 127.0.0.1:<port> --receive-mode on-connection`
itself. It stops the daemon when the gateway stops and restarts it if it
exits. Its output goes to `gateway/signal/daemon.log` in the instance's
data folder. If a daemon already answers on the port, ferrule uses it as
it is.

To run the daemon yourself, for example in the
[bbernhard/signal-cli-rest-api](https://github.com/bbernhard/signal-cli-rest-api)
container in `json-rpc-native` mode, or as a system service, start it
with `--http` and set `url`. A daemon that serves several accounts works
too, because ferrule names its own account in each call.

`ferrule tasks run-now` doesn't start a daemon. To deliver a result to
Signal, it needs the gateway's daemon (or yours) to be running.

### Who gets an answer

- **Allowed numbers in a DM.** A message from a number (or uuid) on
  `allowed_users` reaches the agent. Anyone else gets nothing, and not
  even a read receipt.
- **Groups: allowed, and mentioned.** In a group on `allowed_groups`,
  ferrule answers when it is @-mentioned or someone replies to its
  message, whoever wrote it.
- **Note to Self** reaches the agent only when the agent's own number is
  on `allowed_users`; setup adds it when you pick that option.
- **Only new messages.** In on-connection mode, messages sent while the
  gateway was down wait on Signal's servers and arrive when it's back.

### What works

- **Text** both ways. The agent's Markdown (bold, italic, strikethrough,
  code, spoilers) becomes Signal's own text styles. Long answers are sent
  in parts of 2000 characters.
- **Replies** quote your message.
- **👀 and a read receipt** when a message is taken.
- **Approvals by reply.** Signal bots have no buttons. The question lists
  the choices ("send `yes a1` to allow"); answer with the keyword.
- **Files in:** photos, voice notes and documents are saved to the
  workspace's inbox, up to `max_file_mb` (default 20) each.
- **Files out:** the agent's files go with its answer.

### Limits and errors

- **No streaming.** Ferrule doesn't use Signal's message edits yet, so the
  answer is sent once, when it's done. There's no typing indicator
  either.
- **Rate limits.** Signal throttles new accounts that write to many
  people. On a rate limit ferrule waits and tries again, like on the
  other channels. A "proof required" challenge has to be solved with
  signal-cli (`submitRateLimitChallenge`); the log names it.
- **A number not on Signal** is said plainly, and so is a contact's
  changed safety number (their identity key). Trust the new key with
  `signal-cli -a <account> trust <number> -a` if you know it's theirs.
- **signal-cli must stay current.** Signal changes its servers now and
  then, and releases older than about three months stop working. Update
  it when doctor or the log says the daemon can't connect.
- **One number, one instance.** signal-cli locks an account's data for one
  daemon, so two instances (M38) on one number fail. Two instances that
  start their own daemons also need different `port`s. Doctor and setup
  name the instance that already has the number or the port.

### Checking it

`ferrule doctor` asks the daemon its version and the account's groups;
nothing is sent. When ferrule starts the daemon and it isn't running yet,
doctor checks that signal-cli is installed and holds the account (and,
for the JVM build, that Java is 21 or newer). It warns when no one is
allowed. `ferrule doctor --offline` only looks for signal-cli.

A live round trip, not run in CI, against a daemon you started:

```sh
FERRULE_LIVE_SIGNAL_URL=http://127.0.0.1:7583 \
FERRULE_LIVE_SIGNAL_ACCOUNT=+972501234567 \
FERRULE_LIVE_SIGNAL_TO=+972541112233 \
cargo test -p ferrule-gateway --test it signal:: -- --ignored
```

It prints the probe and sends `…_TO` one message.

## Mattermost

Ferrule joins a Mattermost server (self-hosted or Mattermost Cloud) as a
**bot account**. It listens on the server's WebSocket and answers through
the REST API (v4), so nothing has to reach your machine: no webhook, no
relay, no open port. It needs Mattermost 5.x or newer.

### What you need

1. **Bot accounts turned on.** A system admin enables them once: System
   Console → Integrations → Bot Accounts → **Enable Bot Account Creation**
   ([docs](https://docs.mattermost.com/configure/integrations-configuration-settings.html#bot-accounts)).
2. **A bot account and its token.** Integrations → Bot Accounts → **Add
   Bot Account**, then **Create New Token** and copy it; it's shown once
   ([how](https://developers.mattermost.com/integrate/reference/bot-accounts/)).
   A person's personal access token works too, but ferrule then answers as
   that person; doctor says so.
3. **The bot in your team**, and in each channel it should answer in
   (`/invite @ferrule` in the channel).
4. **Your own Mattermost account**, to talk to it.

### Steps

The short way is **`ferrule setup` → Mattermost**, or the dashboard's
Mattermost card. Setup:

1. asks the server's address (`chat.example.com` becomes
   `https://chat.example.com`);
2. takes the token and checks it right away: it shows the bot's
   `@username`, and says when the token is a person's rather than a bot's;
3. saves the token to the secrets file and the server to the config;
4. pairs you: send the bot a direct message with the code setup shows. If
   that doesn't arrive, type your username; setup looks up its id.

**Allow a channel** in setup lists the channels the bot is in, across its
teams, and adds the ones you pick. Invite the bot first.

On the dashboard, fill in the card (server, token, users and channels),
**Save**, then **Test**.

By hand, in `config.toml`:

```toml
[gateway.mattermost]
server_url = "https://chat.example.com"
# token_env = "MATTERMOST_TOKEN"         # the default; the value goes in secrets.env
allowed_users = ["@max"]                 # usernames or user ids: whose DMs reach it
allowed_channels = ["c9x3…"]             # channel ids (the channel's menu → View Info)
# stream = true                          # default: [agent] stream
# max_file_mb = 20
```

Usernames in `allowed_users` are looked up when the gateway starts; setup
writes ids.

### Who gets an answer

- **DMs.** An allowed user's direct messages. The chat is known by their
  user id, so an owner notice or a scheduled task's report reaches them
  even before they've written: ferrule opens the DM.
- **Channels.** In an allowed channel, anyone in it can reach ferrule, but
  only by @-mentioning the bot. Ferrule answers **in a thread** under that
  message, and the thread is its own chat: a follow-up in a thread ferrule
  has answered in needs no mention.
- **Strangers get nothing.** A channel that isn't allowed is ignored, even
  with a mention; so are other bots, webhooks and system messages. While
  no one is allowed at all, a stranger's DM is answered once with their id,
  to paste into `allowed_users`.

### What works

- **Text** both ways, up to 16 383 characters per post; a longer answer is
  split. Markdown is Mattermost's own, so it's sent as is.
- **Streaming**: the answer is edited in place every 2 seconds while it's
  written (`stream = false` turns it off).
- **Approvals** as reactions. The post lists each choice with its emoji
  (`:+1:` allow, `:-1:` refuse, then `:one:` `:two:` …), and ferrule adds
  each one so you only click. Only an allowed user's reaction counts, once.
  You can also send the keyword (`yes a1b2`).
- **👀** (`:eyes:`) on your message when ferrule starts on it, and the
  channel is marked read.
- **Files in:** any file posted with the message, into the workspace's
  inbox, up to `max_file_mb` (default 20). A bigger one is refused with a
  reply, and never downloaded.
- **Files out:** uploaded to the channel and attached to the post, five per
  post.

### Limits and errors

- **Rate limits.** On a 429, ferrule waits as long as the server says
  (`X-Ratelimit-Reset`, up to 30 seconds) and retries three times.
- **A token that stops working** (revoked, or the bot deactivated) stops
  the channel, shows in `/status` and fails doctor with "make a new
  token". The other channels keep running.
- **Not a member.** Posting where the bot isn't a member fails with "is
  the bot a member of the channel?"; doctor warns about allowed channels
  the bot isn't in.
- **The server's own limits** apply: the file size limit (System Console →
  File Storage) and whether file sharing is on at all. Ferrule says which
  one refused an upload.
- **Messages sent while the gateway was off** aren't read afterwards: the
  WebSocket only delivers what happens while it's open.
- **One bot, one instance.** Two instances (M38) with one bot token would
  both answer. Doctor and setup name the instance that already has it.

### Checking it

`ferrule doctor` checks the token (`/users/me`), says whether it's a bot
account, and lists allowed channels the bot isn't a member of. It warns
when nobody is allowed. `ferrule doctor --offline` skips the network.

A live round trip, not run in CI:

```sh
FERRULE_LIVE_MATTERMOST_URL=https://chat.example.com \
FERRULE_LIVE_MATTERMOST_TOKEN=… \
FERRULE_LIVE_MATTERMOST_TO=@you \
cargo test -p ferrule-gateway --test it mattermost:: -- --ignored
```

It prints the probe, then sends `…_TO` a message, edits it and reacts 👀.

## HTTP API

For programs rather than people: a script, a shortcut, n8n, Zapier, a CI
job. They POST a message and get the answer back, as one JSON response or
as it's written (server-sent events). What ferrule sends on its own, such
as a scheduled task's result or an approval ask, waits in the program's
**outbox**, and can be POSTed to a **webhook** of its own.

It's **off by default**. When on, it listens on **127.0.0.1 only**
(port 8788). Nothing outside the machine reaches it unless you turn on
`public = "tunnel"` or put your own reverse proxy in front.

### What you need

Nothing outside ferrule. For access from elsewhere: cloudflared, which
setup fetches for you (the same one as the dashboard's remote access), or
your own proxy.

### Steps

The short way is **`ferrule setup` → HTTP API**. It picks the port (and
says when something else has it), creates the first key and prints it
**once** with a `curl` to try, and offers the tunnel.

On the dashboard: the **HTTP API** card. **Save** turns it on; **Create a
key** shows the new key once, with a copy button. The card lists every key,
when it was last used, and a **Revoke** button.

By hand, in `config.toml`:

```toml
[gateway.http]
# port = 8788
# requests_per_minute = 30     # per key; over it: 429 and Retry-After
# bind = "127.0.0.1"           # a container needs "0.0.0.0" (or FERRULE_HTTP_BIND, which wins)
# public = "tunnel"            # a Cloudflare quick tunnel; its URL is in /status
# stream = true                # default: [agent] stream
```

Keys, on the command line:

```sh
ferrule channels keys add n8n                                 # prints the key once
ferrule channels keys add ci --webhook https://ci.example.com/hook   # and a signing secret
ferrule channels keys list                                    # --json for scripts
ferrule channels keys webhook ci https://…    # a new URL and secret; --off clears it
ferrule channels keys revoke n8n              # its next request gets 401
```

A key looks like `frk_…`. Ferrule keeps only its SHA-256, in
`<data>/gateway/http/clients.json`, so a lost key can't be shown again:
revoke it and make another. Keys made or revoked while the gateway runs
count at once; only turning the API on or changing its port needs a
restart.

### Sending a message

```sh
curl http://127.0.0.1:8788/v1/messages \
  -H "Authorization: Bearer $FERRULE_KEY" \
  -H "Content-Type: application/json" \
  -d '{"text": "What changed in the repo today?", "conversation": "daily"}'
```

The answer:

```json
{"id": "r12", "conversation": "daily", "text": "…", "files": []}
```

- **`conversation`** (optional; 1–64 letters, digits and `. _ : -`) is a
  session of its own. Without it, each key has one conversation, named after
  the key.
- **One turn at a time per conversation.** A second request queues behind
  the first.
- **A plain request waits up to 30 minutes.** After that it answers 504,
  and the answer lands in the outbox. It also lands there when the program
  hangs up first: nothing is lost.
- **Files in:** `"files": [{"name": "report.pdf", "data": "<base64>",
  "mime": "application/pdf"}]`, within the 64 KiB body. They're saved to
  the workspace's `inbox/http/`, and the agent is told where.
- **Files out:** `"files": [{"name": "…", "url": "/v1/files/<token>"}]`.
  Fetch the URL with the same key within an hour.

**Streaming:** send `Accept: text/event-stream` and read the events:

| event | data |
|---|---|
| `accepted` | `{"id", "conversation"}` |
| `delta` | `{"id", "text"}`: the **whole** answer so far (an edit can change what came before) |
| `message` | anything else sent to the conversation meanwhile, such as an approval ask |
| `done` | the same object as a plain request's answer, and the stream ends |

A `: keepalive` comment comes every 15 seconds.

### The outbox and the webhook

`GET /v1/events?after=<n>&wait=<seconds>` returns
`{"events": [...], "last": n}`: what ferrule sent this key that no request
was waiting for. That covers task results, notices, approval asks and late
answers. Each event has `n`, `id`, `conversation`, `text`, `files`,
`choices`, `reply_to` (the request it answers, if any) and `ts`. Pass the
last `n` you saw as `after`. `wait` (at most 60) holds the request open
until something arrives. The last 200 are kept per key, across restarts.

With a **webhook**, every outbox event is also POSTed to it as JSON (with
`"client"` added), signed:

```
X-Ferrule-Signature: sha256=<hex HMAC-SHA256 of the raw body, keyed with the webhook secret>
```

The secret (`frw_…`) is printed once when the webhook is set, and it isn't
the API key, so the receiver can check signatures without being able to
call ferrule. Delivery is tried at once, then after 1, 5 and 30 seconds. A
delivery that still fails shows as a problem in `/status` and on the card,
and the event stays in the outbox. The URL must be `https://`, or `http://`
to this machine.

### Who gets an answer

Anyone with a key. There's no allowlist beyond the keys: each key is one
program, and its name is who it is. **Revoke** a key to shut a program out.

**The owner.** `[trust] http_owner = "<key name>"` makes that program the
owner on this channel. Its own conversation (no `conversation` field) can
then run owner commands, and approvals and owner notices go to its outbox
and webhook. It's unset by default: a program is never the owner unless
you say so.

**Approvals:** an ask comes with
`"choices": [{"label": "Allow", "reply": "yes a1b2"}, …]`. Answer by POSTing
the `reply` as the text.

### Limits and errors

| Status | When |
|---|---|
| 400 | not JSON, empty `text`, a bad `conversation`, a file that isn't base64 |
| 401 | an unknown or revoked key (`WWW-Authenticate: Bearer realm="ferrule"`) |
| 404 | an unknown path, or a file link that's expired or another key's |
| 405 | a wrong method on `/v1/messages` or `/v1/events` |
| 413 | a body over 64 KiB |
| 429 | over `requests_per_minute` for this key; `Retry-After` says when |
| 504 | no answer within 30 minutes; it will come in the outbox |

- **The address.** The API listens on `127.0.0.1` unless `[gateway.http]
  bind` or the env `FERRULE_HTTP_BIND` (which wins) names another IP
  address, such as `0.0.0.0` in a container behind a proxy
  ([docker.md](docker.md)). A name isn't accepted, only an address. Like the
  port, it is read at start.
- **A port in use** stops only this channel, with a problem naming
  `[gateway.http] port`. The other channels keep running.
- **A broken `clients.json`** refuses every key (401) and shows as a
  problem until it's fixed.
- **Public.** The tunnel's URL changes on each start. It's a quick tunnel,
  meant for trying things out. For a fixed address, use a named Cloudflare
  tunnel or your own proxy to `127.0.0.1:<port>`.
- **One port, one instance.** Two instances (M38) on one port: the second
  can't listen. Doctor and setup name the instance that already has it.

### Checking it

`ferrule doctor` says whether the gateway answers on the port (a 401 with
ferrule's realm), whether the port is free, or what else is on it. It
counts the keys and webhooks, and warns when there's no key. With
`public = "tunnel"`, it says whether cloudflared is here.

## Chat commands

These work in every chat, answered by the gateway itself (the model never
sees them):

| Command | What it does |
|---|---|
| `/new` (or `/reset`) | Starts a fresh conversation in this chat. A running turn is stopped, and messages waiting behind it are dropped (the reply says how many). The old transcript stays on disk as `sessions/<chat>.<UTC time>.jsonl`. It's kept, not replayed. Memory, tasks and settings stay. |
| `/status` | What ferrule is doing right now. |
| `/help` | The commands this chat answers. |
| `/stop`, `/resume` | Stop every run now; let runs start again. |
| `/plan <task>`, `/undo` | Explore first, then ask; revert the agent's last commit. |
| `/model` | Show or switch the model. |
| `/update`, `/restart`, `/doctor` | Check for a new Ferrule and install it; restart; check the setup and offer fixes. Each asks first and tells you in the same chat when it's done ([updates.md](updates.md#from-a-chat)). |
| `/login`, `/logout`, `/connect`, `/connections`, `/skills`, `/mcp`, `/hooks`, `/caps` | Plans, services, extensions and spending caps. |
| `/dashboard` | A link to the dashboard (the owner's private chat). |

`/help` and Telegram's menu are one list, so a command that works is in
both. `/help` lists the ones this gateway has; the owner-only commands are
only shown to the owner.

When a turn fails the same way twice in a row (the same 4xx from the
model), the error ends with a hint to send `/new`. The conversation itself
is then the likely problem: something in it that the provider rejects, or
a history that no longer fits.

Telegram's `/` menu is set when the gateway starts, per scope: groups and
anyone else get `/new`, `/stop`, `/status` and `/help`; private chats, and
the owner's own chat even if those were cleared, get the full list. Discord
gets `/new` and `/help` as slash commands when `ferrule setup` registers
them ([discord.md](discord.md)). `ferrule chat` takes `/new` too.

### Changing Ferrule from the conversation

The owner's chats give the agent a `ferrule_admin` tool: switch the default
model or a fallback, set a spending cap, turn a skill or MCP server on or
off, trust a workspace's hooks, check for and install an update, restart.
It runs in the gateway, not in the shell sandbox, so it can write
`ferrule.toml` even where the shell can't. Every change shows the owner the
exact change on a card first and waits for a tap (Allow on Telegram, a
reply elsewhere); an approval covers that one change, in that chat, once.
The agent never asks you to paste a command or open a terminal for
something it can do this way. Only the owner's own chat (and the dashboard's Chat) gets the tool; groups and other people don't.
Nothing secret is ever printed on a card or in a result.

## Photos

A photo is saved to the workspace's inbox like any file and then handed to
the model **as a picture**, if the model can see one (see [Photos: which
models see them](models.md#photos-which-models-see-them)). A caption is the
message's text. If the model can't see images, the chat is told so once, in
a sentence, and the agent still gets the saved file's path.

- **Telegram** sends the largest size Ferrule will take (at most 2000 px on
  the long side and within the size cap). An album is read as its first
  photo only; the bot says so once per album, and you send the others one
  at a time.
- **The dashboard's Chat** has an attach button (also in the command
  palette). The page shrinks the photo to 1568 px and re-encodes it as
  JPEG in your browser, which also drops its EXIF and GPS data, before
  anything leaves the phone. The server takes JPEG, PNG, WebP and GIF up
  to 3.5 MB, checks the file's first bytes, and refuses SVG.
- The other channels that save files (WhatsApp, Matrix, Mattermost, Signal,
  email, the HTTP API) hand images to the model the same way.

## Voice messages

A voice note or audio file sent to ferrule is transcribed before the agent
sees it. The agent gets the text and the saved file's path:

```
[voice message, 0:14, transcribed]: בוא נקבע את הפגישה ליום שלישי
[file saved: inbox/telegram/2026-09-29/voice-1234.ogg (audio/ogg, 21 KB)]
```

This works on every channel that saves files into the inbox: Telegram
(voice notes and audio files), WhatsApp, Matrix, Mattermost, Signal, email
and the HTTP API. Discord and Slack don't download files yet.

The gateway makes the call itself, so the key never enters the sandbox. No
language is assumed; the backend detects it unless you pin one.

**Turning it on.** With an OpenAI API key set (`OPENAI_API_KEY`, or a
`[providers.X]` on `api.openai.com` whose key is set), it's on with no
config at all. Otherwise, pick a backend in `ferrule.toml`:

```toml
[transcription]
backend = "openai"                       # auto (the default) | openai | command | off

# Any OpenAI-compatible endpoint, e.g. Groq:
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"             # "" for a local server with no key
model = "whisper-large-v3"               # default: whisper-1
# provider = "groq"                      # or borrow a [providers.X]'s URL and key
# language = "he"                        # unset: detected
# price_per_minute = 0.006               # USD, for the ledger; 0 when local
# timeout_secs = 120
```

Or a program on this machine, such as whisper.cpp. `{file}` is the audio
file's path. There's no shell, `~` is expanded, stdout is the transcript,
and the program is killed at `timeout_secs`:

```toml
[transcription]
backend = "command"
command = "whisper-cli -m ~/models/ggml-small.bin -nt {file}"
```

Ferrule doesn't convert audio. The file goes up as it came (OGG/Opus from
Telegram and WhatsApp; OpenAI and Groq accept it). If a backend refuses the
format, the reply says so.

**When it can't transcribe**, the file is still saved, and the agent still
runs with a note saying why:

- **Off** (no key and no backend): the sender is told how to turn it on,
  once per chat while the gateway runs. If the only models are ChatGPT or
  Claude subscriptions, the message adds that a plan doesn't come with an
  API key for this.
- **The backend failed** (down, a refused format, a timeout): the sender
  gets the reason in one line.

Each transcription is a ledger row (`call_kind = "transcription"`), costed
at `price_per_minute` when the length is known. `ferrule doctor` has a
`voice` line: which backend is active and why, or how to turn it on.

## Typing indicator

While a turn runs, the chat shows that ferrule is typing. The indicator is
refreshed until the reply goes out, and stops then, on `/stop`, and on an
error:

| Channel | How | Refreshed every |
|---|---|---|
| Telegram | "typing…" | 4 s |
| Discord | "Ferrule is typing…" | 8 s |
| Matrix | the room's typing notice, cleared at the end | 25 s |
| WhatsApp | the Cloud API's typing indicator, which also marks the message read | 20 s |

Slack, Mattermost, Signal, email and the HTTP API have none. If a channel
answers a typing call with a rate limit (429), typing stops for the rest of
that turn, so it never competes with the reply. To turn it off:

```toml
[gateway]
typing = false
```
