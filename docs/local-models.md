# Local models

ferrule works with a model served on your own machine or LAN by
**Ollama**, **llama.cpp** (`llama-server`), **LM Studio** or **vLLM**.
They all speak the OpenAI chat API, so each is an ordinary provider with
no key. This guide covers what ferrule checks so the first run works, and
how to fix what it finds. The design is in
[m34-ssh-local.md](m34-ssh-local.md) (§10–§15).

ferrule doesn't install a server or download models. Start the server and
pull a model first (`ollama pull qwen3-coder:30b`).

## The one problem that matters: a shrunk window

A model trained for 128K tokens is often *served* with far less. Ollama's
default is 4096 on most laptops. When a prompt is longer than the
server's window, the server doesn't fail: it quietly drops the front of
the prompt, which holds the agent's instructions and its tools. The agent
then acts confused and nothing says why.

ferrule reads two numbers for each model:

- **trained**: what the model can hold;
- **effective**: what the server will actually give a request.

It plans for the effective one, and says so when it's smaller than what
it would otherwise plan for.

| effective window | what ferrule says |
|---|---|
| smaller than the planned window | **fail**: the silent-truncation case, with the fix |
| under 8192 | fail: the agent can't hold its own instructions |
| under 16 384 | too small for agent work |
| under 32 768 | works, but compacts often |

The planned window is the provider's `[providers.X.models."m"]
context_window` when it's set, else the profile's window (128 000 for
`generic`). When the window is small, ferrule also shrinks the output
reserve to a quarter of it, and below 16K compacts at 80% instead of the
profile's threshold. Hosted models with large windows are unaffected.

## Set it up

**`ferrule setup` → Model provider → Add a provider** starts by looking
for servers on the usual ports, in parallel, 0.7 s each:

| server | where | recognized by |
|---|---|---|
| Ollama | `OLLAMA_HOST`, else `127.0.0.1:11434` | `/api/version` |
| llama.cpp | `127.0.0.1:8080` | `/props` |
| LM Studio | `127.0.0.1:1234` | `/api/v1/models` (0.4), `/api/v0/models` (0.3) |
| vLLM | `127.0.0.1:8000` | `/v1/models` with `owned_by: "vllm"` |

Each one it finds is offered first ("Ollama 0.34.4 at 127.0.0.1:11434 ·
3 models (running here)"), with every model's trained and effective
window. A known-good model (below) is the default pick. After you pick,
setup:

1. makes **one tool call** to the model (it loads the model, which can
   take a minute) and says whether tool calling works;
2. reads the window the server really gives;
3. if the window is under 32K, offers the fix. On Ollama it can make the
   fix itself, **only on your yes** (see below). On the others it prints
   the command;
4. writes the window as the model's `context_window`, so ferrule plans
   for it.

A server on another port or another machine on your LAN works too: add
it as a custom endpoint. Any provider whose `base_url` is loopback or a
private address gets the same checks.

## Fixes, per server

**Ollama.** The OpenAI-compatible endpoint can't raise the window per
request, so the fix has to live on the server side. Pick one:

1. **A derived model** (what setup offers). It creates `<model>-32k`,
   the same weights with `num_ctx 32768`, via Ollama's `/api/create`.
   Your model and Ollama's settings stay as they are, and the config then
   names the new model. By hand:
   ```
   printf 'FROM qwen3-coder:30b\nPARAMETER num_ctx 32768\n' > Modelfile
   ollama create qwen3-coder:30b-32k -f Modelfile
   ```
2. **The server's default.** Set `OLLAMA_CONTEXT_LENGTH=32768` in the
   Ollama server's environment and restart it (`systemctl edit ollama` on
   Linux, `launchctl setenv` on macOS, the system environment on Windows).
   ferrule prints this and never changes it for you.

For Ollama the effective window is known only once the model is loaded.
Before that, doctor says "the window is known once Ollama loads it", and
`--ping-models` loads it.

**llama.cpp.** Restart `llama-server` with `-c 32768 -np 1`. The window
is split across the `--parallel` slots, so four slots of a 32K context
give each request 8K.

**LM Studio.** `lms load <model> --context-length 32768`.

**vLLM.** Restart with `--max-model-len 32768`. For tool calls it also
needs `--enable-auto-tool-choice --tool-call-parser <parser>`.

## Tool calling: can't vs broken

The probe sends one tool, `lookup_order`, and asks the model to use it.
The answer falls in one of three cases:

| what comes back | verdict | what to do |
|---|---|---|
| a real tool call | works | nothing |
| the server refuses tools, or the model answers in plain text | **can't call tools** | pick a model that supports tools, or start the server with its tool flags (vLLM, above) |
| text that *looks* like a tool call (`<tool_call>`, `{"name": …`, `[TOOL_CALLS]`) | **template broken** | the model tried, and the server's chat template didn't turn it into a call. Update the server, re-pull the model (Ollama), or start llama.cpp with `--jinja` or the model's own `--chat-template-file` |

On Ollama, a model whose capabilities don't list `tools` is "can't call
tools" without a call.

## Known-good models

As of **2026-09-26**, from the Ollama library, where each is tagged
`tools`. They were **not** run through ferrule's probe or eval for this
list (the build machine has no GPU). The evidence is the library's
capability flag, and the probe you run in setup is the real check.

| model | size (default tag) | trained window | note |
|---|---|---|---|
| `qwen3-coder:30b` | 19 GB | 256K | MoE, fast on 24 GB+ |
| `gpt-oss:20b` | 14 GB | 128K | reasoning; fits 16 GB |
| `devstral:24b` | 14 GB | 128K | agentic coding |
| `qwen3.6:27b` | 18 GB | 256K | general + tools |
| `granite4.1:8b` / `:3b` | 5.3 / 2.1 GB | 128K | small machines; expect weaker tool use |

Setup shows this list when your Ollama has none of them.

## Check it

- **`ferrule doctor`**, for each local provider: the server and version,
  and each model's effective, trained and planned window, with the fix
  when they don't fit. `--ping-models` adds the tool probe. `--offline`
  skips all of it.
- **`/status`** (in the gateway) checks the default model's server at
  start and every 10 minutes, and runs the tool probe once at start. It
  shows lines only when something is wrong:
  - `local: qwen3-coder:30b window 4096 < 128000 planned (Ollama drops the front of long prompts)`
  - `local: qwen3-coder:30b can't call tools (template broken)`
- **The dashboard** lists the same lines as problems.

See also [models.md](models.md) for connecting and naming models.
