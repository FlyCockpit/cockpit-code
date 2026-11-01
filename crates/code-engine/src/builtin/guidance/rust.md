---
when: editing shared state, tasks, or resource lifetimes
languages: [rust]
globs: ['**/*.rs', '*.rs']
---
- **RS1 Keep lock order visible.** Gather the needed state under a short lock, release it, then await or call code that may lock again. Follow one order when multiple locks are unavoidable.
- **RS2 Own spawned work.** Every spawned task needs a cancellation path and an owner that observes its completion. Dropping a handle must not leave writes running after cancellation is acknowledged.
- **RS3 Make notification waits race safe.** Register the notification before inspecting the condition. Recheck after waking, and distinguish waiting for work from holding a scarce execution slot.
- **RS4 Keep durable transitions together.** Persist one authoritative state change before notifying consumers. Replay must produce the same state without relying on live task handles.
