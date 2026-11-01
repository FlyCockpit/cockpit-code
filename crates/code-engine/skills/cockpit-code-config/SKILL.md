---
name: cockpit-code-config
description: Add or change cockpit-code providers, models, benchmarks, MCP servers and config.json settings.
---

# cockpit-code configuration

Use this skill when the user asks to add, change, enable or remove a cockpit-code **provider**, **model**, **benchmark**, **MCP server**, **web provider** or any **config.json** setting.

## Where the files are

Run `cockpit-code doctor` first. Its first line prints `CONFIG=<dir>`. All files live there:

```
CONFIG/config.json          global settings (one file, no project layering)
CONFIG/providers/<id>.json  one provider per file; <id> is the file stem
CONFIG/mcps/<id>.json       one MCP server per file
```

`CONFIG` is normally `~/.config/cockpit-code/`, or `~/.cockpit-code/` when `~/.config` does not exist, or `$COCKPIT_CODE_HOME`. Always use the path printed by `doctor`.

A model is addressed as `<provider id>/<model key>`, for example `openrouter/anthropic/claude-haiku-5.5` (the provider file `openrouter.json` with the model key `anthropic/claude-haiku-5.5`).

## Credentials: always `${ENV}`

- **Never** write an API key into any file. Reference an environment variable instead: `"headers": {"Authorization": "Bearer ${MY_API_KEY}"}`.
- `${VAR}` is expanded only in provider `headers`, `baseUrl`, `command` (argv, stdin, cwd, env, discover argv), MCP `command`/`url`/`headers`/`env`, and `web.apiKey`.
- Do not read `.env` files, run `env`/`printenv`, or echo a key to check it. Tell the user which variable to export (for example `export MY_API_KEY=...` in their shell profile) and let them do it.
- An unset variable makes that provider or MCP file fail to load; `doctor` lists it under `skipped ...` and names the variable, never its value.

When you need model metadata (context window, max output, pricing, capabilities or modalities) for provider/model entries, look it up from models.dev; see [the endpoint and field-mapping reference](references/models-dev.md). `discover` and the provider's own API take precedence.

`cockpit-code init --provider <template> --key-env VAR --default-model provider/model --default-mode auto --safety-model provider/model` scaffolds configuration without storing secrets. Use `--json`, `--print-skill` and `--skill-target claude|codex|<path>` for agent setup; existing files are kept unless `--force`.

## Workflow for a change

1. `cockpit-code doctor` to find `CONFIG` and see what already loads (and what is `skipped`).
2. Read the existing file before editing it. Keep valid JSON (no comments, no trailing commas).
3. Make the smallest edit. New provider = new file `providers/<id>.json`.
4. If you don't know the API dialect, set your best guess for `type`, then run `cockpit-code doctor --probe <id>`. It makes one tiny request per dialect and prints `probe <Dialect>: accepted; suggested type="..."` for those that work. It never writes files: copy the suggestion into `type` yourself, and only after the user agrees if more than one dialect is accepted.
5. Run `cockpit-code doctor` again and confirm the provider/model appears and nothing is `skipped`.
6. Config reloads at the next turn boundary when a file changes; no restart is needed. A running agent keeps the tools and model it started with.

## Adding an HTTP provider (the common case)

`type` is required: `anthropic` (Messages API, `POST {baseUrl}/v1/messages`), `openai-chat` (`POST {baseUrl}/chat/completions`), or `openai-responses` (`POST {baseUrl}/responses`). `baseUrl` is the prefix before those paths.

Documented example (OpenRouter is only an example; any compatible gateway works the same way):

```json
{
  "type": "openai-chat",
  "label": "OpenRouter",
  "baseUrl": "https://openrouter.ai/api/v1",
  "headers": { "Authorization": "Bearer ${OPENROUTER_API_KEY}" },
  "reportedCost": { "from": "body", "pointer": "/usage/cost", "unit": "usd" },
  "models": {
    "deepseek/deepseek-v4.1-flash-1017": {
      "label": "DeepSeek V4.1 Flash",
      "description": "Fast, cheap generalist. Good for small edits and exploration.",
      "contextWindow": 100000,
      "maxOutputTokens": 8192,
      "reasoning": { "efforts": ["low", "medium", "high"], "default": "low", "normalized": { "max": "high" } },
      "capabilities": { "input": ["text"], "output": ["text"], "tools": true },
      "cost": { "inputPerMTok": 0.1, "outputPerMTok": 0.4 },
      "benchmarks": { "aa": 58, "costPerTask": 0.02 }
    }
  }
}
```

Rules worth checking every time:

- `description`: one line on strengths and when to use it. Agents read it when choosing subagent models.
- `capabilities.output` must be exactly `["text"]` for the model to be usable as a subagent. Any image/video/audio output makes it a media model, reachable only through the `generate` tool.
- Anthropic models that declare `reasoning.efforts` must also set `"thinking": "adaptive" | "manual" | "none"`.
- `priorThinking` other than `keep` is rejected for `anthropic`, `openai-responses`, DeepSeek and OpenRouter.
- Benchmarks: add unknown benchmark keys to `config.json` `benchmarks` with the right `higherIsBetter` so ranking sorts correctly (cost-like scores are lower-is-better).

## References

Read only the one you need (paths are relative to this skill's directory):

- `references/providers.md`: every provider and model field (reasoning, cost, `reportedCost`, `priorThinking`, `selfEvaluateWrites`, `maxConcurrentContexts`, `compactAt`, `generateParams`, ...), media models, subagent eligibility.
- `references/command-providers.md`: `type: "command"` providers that wrap a CLI (argv placeholders including per-agent `{conversationId}` and prompt-family `{cacheKey}`, `output`, `history`, `discover`).
- `references/config-json.md`: every `config.json` key, including `permissions`, `subagents`, `benchmarks`, `web` and frozen `/goal` policy.
- `references/mcps.md`: MCP server files.

`goal` defaults: USD/time/token limits disabled; noProgressTurns 3; repeatedGapRounds 3; reviewPanel true; maxReviewRounds 3; usageResume max 3 and backoffSecs 30 (doubling to 300). See references/config-json.md. Changing config never changes a goal already created.

The agent contract tool is `criteria` (Monty `criteria.*`); `/goal` and the `goal` configuration section control autonomous runs.

Tool tiers use `tools.profile: "full" | "core"` in config.json, provider entries or model entries (model → provider → global; default full). Core keeps routine tools native and exposes extensions through script discovery. Profiles and callable catalogs are fixed per session/role/model; resume and forks preserve them. `enabled:false` still disables both native and script access. See the tool configuration section in `references/config-json.md` for examples.

LSP defaults auto-detect installed Rust, JS/TS, Python, Go and C/C++ servers only for languages present when the session starts. `lsp.<name>` partially overrides/adds a server; `enabled:false` disables a default. `init`/`doctor` show detections and install hints without downloads. Server resolution and LSP availability freeze per session; see `references/config-json.md` for names and fields.

The standalone repository `search` tool is retired; use grep for text and code for syntax. `tools.search(query=...)` still discovers allowed capabilities.

Core includes native code (19 root / 21 tree child / 15 flat child tools before disables (plus terminal when the interactive frontend opts in)). Graph, context_pack and change_impact are script-only in every profile.

Monty discovery is `tools.search(query=..., limit=10)` / `tools.describe(name=...)`; `mcp.search/describe` are metadata aliases. File text uses grep(pattern=...), including any configured rename. Discovery never exposes disabled or role-forbidden schemas.

Interactive bash parameters are `tty` (default false) and `yieldMs` (0–30000 ms, PTY default 1000). `background:true` yields immediately. Running commands return jobId and paged output; native `jobs {action:"input", jobId, chars}` / Monty `jobs.input(...)` sends stdin or polls with empty chars. Per-tool overrides (`tools.<name>`) may rename bash/jobs and describe these parameters without changing their types. Existing frozen sessions keep their historical schemas; start a new session for these additions.

`bash.cwd` is an optional string resolved against the agent cwd; it changes execution directory without widening permission or scope policy. Tool descriptions identify cross-session/other-agent/external content as untrusted data. These defaults apply to new frozen tool surfaces; Tool description overrides continue to take precedence.

Monty `store(key,value)` / `load(key)` provide agent-local JSON across fresh VMs, capped at 1 MiB serialized total and 1,024 keys (keys 1–256 bytes). Missing load returns None; updates over the cap leave earlier values intact. State clears on agent end/restart and is not secret storage. Names store/load are reserved for new tool renames. There is no config knob for the cap.

The user-terminal tool is `terminal` (renamable/disableable via `tools.terminal`, e.g. `tools.terminal.enabled:false`). It is included in full/core only when the embedding frontend opts in before tool freezing and the session is not unattended; headless/ACP omit it, including Monty discovery. The TUI opts in (its terminal sidebar renders the request privately). No configuration can bypass this frontend requirement. Arguments prompt/command/cwd/share/wait have fixed types; share defaults status and wait defaults blocking. Private PTY bytes never use output files; only output finalized by user close can enter history. The engine uses the account login shell (bash/zsh), disables startup files/history/core dumps, and retains a bounded private 256 KiB tail (automatic/manual sharing ≤64 KiB). Existing terminal-capable frozen sessions require a capable frontend and cannot switch to unattended; start a new session instead of rewriting their schemas.
