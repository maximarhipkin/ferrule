# Images the model sees

Two kinds of message carry images: a photo someone sends in a chat (M47,
[models.md](models.md), "Photos: which models see them") and, since M60,
a tool's result. Both keep the image as a file on disk: the history and
the transcript hold its path, type and name (`ferrule_core::ImageRef`),
never its bytes. The driver that serves a model call reads the file and
decides what goes on the wire: the pixels, or a one-line note.

## Which tools return images

- **`read_file`** on a PNG, JPEG, GIF or WebP file. The type is read from
  the file's first bytes, not its name, so a script called `x.png` is
  read as text and SVG never counts. The text result is
  `[image <path>: image/png, <n> bytes]`, and the image rides with it.
  Only the local workspace's `read_file`: an SSH workspace's (`ferrule-ssh`)
  still reads text only.
- **MCP tools** that return `image` content, such as the browser's
  screenshots. These parts were already saved under
  `<workspace>/.ferrule/mcp-content/` and named in the text
  ([browser.md](browser.md), "Limits"); each saved part that is an image
  now goes to the model too.
- **`generate_image`** ([models.md](models.md), "Making media"): the image
  it saved under `media/`, so the model can check what it made before
  saying so. That costs an image's tokens per call.
- **`screenshot`** (M61, [m61-screen.md](m61-screen.md)): a capture of
  the owner's own Mac screen, offered only in the owner's chats with
  `[screen]` on.

A tool adds an image with `ToolOutput::with_images`; the agent puts the
images on the tool's result message (`Message::images`).
`ferrule_core::vision::image_file` is the check every tool above uses: is
the file an image by its bytes, and is it under the size cap.

## How each API gets them

| API | Where a tool's image goes |
|---|---|
| Anthropic Messages | inside the `tool_result`: its text, then an `image` block (base64) per image |
| OpenAI Chat Completions (and compatible servers) | a `tool` message takes only text, so the result's text goes there, and one `user` message follows the last result of the batch with the images as `image_url` parts |
| OpenAI Responses, and the ChatGPT plan's Codex backend | the `function_call_output` gets the text, and one `user` message follows the batch's outputs with `input_image` parts |

The `user` message that carries a batch's images starts with a line saying
what it is: `[Tool output, not a message from the user: the images the
tool call(s) <ids> returned, in order.]`. It goes after every result of
the batch, since Chat Completions wants an assistant's tool calls answered
by tool messages with nothing in between.

Responses can also take images inside a `function_call_output`. Ferrule
uses the user message there instead because one body builder serves the
Codex backend too, and a user message with `input_image` parts is the
form it already sends there for photos (`responses.rs`, `codex.rs`).

## Models that can't see images

Whether a model sees images is the provider's call, per request
(`Provider::sees_images`: the `vision` key in the config, else the model's
name; [models.md](models.md)). A model that can't gets a note in place of
each image, after the tool's own text:

```
[The tool returned an image, chart.png, but this model can't see images.]
```

If a provider answers a request with a 400 that names images, the request
is sent once more with notes, and that provider gets notes from then on
(M47's rule, now covering tool images as well).

## Only the newest few, as pixels

At most four images go as pixels in a request
(`ferrule_core::vision::MAX_IMAGES`): the newest four of the
conversation, photos and tool images counted together. An older tool
image becomes a note in place:

```
[image omitted: image-1761050000000-3.png, an earlier tool image]
```

Only the pixels go. The result message stays where it is, with its text,
so a call and its result are never split; compaction and truncation work
on messages as before. When compaction marks a result superseded by a
later identical one (two reads of the same file), its images go with
its text.

A cost to know: when a fifth image arrives, the oldest one's message turns
from pixels into a note, so that request rewrites history from there, and
the provider's prompt cache misses from that message on. Photos have had
the same trade since M47.

## Size

The workspace has no image library, so ferrule itself shrinks or
recompresses nothing (`screenshot`'s smaller copy is made by macOS's
`sips`, below). An image over **3.5 MB**
(`ferrule_core::vision::MAX_IMAGE_BYTES`, about 4.7 MB as base64, under
the 5 MB per image Anthropic's API takes,
[vision docs](https://docs.claude.com/en/docs/build-with-claude/vision))
isn't sent:

- `read_file` refuses it: "… an image (image/png) of 5.2 MB, over the
  3.5 MB cap on an image sent to the model, so it isn't shown; a smaller
  copy (resized with a shell command) can be read".
- An MCP result gets a line naming the file and why it isn't shown.
- `generate_image` adds "It isn't shown to you: …" to its answer.
- `screenshot` keeps the PNG whole for the owner and shows the model a
  JPEG copy at most 1600 pixels on its long side, made with macOS's
  `sips`.

The driver checks again when it sends (the bytes are still an image,
still under the cap); a file that has gone or changed becomes a note
saying so. Pixel dimensions aren't checked: when a provider refuses an
image with a 400 whose message names images, the retry above applies.

## Context and transcripts

- Each image that would go as pixels counts 1,600 tokens
  (`vision::IMAGE_TOKENS`) in the estimate that decides when to compact.
- A transcript line holds the image's path, type and name: a few hundred
  bytes, not the megabytes of base64 a request carries. The file has to
  still be there for a later request, or a resumed session, to send it;
  if it isn't, the note says so. Nothing prunes `.ferrule/mcp-content/`
  yet ([browser.md](browser.md)).

## Not covered

- The Claude Code plan engine is text-only: its own tools run inside it.
- The dashboard's chat doesn't show a tool's images.
- No live run against the providers yet: the wire shapes are checked by
  the code paths above, not by a real Anthropic, OpenAI or ChatGPT-plan
  call.
