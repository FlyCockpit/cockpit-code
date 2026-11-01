# config.json

Every key is optional. Unknown keys are ignored. A syntax error in `config.json` stops cockpit-code from starting (provider and MCP files fail individually instead), so validate with `cockpit-code doctor` after editing.

```json
{
  "defaultModel": "anthropic/claude-opus-5-5",
  "tools": { "profile": "full" },
  "permissions": {
    "mode": "manual",
    "autoModel": "anthropic/claude-haiku-5-5",
    "autoVariations": 3,
    "prompts": { "toolCall": "safety-tool-call.md", "writeEdit": "safety-write-edit.md" }
  },
  "subagents": { "maxDepth": null, "maxConcurrentChildren": 4, "maxTotalAgents": null },
  "script": { "cpuSecs": 10, "maxCalls": 1000 },
  "benchmarks": {
    "aa": { "label": "Artificial Analysis index", "higherIsBetter": true },
    "costPerTask": { "label": "Cost per task", "unit": "USD", "higherIsBetter": false }
  },
  "web": { "provider": "firecrawl", "apiKey": "${FIRECRAWL_API_KEY}" },
  "systemPrompt": "system.md",
  "theme": "auto"
}
```

| Key | Default | Notes |
|---|---|---|
| `defaultModel` | none | `provider/model` for new sessions. |
| `permissions.mode` | `manual` | `manual` \| `auto` \| `unattended` \| `yolo`. |
| `permissions.autoModel` | none | Safety sidecar for `auto`/`unattended`; must take and produce text. |
| `permissions.autoVariations` | `3` | Candidates generated when the sidecar answers "improve". |
| `permissions.prompts.toolCall` / `.writeEdit` | built-in | Markdown files replacing the safety prompts; relative to `CONFIG`. |
| `subagents.maxDepth` | `null` (unlimited) | |
| `subagents.maxConcurrentChildren` | `4` | Per parent. |
| `subagents.maxTotalAgents` | `null` | Live agents across the tree. |
| `script.cpuSecs`, `script.maxCalls` | `10`, `1000` | Limits for the `script` tool. |
| `benchmarks` | `{}` | Key → `{label, unit?, higherIsBetter}` (`higherIsBetter` defaults to true). Register every benchmark key used in provider files so `models rank` sorts the right way. |
| `web.provider` | none | `firecrawl` \| `tinyfish`. Without it `web_search` errors and `web_fetch` does a plain HTTP GET. |
| `web.apiKey` | none | Must be exactly one `${VAR}` reference such as `"${FIRECRAWL_API_KEY}"`; a literal key is a config error. Expanded when a web tool runs. |
| `web.baseUrl` | provider default | Self-hosted Firecrawl, or a proxy. For TinyFish it replaces both hosts (`{baseUrl}/search`, `{baseUrl}/fetch`). |
| `systemPrompt` | `CONFIG/system.md` if present | Markdown file replacing the built-in system prompt. |
| `skillsDirs` | `["~/.agents/skills", "./.agents/skills"]` | |
| `theme` | `auto` | |
| `mouse` | `true` | TUI mouse capture. |
| `vim` | `false` | TUI composer vim mode. |
| `pasteCollapseLines` | `8` | |
| `modelSwitchWarnMinutes` | `60` | 0 turns the warm-cache warning off. |
| `queueDefault` | `next-turn` | `next-turn` \| `steer` \| `interrupt`. |
| `steerBackgroundSecs`, `steerMinWaitSecs` | `60`, `15` | When a steer backgrounds a running foreground call. |

## Autonomous goal policy

`goal` in config.json defaults to:

```json
{"limits":{"maxUsd":null,"maxMinutes":null,"maxTokens":null},"noProgressTurns":3,"repeatedGapRounds":3,"reviewPanel":true,"maxReviewRounds":3,"usageResume":{"max":3,"backoffSecs":30}}
```

Limits pause the run; maxMinutes counts active work, excluding owner approvals, blocking questions and usage backoff. `null` disables a limit. Positive counters/time values are required. Usage retries double the backoff up to 300 seconds. The policy and execution/review model configuration are frozen when `/goal` starts; reloads affect future goals. Restart always pauses the goal. `/goal pause|resume|edit|clear` controls its lifecycle. `cockpit-code -p "objective" --goal` drafts and approves automatically; `--json` ends with the contract and exitCode (0 complete, 3 roadblocked, 4 paused, 75 persistent usage limit, 1 error). The review panel defaults on for root goals and is opt-in for children via task review=true.

`permissions.reviewerGuidance` accepts inline text or a CONFIG-relative Markdown
file. It is appended to both reviewer prompts. Project guidance at
`.cockpit-code/reviewer-guidance.md` follows it only after user trust; hashes live
in DATA and changes need renewed trust. Headless ignores untrusted project text.
Agent edits always require approval, including yolo.

Tool definitions live in `CONFIG/tools.json`, keyed by canonical tool:
`{"read":{"name":"inspect_file","description":"Read a file.","parameters":{"path":{"description":"File path"}}},"bash":{"enabled":false}}`.
Provider and model `tools` maps override individual global fields (model wins).
The separate native tier setting is `"tools":{"profile":"full"}` or `"core"`
in config.json. Provider/model `tools` objects may mix `profile` with canonical
tool overrides, for example `"tools":{"profile":"core","grep":{"name":"find_text"},"lsp":{"enabled":false}}`.
Profile precedence is model, provider, global; the default remains `full`.
Core exposes 19 root tools (read, write, edit, bash, grep, glob, code, script, history,
question, answer, escalate, message, task, agents, jobs, scope, criteria, skill).
Tree children add report/compact; flat children drop task/agents/message/answer/
escalate and retain compact (15 tools). Disabled tools reduce those counts.
Code intelligence, models, ledger, web and eligible media remain
callable through Monty: `print(tools.search(query='lsp'))`, then
`print(tools.describe(name='lsp'))` and call the described keyword function.
Core changes advertisement only. `enabled:false` removes both surfaces;
flat-forbidden tools never return through discovery. Native definitions and the
callable catalog freeze per session/role/model before its first request; resume,
compaction, continuations and hidden forks preserve them. Start a new session
to apply changed tiers/overrides. Keep full unless per-model comparison supports
core; it is opt-in.
Only name/description/parameter descriptions/enabled may change; shape is fixed.
Names must be unique Python identifiers and avoid reserved keywords/namespaces.
Changes apply to new agent profiles; Monty keeps canonical aliases. `init` writes
managed defaults and upgrades untouched entries, preserving edits and deletions.

LSP auto-detects installed binaries for project languages at session start (gitignore-aware scan, up to 100,000 entries). Shipped names are `rust` (rust-analyzer), `typescript` (typescript-language-server --stdio), `python` (basedpyright-langserver --stdio, then pyright-langserver), `go` (gopls), `cpp` (clangd). No servers are downloaded. `init`/`doctor` show detections and missing-binary install hints for observed languages.

`lsp.<name>` partially overrides defaults: `command`, `args`, `fileGlobs`, `rootMarkers`; omitted fields inherit, explicit empty arrays replace. `enabled:false` disables a default; arbitrary names add servers. For example `{"lsp":{"cpp":{"enabled":false},"python":{"command":"pyright-langserver"}}}`. Both binary availability and matching project files are required. The resolved map, including an empty map, is persisted in the session; config/PATH/new language changes need a new session. No matching server means no native or script LSP tool. Existing frozen legacy tool surfaces remain intact.

LSP also accepts `lsp.afterWrite`:
`"errors" | "errors+warnings" | "off"` (default errors+warnings) and
`lsp.afterWriteMs` (positive milliseconds, default 1500). Successful write/edit
results show only introduced diagnostics (errors first), with pre-existing counts,
capped to 20 lines. Timeouts/no matching server never fail a write. A model may
set `"lsp":{"afterWrite":"off"}` (or errors/errors+warnings) to override global
behavior; afterWriteMs can also be overridden per model.

The standalone repository `search` tool is retired for new sessions; use grep for text and code for syntax. Its internal index remains. Untouched managed search defaults are removed on upgrade; edited legacy entries are inert for new sessions.

`code` is native in core. `graph`, `context_pack` and `change_impact` are Monty-only in both full and core. Their bounded heuristic results cannot prove absence or correctness.

Code outline uses name/symbol_kind filters and 1-based offset/next_offset pages (default 40, max 100 items, 5000 item bytes). Large limit values do not bypass the byte bound. Impact limit defaults 20/max 200, bounds nested lists and total response bytes; inspect counts/truncated. History query/get keeps literal agent records and adds sourceAgent metadata. These are tool behaviors, not configuration switches.

Discovery accepts `tools.search(query='semantic references', limit=3)` (ranked results, default 10/max 100) and `tools.describe(name='lsp')`. `mcp.search`/`mcp.describe` alias the same allowed catalog. They do not execute a file search: use the configured grep name with pattern/path/glob. Renames remain callable through their canonical aliases; disabled/flat-forbidden schemas stay unavailable.

### Interactive engine tools

`bash` adds tty (boolean), yieldMs (0–30000 integer, tty default 1000) and cwd (string relative to agent cwd). jobs adds action input with jobId/chars; input is permission-gated as a mutation, including empty polls. These are tool arguments, not global config keys. `terminal` is a tool configurable via `tools.terminal` (rename, description, enabled), eligible only with an explicitly capable interactive frontend, never in unattended/headless/ACP. `{"tools":{"terminal":{"enabled":false}}}` disables it in new sessions. There is no config override for frontend capability, private sharing limits, or the script store cap. Monty store/load are reserved names for new tool renames; state is per agent, memory-only, ≤1 MiB/1024 keys and cleared at agent end/restart. Frozen native/catalog schemas and descriptions stay unchanged in existing sessions.
