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
