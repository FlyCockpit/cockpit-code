# Changelog

All notable changes to Cockpit Code are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 1.0.0 - Unreleased

### Added

- First public release of Cockpit Code, licensed under MIT.
- One in-process agent engine with three front ends: a full-screen terminal UI, headless runs (`cockpit-code -p`, with `--json` and `--goal`) and an Agent Client Protocol (v1) stdio server for editors (`cockpit-code acp`).
- Native `anthropic`, `openai-chat` and `openai-responses` providers, plus command providers that wrap any executable over a JSONL protocol (`cockpit-code.provider/1`), with model discovery, cost tracking and usage readers (`cockpit-code.usage/1`).
- Subagents in tree or flat layouts, and autonomous `/goal` runs with spending, time and token limits and an optional review panel.
- Permission modes `manual`, `auto`, `unattended` and `yolo`, with a configurable safety model for the automatic modes.
- MCP servers over stdio, HTTP and SSE; LSP navigation and diagnostics through locally installed language servers; persistent sessions with resume, fork, compact and prune; PTY background jobs and user terminals.
- A sandboxed Python `script` tool, web search and fetch, skills from `~/.agents/skills`, and file attachments.
- Release binaries for Linux (static musl builds for x86_64 and aarch64) and macOS (Apple silicon, Intel) on GitHub Releases, with SHA256SUMS, third-party license notices and signed GitHub build provenance attestations for every asset (`gh attestation verify <file> --repo FlyCockpit/cockpit-code`).
