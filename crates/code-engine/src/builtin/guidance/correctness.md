---
when: changing behavior or handling failures
languages: []
globs: []
---
- **CR1 Follow the whole transition.** When a value changes, update its dependent status, evidence and cached representation together. Check the callers that decide whether work is complete.
- **CR2 Keep the original failure.** Cleanup should preserve the cause of failure. Report a cleanup failure separately, and make retries safe after partial progress.
- **CR3 Recheck before committing.** A decision made before an await may be stale afterward. Validate identity, ownership and source revision again at the point of mutation.
- **CR4 Bound retained work.** Put explicit limits on queues, scans and accumulated diagnostics. If data is coalesced or omitted, tell the consumer what remains unknown.
