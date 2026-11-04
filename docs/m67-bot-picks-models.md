# M67 — the bot picks the model for each job

The owner connects OpenRouter once. With one switch on, the bot picks
the OpenRouter model each job needs: a sub-agent on a model suited to
its task, an image or a video on the model best for it. No new key and
no model connected by hand first.

## The switch

`[models] bot_picks` (off by default), on the dashboard's **Config**
page, in the **Models** group: "Let the bot pick the best OpenRouter
model for each job". The gateway reads it when it starts, so the page
asks for a restart after the change.

Off, nothing changes (M21): `spawn_agent` takes a connected model only
when the owner asked, and the media tools take `[media]`'s default and
its `image_models` / `video_models` lists.

## What it changes

- **`spawn_agent`'s `model`** (`ferrule-cli/src/agents.rs`): the agent
  may name a model whenever the job needs a different strength. A model
  that isn't connected but looks like an OpenRouter id
  (`vendor/model[:variant]`) is connected on the OpenRouter provider
  for the spawn (`Models::add_model`, written to the config, shown on
  **Models** like any other). A model `[models] deny` names, connected
  or not, is still refused. The parameter's description says so
  (`Supervisor::set_model_hint`).
- **`generate_image` and `generate_video`'s `model`**
  (`ferrule-gateway/src/tools/media.rs`, `MediaLimits::any_model`): any
  model id, not only the lists. OpenRouter answers when it has no such
  model. `[media] max_per_day`, `max_videos_per_day`,
  `max_usd_per_video` and `[trust]`'s dollar caps still hold, and a
  video whose price can't be estimated is still refused while a dollar
  cap is set. A reference image still goes only to a model in
  `[media] reference_models`.
- **`find_models {kind, search?}`** (`ferrule-gateway/src/tools/find_models.rs`),
  offered in the chat and to a media sub-agent while the switch is on:
  OpenRouter's models for a job, newest first, at most 40, each with
  its price and context. `text` lists the models that take tools (a
  sub-agent needs them), `vision` the ones that also take images,
  `image`, `video` and `speech` OpenRouter's lists for those
  (`MediaBackend::models`, read through the proxy and kept 6 hours like
  the price lists).

## Each job's default, on the page

The same **Models** group has the default model for each job, as
OpenRouter ids: `media.image_model`, `media.video_model`,
`media.vision_model` (the eyes for a model that can't see, the Claude
plan) and `media.speech_model`. The bot can change them with
`config_set` when asked; the owner approves the card, as for every
field there.

## Not in scope

- Per-role defaults (`[agents.roles.<role>] model`) stay config-only:
  a role's model must be connected when the gateway starts, and the
  form can't check that. With the switch on, the agent names the model
  per spawn instead.
- Named agents the owner defines, each with its own model and
  instructions: still M12 part 6.
- Video editing isn't a model on OpenRouter: it's the `video-editing`
  skill's ffmpeg recipes (M65 §2), on whatever model runs the agent.

**Not verified live.** No spawn or media call has run with the switch
on; OpenRouter's `/models` fields (`supported_parameters`,
`architecture.input_modalities`, `created`) are read from its docs.
