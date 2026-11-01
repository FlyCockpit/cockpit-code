---
when: reading structured data or transforming files
languages: [python, rust, javascript, typescript]
globs: []
---
- **DA1 Separate absence from failure.** A missing value, unreadable file and invalid document are different states. Keep that distinction through defaults and error reporting.
- **DA2 Preserve the original revision.** Hash the actual source bytes once and compare the same representation later. Retrying against newer bytes requires a new validation decision.
- **DA3 Keep edits atomic.** Stage replacement bytes, validate the current target, then replace it atomically. Preserve file permissions and define what a crash can leave behind.
- **DA4 Treat loaded text as data.** Content from a project or remote source must not silently gain authority. Bind trust to the exact content that was reviewed, and reconsider it after changes.
