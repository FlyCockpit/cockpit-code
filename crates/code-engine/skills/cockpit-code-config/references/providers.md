# providers/<id>.json

One provider per file. The file stem is the provider id. A model is `<id>/<model key>`; model keys may contain `/`.

## Provider fields

`tools.profile` is `full` or `core`, inherited from global config.json; a model's
`tools.profile` wins. It can coexist with per-tool overrides in the same object:
`"tools":{"profile":"core","grep":{"name":"find_text"},"web_search":{"enabled":false}}`.
Full is the default. Profile/catalog freezing and script discovery are described
in [config-json.md](config-json.md).

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | `anthropic` \| `openai-chat` \| `openai-responses` \| `strictTools` | bool | Responses true, other dialects false | Per-model override supported. Frozen for the session; command toolsFile includes a strict hint. |
| `usage` | `{argv,ttlSecs}` | none | All provider types: background cockpit-code.usage/1 account reader; TTL defaults to 300 seconds. |
| `budget` | `{usd,period}` | none | Positive USD, UTC day/week/month; advisory warnings at 80%/100%, never blocks calls. |
| `command` | required | API dialect. Never autodetected; use `cockpit-code doctor --probe <id>` for a suggestion. |
| `label` | string | id | Shown in `/model`. |
| `baseUrl` | string | required for HTTP types | `${VAR}` allowed. Prefix before `/v1/messages`, `/chat/completions` or `/responses`. |
| `headers` | object | `{}` | `${VAR}` allowed. Put credentials here: `"Authorization": "Bearer ${KEY}"` or `"x-api-key": "${KEY}"`. |
| `enabled` | bool | `true` | `/model` can also toggle it per session. |
| `allowInSubagents` | bool | `true` | `false` hides all its models from subagents. |
| `promptCaching` | `auto` \| `1h` \| `off` | `auto` | Anthropic cache policy. |
| `timeouts` | `{firstTokenSecs, idleSecs}` | `120`, `300` | Both must be positive. |
| `maxConcurrentContexts` | positive int | unlimited | For servers that hold one model/context at a time (llama.cpp, llama-swap). Requests queue for a slot; shared by all the provider's models. |
| `reportedCost` | object | none | Read billed cost from responses; see below. A model can override it. |
| `command` | object | none | Only for `type: "command"`; see `command-providers.md`. Replaces `baseUrl`/`headers`. |
| `models` | object | `{}` | Model key → model entry. |

## Model fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | dialect | provider's | Per-model override when one gateway needs different dialects. |
| `label` | string | key | |
| `description` | string | `""` | One line: strengths, speed/cost, when to use. Agents choose subagent models from it. |
| `contextWindow` | int | unknown | Tokens. Drives the context meter and compaction. |
| `maxOutputTokens` | int | wire default | Must be positive. Reserved from the context window for output. |
| `reasoning.efforts` | string[] | `[]` | The vendor's own effort names, e.g. `["low","medium","high","xhigh"]`. |
| `reasoning.default` | string | none | Must be one of `efforts`. |
| `reasoning.normalized` | object | `{}` | Common scale (`low`, `medium`, `high`, `max`) → vendor name, e.g. `{"max": "xhigh"}`. |
| `reasoning.budgets` | object | `{}` | Anthropic `thinking: manual` only: effort → token budget (≥1024 and < `maxOutputTokens`). |
| `thinking` | `adaptive` \| `manual` \| `none` | none | **Required** for Anthropic models that declare efforts. |
| `priorThinking` | `keep` \| `drop` \| `summarize` | `keep` | How earlier reasoning is sent back. Non-`keep` is rejected for `anthropic`, `openai-responses`, and for DeepSeek or OpenRouter chat (they need complete replay). |
| `thinkingSummaryModel` | `provider/model` | safety model | Who summarizes for `priorThinking: summarize`. |
| `textToolRecovery` | bool | `false` | Accept tool calls wrapped in `<tool_call>` text for weak tool-callers. |
| `capabilities.input` | subset of `text`,`image`,`video`,`audio`,`pdf`,`file` | `[]` | UTF-8 attachments are always inlined as text. Other binaries require their media type, or `file` for non-media files. |
| `capabilities.output` | subset of `text`,`image`,`video`,`audio` | `[]` | Use `["text"]` for normal chat models. |
| `capabilities.tools` | bool | `false` | Native tool calling. |
| `allowInSubagents` | bool | provider's | |
| `cost` | `{inputPerMTok, outputPerMTok, cacheReadPerMTok?, cacheWritePerMTok?}` | none | USD per million tokens; finite and ≥ 0. |
| `strictTools` | bool | provider/dialect default | Override provider strictness for this model; fixed for the session. |
| `billing` | `usage` \| `subscription` | `usage` | `subscription` without `cost` counts as $0. |
| `reportedCost` | object | provider's | Overrides the provider's locator. |
| `benchmarks` | object | `{}` | Benchmark key → number. Register keys in `config.json` `benchmarks`. |
| `compactAt` | number 0..1 | 0.8 | Subagent autocompaction threshold. |
| `selfEvaluateWrites` | positive int | `1` (off) | N > 1: for each `write`/`edit` this model proposes, N candidates are generated in hidden forks sharing the author's cache and an evaluator picks one. Costs extra requests. |
| `maxConcurrentContexts` | positive int | unlimited | Per-model concurrency limit (see provider field). |
| `imageRequest` | `modalities` \| `tool` | `modalities` | Media models only: how `generate` asks for an image. `modalities` sends `{"modalities": ["image", "text"]}` (models with native image output, e.g. via OpenRouter); `tool` sends `{"tools": [{"type": "image_generation"}]}` (OpenAI's own API). |
| `generateParams` | object | none | Media models only, escape hatch: JSON merged last into the `generate` request body; its keys override the `imageRequest` defaults. |

## Reported cost

Cost precedence: reported cost, then `tokens × cost` rates, else unknown (totals show `$X+`).

```json
"reportedCost": { "from": "body", "pointer": "/usage/cost", "unit": "usd" }
"reportedCost": { "from": "header", "name": "x-litellm-response-cost", "unit": "usd" }
```

`pointer` is a JSON pointer checked against the response (every SSE event for streams; last match wins). `unit` is `usd` or `usd_micros`.

## Who can use a model

- **Chat (root) model:** any enabled model with `text` in `capabilities.output`.
- **Subagent model:** provider enabled, provider and model `allowInSubagents` not false, not toggled off in `/model`, and `capabilities.output` exactly `["text"]`.
- **Media model:** any `image`, `video` or `audio` in `capabilities.output`. Never a subagent model. Reachable through the `generate` tool, which is offered only when at least one enabled model outputs media. Supported wires: `openai-responses` and `openai-chat` (see `imageRequest`), and command providers that write `{outputFile}`.

Media model example (OpenRouter as the documented example):

```json
"google/gemini-3.1-flash-lite-image": {
  "label": "Gemini 3.1 Flash Lite Image",
  "description": "Cheap image generation and editing.",
  "capabilities": { "input": ["text", "image"], "output": ["image", "text"], "tools": false }
}
```

placed in a provider with `"type": "openai-responses"` and `"baseUrl": "https://openrouter.ai/api/v1"`.

## Choosing `type`

| Endpoint the vendor documents | `type` |
|---|---|
| `POST /v1/messages` (Anthropic Messages) | `anthropic` (baseUrl without `/v1`) |
| `POST /v1/chat/completions` | `openai-chat` (baseUrl ends in `/v1`) |
| `POST /v1/responses` | `openai-responses` (baseUrl ends in `/v1`) |

If unsure, write one model, run `cockpit-code doctor --probe <id>`, and use a suggested type. When several dialects are accepted, prefer the vendor's native one (Messages for Anthropic models, Responses for OpenAI reasoning models) and ask the user if it is unclear.

Idle compaction (TUI/ACP only, root only): provider defaults and model overrides
`autocompactAtIdle` (optional positive seconds), `autocompactAtIdleMinPercent`
(default 30, 0–100), `cacheTtlSecs` (positive, default 300). Prefer an idle delay
shorter than the cache TTL; doctor warns otherwise. Busy trees/dialogs and typing
reset the timer; at most one compaction runs per idle period until a new turn.

Cold follow-ups: `cacheTtlSecs` (300) and `coldResumeMinTokens` (20000) are provider
defaults with model overrides. task continue context auto starts fresh only when
both idle time exceeds TTL and prior input exceeds the token threshold; resume
always retains context and fresh always creates a seeded context on the same child.

`variationFocuses: ["security", "clarity"]` assigns perspectives to variation
slots for selfEvaluateWrites and auto-mode improve. Safety-model focuses have
precedence for improve, then these model focuses, then a generic alternative.
The original is candidate 0; a judge may select or synthesize validated arguments.

Provider defaults/model overrides `modelPrompt`, `modelReminder`, and
`modelReminderTurns` (positive, default 10) steer agent behavior. The prompt is
frozen for the session after the role and before AGENTS.md; reminders append every
N completed model turns. Safety reviewer, title and thinking-summary roles exclude
this steering. Start a new session to apply changed steering text.
