# Looking up model metadata

When provider/model entries need a context window, maximum output, pricing,
capabilities or modalities, look up the model at [models.dev](https://models.dev/):

- `curl https://models.dev/api.json`: providers and their models.
- `https://models.dev/models.json`: provider-agnostic model metadata.
- `https://models.dev/catalog.json`: both providers and canonical metadata.
- Add `?type=all` to include specialized model types.

Select the exact provider/model variant. Map `limit.context` to `contextWindow`
and `limit.output` to `maxOutputTokens`. Pricing is already USD per million
tokens: map input/output/cache-read/cache-write to `cost.inputPerMTok`,
`outputPerMTok`, `cacheReadPerMTok`, `cacheWritePerMTok` without dividing by a
million. Map modalities.input/output to capabilities.input/output and tool-call
support to capabilities.tools. Only advertise modalities the configured wire
can actually carry; use cockpit-code's `file` capability for supported file input.
Reasoning support alone does not specify vendor effort names or thinking mode;
verify those with the provider. Preserve unknown values rather than inventing them.

Values returned by `cockpit-code discover` or the provider's own API take
precedence over this catalog. Confirm the API dialect and run doctor afterward.
