# Provider usage: `cockpit-code.usage/1`

Any provider type may declare a background account reader and a soft USD budget:

```json
{
  "usage": {"argv": ["/path/to/helper", "usage"], "ttlSecs": 300},
  "budget": {"usd": 20, "period": "month"}
}
```

`argv` runs directly, without a shell or inference placeholders. `${VAR}` references
are expanded at config load. The process inherits the frontend environment; command
providers additionally supply their configured `command.env`. It has no stdin, a
30 second timeout, and a 1 MiB stdout limit. Stderr is discarded. Errors expose only
safe spawn/timeout/JSON/protocol diagnostics or the exit code. The whole process
group is killed when the reader ends or is cancelled.

Exit zero and print exactly one JSON document:

```json
{
  "protocol": "cockpit-code.usage/1",
  "account": {"label": "Personal subscription"},
  "limits": [
    {"id": "five-hour", "label": "5 hour window", "usedPercent": 42,
     "resetsAt": "2026-10-29T02:00:00Z"},
    {"id": "weekly", "label": "Weekly", "used": 850, "limit": 1000,
     "unit": "requests", "resetsAt": "2026-11-02T00:00:00Z"}
  ],
  "credits": {"unit": "USD", "remaining": 12.50, "spentUsd": 7.50,
              "periodStart": "2026-10-01T00:00:00Z"},
  "notes": ["Some activity may take a few minutes to appear."]
}
```

`account`, `credits`, and `notes` are optional JSON metadata; omit credentials,
key hashes and sensitive upstream responses. `limits` defaults to an empty array.
Each limit needs a unique nonempty `id` and a nonempty `label`; `usedPercent`,
`used`, `limit`, `unit`, `resetsAt` are optional. Numbers must be finite and
nonnegative; a supplied `limit` must be positive. Percent is `usedPercent`, or
`100 * used / limit`. Values above 100 are allowed. A reset timestamp is RFC3339.
A limit without enough numeric information remains visible, with unknown headroom.

The CLI/TUI entry point and ACP startup start the shared cache in the background;
engine sessions also start it when embedded. Successful data is saved privately in
CACHE/usage/. Reloads and process restarts reuse matching reader/environment
fingerprints. Default TTL is 300 seconds; 0 refreshes each poll (5 seconds). Failed
refreshes retain the last successful document and expose an error and stale state.
The same TTL backs off failed attempts. `Session::refresh_usage()` forces a refresh
for the TUI `/usage` dialog's `r` key. Concurrent refreshes are coalesced per provider.
The frontend reads `Session::provider_usage()`; it does not need to run a reader.

Live `quota` inference events update this same in-memory view when their attempt's
usage is committed; startup also recovers them from durable usage records. They can carry the versioned document, `limits[]`, or the
single-limit fields (`id`, `label`, `usedPercent`, `used`, `limit`, `unit`,
`resetsAt`). Limits merge by ID, preserving other windows and account metadata.
The greatest used percentage across windows and any budget is the tightest limit
for a badge and for `models` headroom. An unconfigured reader has no quota limit;
a configured reader with no usable result has unknown quota headroom.

## Soft budgets and model selection

Budget periods are UTC calendar days, Monday-based weeks, and calendar months.
Positive finite `usd` is required. Startup recovers durable usage from all local
sessions in the background and deduplicates original call IDs across contexts and
forks; new committed calls update the view immediately. Known costs are summed;
unknown-cost call counts and loading/stale state remain available to frontends.
`credits.spentUsd` can supplement local costs when `credits.periodStart` matches
the configured budget period exactly. The larger total wins, avoiding double counting.

A budget appears as a separate USD limit in `ProviderUsage.budget`. Frontends and
model boundary reminders warn at 80% and at 100%. Exceeding it never blocks a call.
Deleting local sessions removes their local cost evidence on the next process start;
this is advisory accounting, not an authoritative billing ledger.

`models list`, `rank`, and `describe` expose cached `headroom`:

```json
{"kind":"quota","remainingPercent":15,"resetsAt":"2026-11-02T00:00:00Z","spentUsd":0.02}
```

`kind` is `quota`, `budget`, or `unlimited`. Percent and reset are absent/unknown
when not reported; budget remainingPercent and local spentUsd stay unknown while
historical cost recovery is running. The tightest known quota/budget selects the kind. Tool reads
perform no command execution, network request, or disk scan. Prompt guidance
prefers healthy subscription quota (about $0 extra per request), avoids models
below about 10% when practical, and weighs cost per task for unlimited metered
models. These are model-selection nudges, not restrictions.

The [reference helper](../examples/command-provider/openai_compatible.py) supports
`usage [--period day|week|month] [--url URL]`. It reads OpenRouter's
[account endpoint](https://openrouter.ai/docs/api-reference/limits), uses only the
spend fields, and never forwards key labels/hashes. `--url` or `OPENAI_BASE_URL`
can select a compatible account endpoint. Authentication remains in
`OPENAI_API_KEY` or `OPENROUTER_API_KEY`.
