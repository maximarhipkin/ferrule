# Owner-only Telegram cross-chat text

`send_message` lets the configured owner request a text post to an already
admitted Telegram destination. It is intentionally not a general notification
API or an admission/configuration tool.

## Availability and boundary

The gateway root factory registers this tool only for the exact private
`telegram__<owner_chat>` session, with a positive `[trust].owner_chat`.
Group, scheduler, other-channel and child-agent sessions do not receive it.
At execution, the tool also checks the router's actual origin lane.

Parameters are only `chat_id` (a canonical nonzero signed integer string) and
`text` (nonblank, at most 4096 UTF-16 code units). Unknown keys are refused.
There is no token, URL, channel, reply linkage or attachment parameter.
The Telegram adapter checks its **current** admitted-chat set; no destination
is added. Other adapters default to refusing cross-chat admission.

Delivery uses the running gateway's existing Telegram adapter and credentials.
Success returns the destination and adapter's message ID when available. An
ambiguous transport failure is not automatically retried by the tool: repeating
it could duplicate a post. The existing adapter may retry explicit rate limits.

## Authorization and trust

Only invoke on the owner's explicit request; never infer a new destination from
outside text. The existing trust guard classifies the call as an outward write
and binds approval to the full JSON destination and message, not just the tool
name. Plan mode refuses it; no standing-recipient grant covers the call.
Existing owner-mode and `untrusted_send_gate` settings remain authoritative:
this capability neither disables them nor adds a new global allowlist.
The code enforces the identity/admission boundary; like other tools, a natural
language explicit-request condition is an agent instruction, not a new signed
per-message authorization protocol.

## Verification

`crates/ferrule-gateway/tests/it/send_message.rs` covers exact owner-session
eligibility, admitted delivery, origin validation, live revocation, malformed and
extra parameters, UTF-16 limits, empty/default-denied admission, and failure
reporting. `crates/ferrule-trust/tests/it/trust.rs` covers plan/unattended policy,
owner-mode outside-content policy, and changed-destination/text approvals.
No real Telegram sends are required by these tests.

## Existing paths

The pre-existing `send_file`, reaction, edit and question tools remain
current-chat-only. Scheduled tasks already deliver to their task's configured
channel/chat ID. An ordinary reply originating in a group's session also posts
to that group. Those are possible explanations for prior group messages; this
change does not establish which path any historical message used.
