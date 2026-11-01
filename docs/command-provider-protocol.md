# Command providers: cockpit-code.provider/1

A command provider starts one executable per model request, including tool-result
continuations. The executable can wrap an HTTP endpoint or a subscription CLI.
It never needs to implement cockpit-code's tools or approval UI.

```json
{
  "type": "command",
  "command": {
    "argv": ["python3", "/absolute/path/openai_compatible.py", "--model", "{model}", "--system", "@{systemPromptFile}", "--messages", "{messagesFile}", "--tools", "{toolsFile}", "{reasoningEffort?--effort={reasoningEffort}}"],
    "stdin": null,
    "cwd": "{cwd}",
    "env": {"OPENROUTER_API_KEY": "${OPENROUTER_API_KEY}"},
    "output": "jsonl",
    "history": "full",
    "timeoutSecs": 900,
    "discover": {
      "argv": ["python3", "/absolute/path/openai_compatible.py", "--discover"],
      "ttlSecs": 86400
    }
  },
  "models": {}
}
```

Use the same model argument in discovery and inference when overriding the
helper's default. Static provider models override discovery **field by field**.
Discovery runs when configuration is loaded or reloaded, using
`CACHE/discover/PROVIDER.json` until its TTL expires. Disabled providers are not
executed. `cockpit-code discover [PROVIDER] --refresh` and
`cockpit-code provider discover PROVIDER --refresh` force refresh. Discovery has a
30 second process-group deadline and a 4 MiB output limit; failures fall back to
static models and appear in catalog diagnostics. The current TUI adapter exposes
these diagnostics and descriptions; a dedicated refresh action/badge needs TUI
integration.

## Templates and files

`argv` is an array, never a shell command. Each substituted argument stays one
argument. Values are substituted once: quotes, `$()`, braces and newlines in
history are literal data. `${ENV_NAME}` expands when loading configuration;
`{name}` expands for each inference attempt. `stdin`, `cwd`, and `env` values also
accept request variables. Stdin is written concurrently with stdout/stderr reads
and closed afterward. With no stdin template, the executable sees EOF.

| Variable | Value |
| --- | --- |
| `{message}` | Newest user or tool-result text |
| `{conversationId}` | Stable per-agent conversation/replay identity `session:agent`; hidden forks retain the author's identity; utility lineages add a suffix |
| `{cacheKey}` | Prompt-cache routing key `session:family`; family is frozen role/model/profile plus a short hash of system prompt/tools. Compatible siblings share it; hidden forks retain their author's key. Utility lineages add a suffix. |
| `{deltaFile}` | Canonical messages added since this model/provider/agent's successful invocation |
| `{providerConversationId}` | Most recent successful invocation's `start` ID (latest history) |
| `{turnId}` | New UUID for every attempt, including retries |
| `{parentConversationId}` | Parent session/agent identity (`session:agent`), empty for root |
| `{depth}`, `{isSubagent}` | Actual lineage depth and `true`/`false` |
| `{agent}` | Role: build, plan, explore, or custom |
| `{model}`, `{provider}` | Config keys |
| `{systemPrompt}`, `{systemPromptFile}` | Frozen system prompt, inline/file |
| `{messages_json}`, `{messagesFile}` | Full canonical conversation JSON, inline/file |
| `{transcript}`, `{transcriptFile}` | Flattened user/assistant/tool transcript, inline/file |
| `{toolsFile}` | JSON array of `{name,description,schema}` tool definitions; `[]` in text mode |
| `{attachmentsDir}` | Private input directory containing every referenced session asset, using each attachment path as its filename |
| `{maxTokens}`, `{temperature}`, `{reasoningEffort}` | Request values, empty when unset; current engine has no temperature setting |
| `{cwd}`, `{permissionMode}` | Actual agent cwd and permission mode |
| `{outputFile}` | Optional final output file |

All files live in a private per-request temporary directory removed after the
process and its pipes finish, including failure, timeout and cancellation. An
additional `{messageFile}` contains the latest message text. Files are UTF-8.
Saved requests predating `{cacheKey}`/separate conversation identities keep their stored identities on replay. Keep provider conversation state keyed by `{conversationId}`, even when caching routes by `{cacheKey}`.
Canonical messages are conversation events from cockpit-code's schema:

```json
[
  {"kind":"user","text":"Hello"},
  {"kind":"assistant","text":"", "reasoning":[], "tool_calls":[{"id":"c1","name":"read","arguments":{"path":"README.md"},"complete":true}],"status":"tool_use","native_reason":"tool_use","replay":null},
  {"kind":"tool_result","id":"c1","name":"read","output":"...","is_error":false}
]
```

Journal metadata is excluded. Engine notices retain `kind:"system_reminder"` and their
original `<system-message id="…">` blocks. Translate them at their chronological position (for example, to inline developer messages); do not hoist later notices into the initial instructions. Reasoning events may include opaque `details[]`, retained in canonical reasoning blocks. They return unchanged in `{messagesFile}` and `{deltaFile}` across resume and fork. `priorThinking: drop|summarize` applies only to plaintext reasoning; opaque details are preserved.
The helper translates this schema to the upstream chat API. An attachment-bearing user event is `{"kind":"user","text":"Look at this","attachments":[{"path":"UUID.png","name":"photo.png","mimeType":"image/png"}]}`. Paths are single filenames relative to SESSION/assets/ and to `{attachmentsDir}`; input bytes never appear in the canonical journal. Files are copied into the private request directory before invocation. Full and latest-history requests include the files referenced by their history. Frontends import attachments with `Session::attach_file` / `attach_bytes` and use `send_with_attachments` / `queue_message_with_attachments`. Unsupported `capabilities.input` fails before spawning the command. The helper accepts `--attachments-dir {attachmentsDir}`.

UTF-8 attachments are always allowed, independently of `capabilities.input`, and
the reference helper inlines them as named text parts. Non-UTF-8 media still
requires its image/PDF/audio/video capability. Other binaries require `"file"`
in `capabilities.input`; the helper uses native Chat file parts for those bytes.
The upstream endpoint's accepted file formats still apply. Command providers
receive the original files and are responsible for translating them to their
own wire format.

The only conditional syntax is a whole argv argument:
`{reasoningEffort?--effort={reasoningEffort}}`. It is omitted if the value is empty.
If total argv exceeds 64 KiB, inline message/system/history/transcript placeholders
become `@/absolute/path` references. The executable must understand this convention;
the reference helper does. Remaining oversized argv fails with an instruction to
use stdin or file placeholders. Explicit file placeholders or stdin are preferable.

## JSONL inference

Set `output: jsonl`. Every stdout line is a JSON object with `type`:

| Type | Fields |
| --- | --- |
| `start` | `model` (requested key), optional `providerConversationId` |
| `text` | `delta` |
| `reasoning` | Optional `delta` string, optional opaque `details` array, optional `summary` metadata |
| `tool_call` | Unique nonempty `id`, offered `name`, object `arguments` |
| `usage` | `inputTokens`, `outputTokens`, optional `cacheReadTokens`, `cacheWriteTokens`, `reasoningTokens`, `contextUsed`, `contextWindow`, `costUsd` |
| `quota` | Optional `usedPercent`, `remaining`, `resetsAt`, `label` |
| `error` | `message`, `retryable`, optional `retryAfterSecs`, `kind` |
| `done` | `stopReason`: `end_turn`, `tool_use`, `max_tokens`, `cancelled` |

Example:

```jsonl
{"type":"start","model":"example-model","providerConversationId":"remote-123"}
{"type":"reasoning","delta":"I will inspect the file."}
{"type":"tool_call","id":"c1","name":"read","arguments":{"path":"README.md"}}
{"type":"usage","inputTokens":200,"outputTokens":20}
{"type":"done","stopReason":"tool_use"}
```

Emit complete tool calls, never partial arguments. cockpit-code validates offered names
and schemas, runs its permission gate, executes tools, and invokes the command
again with results. A `max_tokens` or `cancelled` response executes no calls.
A `done` event is required and must be terminal. `tool_use` must agree with the
presence of calls. Invalid JSON lines become visible text and generate one protocol
warning per attempt; malformed structured events fail. Each line is limited to
4 MiB. Stdout and stderr drain concurrently. Failure diagnostics retain the last
64 KiB of stderr, with explicit command-env values redacted.

`usage` is a complete snapshot; the last usage event wins. Input tokens include
cache reads and writes. Cost precedence is the configured `reportedCost` body
JSON-pointer (model override first), then `costUsd`, then token rates. For example,
`{"from":"body","pointer":"/upstream/cost","unit":"usd"}` reads a nested
value on a usage event. `usd_micros` is also supported. HTTP header locators are
rejected for command models; expose the value in JSON instead. Subscription billing
without rates costs $0; quota is retained in the durable usage record. Unknown
calls retain partial-cost accounting (`$X+` when mixed with known cost).

## Plain text inference

Set `output: text` (the default). UTF-8 stdout streams as assistant text;
`toolsFile` contains an empty array. If stdout is empty, the command may write UTF-8 final text
to `outputFile`. To report usage, emit one stderr line:

```json
{"protocol":"cockpit-code.provider/1","type":"usage","inputTokens":100,"outputTokens":20,"costUsd":0.001}
```

The last such trailer wins. Other stderr is diagnostic only. Without a trailer,
token usage and cost remain unknown (except explicit subscription billing).
The plan references a trailer but does not give its framing; this versioned JSON
line is the implementation's concrete framing.

## History, scheduling and cancellation

`history: full` (default) replays the complete canonical conversation every call.
Hidden forks and self-evaluation preserve frozen request prefix bytes and append
only the private tail. CLI caching is the wrapper's responsibility.

`history: latest` is for wrappers maintaining their own conversation. Read
`deltaFile` instead of `messagesFile`, resume `providerConversationId`, and echo
that ID in `start`. The delta skips the wrapper's own previous assistant reply;
it contains new user messages, tool results and engine notices. The engine saves
a cursor and remote ID in `DATA/command-conversations/` only after successful
completion, scoped by session, agent, provider and model, so resume works. Full-history author
calls also retain the remote ID; private forks use independent state. A
changed history prefix (restore/compaction) resets the cursor and remote ID.
The wrapper must save its own successful assistant reply before exiting. Use
`turnId` for upstream deduplication if available; a retry has a new ID.
Hidden forks, titles and `selfEvaluateWrites` are disabled with latest history,
and the model description carries a warning. Context uses both reported
`contextUsed` and `contextWindow`; otherwise its source is `unknown`, with no
local context-limit guess. The current TUI drops the source flag and retains its
previous window; showing an explicit unknown meter needs TUI integration.

Command providers default to **one provider slot**, shared across their models.
Explicit `maxConcurrentContexts` overrides it; model ceilings also apply. Parent
reservations yield to children and short calls using the existing scheduler.
Exclusive mode constrains command models exactly as it constrains HTTP models.
Quota events also update the provider usage cache; frontends read `provider_usage()` and use `refresh_usage()` for a forced background refresh. See [usage and soft budgets](usage-protocol.md).

Cancellation and timeout send SIGINT to the entire process group, allow up to
five seconds, then SIGKILL remaining members and reap the process. Dropping an
in-flight future kills the group. Cancelled visible output is preserved in a
cancelled assistant event without executable tool calls. `timeoutSecs` bounds the
whole process/pipe lifecycle. Exit 0 requires valid completion, 75 is retryable,
other codes fail unless a structured error supplied retry behavior. Supported
error kinds are `auth`, `rate_limit`, `context_overflow`, `bad_request`, `unavailable`.
Retries use the engine's existing backoff and emit separate usage attempts.

## Discovery and live diagnosis

Successful discovery is cached for `discover.ttlSecs` (default 86400). Failures
are cached separately for 300 seconds, including across process restarts. Static
models remain available. A provider-file edit or explicit refresh bypasses both
caches. `ConfigStore::refresh_discovery_many(providers)` reloads once and refreshes
each selected provider once. `Snapshot.discovery_errors` maps provider IDs to
`DiscoveryError {kind,message,cached,retry_at}` (camelCase in JSON); the legacy
`errors` diagnostics remain available. `cockpit-code discover` includes the same
structured error in each provider row's `error` field (null on success).

Discovery prints one object with `protocol`, a `models` object and optional
`account` metadata. Model entries use normal model config fields, including
`reasoning: {efforts, default, normalized}`. Efforts are vendor names;
`normalized` maps `low|medium|high|max` to those names. Explicit subagent common
names are mapped; configured defaults remain vendor names.

`cockpit-code provider test PROVIDER` refreshes discovery, makes **one minimal
inference call**, and reports observed streaming, reasoning, tool counts, terminal
status, usage and protocol warnings. It does not execute a tool or simulate a
cancellation. Verify those lifecycle cases with a real session.

The dependency-free [Python helper](../examples/command-provider/openai_compatible.py)
wraps an OpenAI-compatible streaming endpoint. It supports full or persisted
latest history, text or JSONL output, usage, tools and effort mapping. Set
`OPENAI_BASE_URL` and `OPENAI_API_KEY`, or use its OpenRouter defaults with
`OPENROUTER_API_KEY`. The optional `--pace` flag delays chunks for cancellation
and timeout demonstrations. `COMMAND_PROVIDER_AUDIT` logs only invocation metadata.
Its upstream framing follows the official [streaming](https://openrouter.ai/docs/api/reference/streaming)
and [tool-calling](https://openrouter.ai/docs/guides/features/tool-calling) documentation.

## Strict tools and generated files

`strictTools` is optional at provider level and overridable per model. Defaults:
`openai-responses` true, all other dialects (including command) false. Each
`{toolsFile}` row has `name`, `description`, `schema`, and a boolean `strict` hint.
Strict rows use closed, required nullable parameters; lenient rows retain optional
properties. Wrappers must pass the hint to their upstream API if supported.
Effective values are frozen per model in session settings, including resume and
fork. Tool definitions remain frozen for the lineage; hot reload does not change
strictness for an existing model.

For `generate`, a command media model receives the prompt in `{message}` and
`{messagesFile}`, reference inputs in `{attachmentsDir}`, no tools, and a private
`{outputFile}` destination. It must write a nonempty regular binary file and finish
successfully (JSONL still requires terminal `done`; plain text may exit zero with
no stdout). Binary output is never decoded as assistant text. The engine writes
the destination atomically without overwriting an existing file, under the usual
write approval rules. Usage events/trailers use the ordinary protocol. Command
output paths default to PNG, WAV, and MP4 by kind; recognized file signatures set
the returned MIME type and final asset extension (unknown data uses octet-stream
and .bin). Reference inputs are capability-gated.
