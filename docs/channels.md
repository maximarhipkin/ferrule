# Channels

Ferrule answers you where you already talk. One gateway daemon runs every
channel that has credentials. Every channel keeps the same rules:

- **You are the owner.** The first person you pair becomes the channel's
  owner, and only the owner can approve a tool call.
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
[m39-channels.md](m39-channels.md).

## Which channel to pick

| You want | Pick |
|---|---|
| The quickest start, on your phone | Telegram |
| Ferrule in the chat app your customers or family already use | WhatsApp (a business number, from Meta) |
| An open, self-hostable chat, or a room shared with a team | Matrix (unencrypted rooms only) |
| Nothing new to install: write to it like a colleague, or get task results in your inbox | Email (a mailbox of its own) |

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
cargo test -p ferrule-gateway --test whatsapp -- --ignored
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
cargo test -p ferrule-gateway --test matrix -- --ignored
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
cargo test -p ferrule-gateway --test email -- --ignored
```

It logs in, prints the probe and sends `…_TO` one mail.
