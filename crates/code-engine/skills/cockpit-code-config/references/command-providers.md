# Command providers (`type: "command"`)

A command provider runs a CLI once per model turn (no shell; argv only). Use it to wrap a subscription CLI or a vendor without an HTTP dialect cockpit-code speaks. A long-lived local server should instead be an HTTP provider pointed at `http://127.0.0.1:...`.

```json
{
  "type": "command",
  "label": "My CLI",
  "command": {
    "argv": ["mycli", "--model", "{model}", "--system", "@{systemPromptFile}", "--messages", "{messagesFile}",
             "--tools", "{toolsFile}", "{reasoningEffort?--effort={reasoningEffort}}"],
    "stdin": null,
    "cwd": "{cwd}",
    "env": { "MYCLI_TOKEN": "${MYCLI_TOKEN}" },
    "output": "jsonl",
    "history": "full",
    "timeoutSecs": 900,
    "discover": { "argv": ["mycli", "cockpit-discover"], "ttlSecs": 86400 }
  },
  "models": {
    "large": { "description": "...", "capabilities": { "input": ["text"], "output": ["text"], "tools": true } }
  }
}
```

## `command` fields

| Field | Values | Notes |
|---|---|---|
| `argv` | string[] | Placeholders are substituted inside one argument each, so model text can't break out. `${VAR}` expands too. |
| `stdin` | `null` \| `"{messages_json}"` \| `"{transcript}"` | What to write to stdin. |
| `cwd` | string | Usually `"{cwd}"`. |
| `env` | object | Extra environment; `${VAR}` and placeholders allowed. Credentials go here as `${VAR}`. |
| `output` | `text` \| `jsonl` | `text`: stdout is the answer and the CLI runs its own tools. `jsonl`: the `cockpit-code.provider/1` event stream, which lets the model call cockpit-code's tools. |
| `history` | `full` \| `latest` | `latest` for CLIs that keep their own conversation (use `{deltaFile}`, `{conversationId}`). |
| `timeoutSecs` | int | |
| `discover` | `{argv, ttlSecs}` | Optional. Prints one JSON document describing the models; values in the provider file override discovered ones field by field. Success TTL defaults to 86400s; failures are cached for 300s. Refresh or a provider-file edit bypasses either cache. |

## Placeholders

`{message}`, `{conversationId}`, `{cacheKey}`, `{deltaFile}`, `{providerConversationId}`, `{turnId}`, `{parentConversationId}`, `{depth}`, `{isSubagent}`, `{agent}`, `{model}`, `{provider}`, `{systemPrompt}` / `{systemPromptFile}`, `{messagesFile}` / `{messages_json}`, `{transcriptFile}` / `{transcript}`, `{toolsFile}`, `{attachmentsDir}`, `{maxTokens}`, `{temperature}`, `{reasoningEffort}`, `{cwd}`, `{permissionMode}`, `{outputFile}`.

- `{conversationId}` is the per-agent conversation/replay identity (`session:agent`); `{parentConversationId}` is the parent's identity. `{cacheKey}` routes the prompt cache by frozen role/model/profile and system/tools hash (`session:family`). Compatible siblings share cache routing while their conversations remain separate. Hidden forks keep the author's identities; compaction/title have separate suffixes. Legacy saved requests retain their recorded identity on replay.
- codex-command providers should include `"--conversation-id", "{conversationId}", "--cache-key", "{cacheKey}"` in argv; this requires a codex-command build supporting `--cache-key`. Keep Copilot `X-Interaction-Id` unchanged.
- `{x?text}` drops the whole argument when `x` is empty, e.g. `{reasoningEffort?--effort={reasoningEffort}}`. It is the only templating feature.
- `@{systemPromptFile}`-style prefixes are literal text kept in the argument.
- `{outputFile}`: the CLI may write its final answer there instead of stdout. Media models use it: `generate` asks the CLI to write the image/audio/video to `{outputFile}`.

## jsonl events (one JSON object per stdout line)

`start {model, providerConversationId?}`, `text {delta}`, `reasoning {delta?, details?: array, summary?}`, `tool_call {id, name, arguments}`, `usage {inputTokens, outputTokens, cacheReadTokens?, cacheWriteTokens?, reasoningTokens?, contextUsed?, contextWindow?, costUsd?}`, `quota {usedPercent?, remaining?, resetsAt?, label?}`, `error {message, retryable, retryAfterSecs?, kind?}`, and a required final `done {stopReason}` (`end_turn|tool_use|max_tokens|cancelled`).

Exit codes: 0 success, 75 retryable, anything else fatal. Cancellation sends SIGINT to the process group, then SIGKILL after 5 s.

## Checking a command provider

`cockpit-code doctor` shows whether it parsed. `cockpit-code provider test <id>` (when available in your build) runs discovery and a short scripted conversation and prints protocol violations.

## Strictness, attachments and account usage

Provider `strictTools` defaults to false for command providers; models can override
it. Each toolsFile row includes `strict`. The session fixes its effective value.
Attachment-bearing canonical user messages include `attachments: [{path,name,mimeType}]`.
Each path names a private file in `{attachmentsDir}`, copied from session assets.
Opaque reasoning `details[]` is retained and returned with assistant reasoning on
resume and fork; wrappers must replay it unchanged.

Any provider type can set `usage: {argv: ["mycli", "usage"], ttlSecs: 300}` to read
an account document in the background. Stdout is one JSON object with protocol
`cockpit-code.usage/1`, optional account/credits/notes metadata, and limits such as
`[{id:"weekly",label:"Weekly",usedPercent:25,resetsAt:"...RFC3339..."}]`.
Limits may instead provide used/limit/unit. Never emit credentials in this document.
`budget: {usd:20,period:"month"}` is a soft UTC budget with 80%/100% warnings;
it does not reject requests. `models list/rank` include cached headroom.
