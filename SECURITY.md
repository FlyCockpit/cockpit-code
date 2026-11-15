# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** through GitHub private vulnerability reporting:

<https://github.com/FlyCockpit/cockpit-code/security/advisories/new>

Do not open public issues, discussions or pull requests for security problems. We aim to acknowledge reports within 72 hours.

In scope: the `cockpit-code` binary and the crates in this repository (the engine, the terminal UI, the ACP server and configuration loading), and the release binaries built from it. Leaks of secrets or session data, permission-gate bypasses in `manual` mode, and problems in how provider credentials are resolved from environment variables are especially important.

## Supported versions

Only the latest release receives fixes.

## Design stance

Cockpit Code is intentionally minimal and does not sandbox the agent: its permission modes decide when to ask you, but tools run with your user's privileges, so run it inside a container, VM or other sandbox of your own when you need isolation.

## Security controls

- Secrets are never written to config: provider files reference environment variables (`${NAME}`), and `cockpit-code init` only writes those references.
- Saved sessions are stored in owner-only (0700) directories, and the job and tool output files inside them are created owner-only (0600), because they hold verbatim tool output.
- SHA-pinned GitHub Actions, and `cargo deny` checks of licenses, sources, bans and RustSec advisories in CI.
