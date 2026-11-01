---
when: changing an API, event, or user-facing flow
languages: [typescript, javascript, rust, python]
globs: []
---
- **IF1 Trace every consumer.** A new field or event affects serialization, replay and adapters as well as its producer. Check missing-field defaults and the names consumers actually receive.
- **IF2 Report what happened.** Distinguish queued, applied, rejected and stale work. Do not show success before the mutation commits or imply verification from a user's override.
- **IF3 Keep actions recoverable.** An interrupted dialog or disconnected client should leave a discoverable pending action. Preserve enough identity to resume the same operation safely.
- **IF4 Keep context local.** Deduplicate against the context the recipient currently has. Session history alone does not show what survived summarization or what a new consumer has seen.
