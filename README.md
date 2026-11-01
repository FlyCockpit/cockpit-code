# cockpit-code

**A terminal coding agent written in Rust: interactive TUI, headless runs, and an ACP server for your editor. Bring any model provider.**

[![License: MIT](https://shieldcn.dev/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.99+](https://shieldcn.dev/badge/Rust-1.99%2B-orange.svg?logo=rust)](https://www.rust-lang.org/tools/install)
[![GitHub release](https://shieldcn.dev/github/FlyCockpit/cockpit-code/release.svg)](https://github.com/FlyCockpit/cockpit-code/releases)
[![GitHub stars](https://shieldcn.dev/github/FlyCockpit/cockpit-code/stars.svg)](https://github.com/FlyCockpit/cockpit-code/stargazers)

cockpit-code is a coding agent you can use in three ways. You can work with it interactively in a full-screen terminal UI, run it as a one-shot command in scripts and CI, or connect it to an editor over the [Agent Client Protocol](https://agentclientprotocol.com). All three share one engine. It talks to Anthropic, OpenAI-compatible (Chat Completions or Responses) and any other model you can wrap in a small command-line adapter. Configuration is plain JSON files, and secrets stay in environment variables.

## Features

- **Terminal UI** (the default when you run `cockpit-code`). It has slash commands for models, reasoning effort, permissions, diffs, cost, usage, jobs, MCP servers and more. Type `/help` to list them.
- **Headless mode**: `cockpit-code -p "task"` runs one task and exits. Add `--json` for machine-readable output, or `--goal` to run the prompt as an autonomous goal (`cockpit-code -p "objective" --goal`).
- **ACP server**: `cockpit-code acp` serves the Agent Client Protocol (v1) on stdio for editors such as Zed.
- **Any provider**: native `anthropic`, `openai-chat` and `openai-responses` dialects, plus **command providers** that wrap any executable (a subscription CLI, a gateway or a script) behind a small JSONL protocol. Model discovery, cost tracking and per-provider usage and budget readers are built in.
- **Subagents**: agents can delegate to child agents in a tree or a flat one-level layout. Each child can use a different model. `/agents` shows the live agent tree.
- **Goals**: `/goal <text>` works autonomously against an agreed contract, with spending, time and token limits and an optional review panel.
- **Permissions**: four modes. `manual` asks you before each gated action. `auto` and `unattended` let a configurable safety model review the action. `yolo` skips the gate. Approvals can be granted once or for the whole session.
- **MCP**: connect Model Context Protocol servers over `stdio`, `http` or `sse`.
- **LSP**: navigation and post-edit diagnostics use the language servers you already have installed for Rust, JS/TS, Python, Go and C/C++. cockpit-code never downloads servers, and you can override or add servers in config.
- **Sessions**: every session is saved to disk. Resume one with `--resume <id>` or `--continue`, fork it with `--fork <id>[@point]`, and list them with `cockpit-code sessions`. Compact or prune long histories from the TUI or the CLI.
- **Shell and jobs**: commands run in pipes or a PTY. Long-running commands become background jobs that you and the agent can inspect, send input to or kill.
- **User terminals**: in the TUI, an agent can hand you a private terminal in a right-hand sidebar, for example to log in or type a password. Press `ctrl+]` to type in it and use `/terminal` to show or hide it. You decide what output, if any, the agent sees.
- **Extras**: web search and fetch (Firecrawl or TinyFish), a sandboxed Python `script` tool for multi-step tool calls, skills from `~/.agents/skills`, file attachments such as images and PDFs (`--attach` in headless mode), vim keys, and themes.

## Installation

cockpit-code is built from source. The crates are not published to crates.io, and there are no prebuilt binaries yet.

**Requirements:** Rust 1.99 or newer (2024 edition). Install it with [rustup](https://rustup.rs).

```sh
git clone https://github.com/FlyCockpit/cockpit-code.git
cd cockpit-code

# Install the `cockpit-code` binary into ~/.cargo/bin
cargo install --path apps/cockpit-code

# ...or just build it (binary at target/release/cockpit-code)
cargo build --release
```

To build without the terminal UI (headless and ACP only), add `--no-default-features`.

## Quickstart

```sh
# 1. Scaffold config: a provider template that reads its key from an env var
cockpit-code init --provider anthropic --key-env ANTHROPIC_API_KEY

# 2. Add a model to the provider file (see Configuration), then check that it loads
export ANTHROPIC_API_KEY=...
cockpit-code doctor

# 3. Run
cockpit-code                          # interactive TUI
cockpit-code -p "fix the failing test" # headless, one task
```

`init` provider templates are `openrouter`, `openrouter-anthropic`, `openai`, `openai-chat`, `anthropic` and `command`. Repeat `--provider` to add several. Other useful flags are `--default-model provider/model`, `--default-mode manual|auto|unattended|yolo` and `--safety-model provider/model`. Existing files are kept unless you pass `--force`. `init` never writes a secret. It only writes a `${VAR}` reference.

Other commands:

| Command | What it does |
|---|---|
| `cockpit-code doctor [--probe <provider>]` | Shows the resolved paths and loaded providers and models, and reports any that were skipped. `--probe` tests which API dialects a provider accepts. |
| `cockpit-code discover [provider] [--refresh]` | Lists models from command providers. |
| `cockpit-code provider test <provider>` | Sends a conformance request to a provider. |
| `cockpit-code sessions [--all]` | Lists saved sessions. |
| `cockpit-code compact <id>` / `prune <id>` | Shrinks a saved session's history. |
| `cockpit-code acp` | Serves ACP on stdio. |

Run `cockpit-code --help` for all flags, such as `--model`, `--effort`, `--mode`, `--cwd`, `--flat`, `--resume`, `--continue` and `--fork`.

## Configuration

All configuration lives in one directory. `cockpit-code doctor` prints its location on the first line. By default it is `~/.config/cockpit-code/`, or `~/.cockpit-code/` if `~/.config` does not exist. Set `$COCKPIT_CODE_HOME` to use a different directory.

```
config.json            global settings (default model, permissions, subagents, web, LSP, tools, ...)
providers/<id>.json    one model provider per file
mcps/<id>.json         one MCP server per file
```

Changes are picked up at the next turn boundary, so you don't need to restart.

The full reference ships with cockpit-code as an agent skill, so cockpit-code (or any other agent) can edit its own config for you:

- [Config skill overview](crates/code-engine/skills/cockpit-code-config/SKILL.md)
- [`config.json` keys](crates/code-engine/skills/cockpit-code-config/references/config-json.md)
- [Providers and models](crates/code-engine/skills/cockpit-code-config/references/providers.md)
- [Command providers](crates/code-engine/skills/cockpit-code-config/references/command-providers.md)
- [MCP servers](crates/code-engine/skills/cockpit-code-config/references/mcps.md)
- [Model metadata from models.dev](crates/code-engine/skills/cockpit-code-config/references/models-dev.md)

## Providers

An HTTP provider is a JSON file with a `type` (`anthropic`, `openai-chat` or `openai-responses`), a `baseUrl`, headers that reference environment variables, and a map of models. Any compatible gateway works.

To use a model that has no compatible HTTP API, or one you reach through a CLI subscription, write a **command provider**. cockpit-code runs your executable once per model request and reads JSONL events back. Your executable never has to implement tools or approvals.

- [Command provider protocol](docs/command-provider-protocol.md)
- [Usage and budget protocol](docs/usage-protocol.md): report account limits, resets and credits for `/usage`.
- [Example adapter](examples/command-provider/openai_compatible.py): a dependency-free Python adapter for OpenAI-compatible endpoints.

## Editor integration (ACP)

`cockpit-code acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com) v1 over stdio, so any ACP client can drive it. It accepts the same `--model`, `--effort`, `--mode` and `--flat` flags as the TUI. For example, in Zed you would register it as a custom agent server:

```json
{
  "agent_servers": {
    "cockpit-code": {
      "command": "cockpit-code",
      "args": ["acp"]
    }
  }
}
```

The exact settings format depends on your editor version, so check its ACP documentation.

## Workspace layout

| Path | Description |
|---|---|
| [`apps/cockpit-code`](apps/cockpit-code) | The `cockpit-code` binary: the CLI, `init`, `doctor` and headless mode. |
| [`crates/code-config`](crates/code-config) | Loads configuration, resolves paths and validates provider and MCP files. |
| [`crates/code-engine`](crates/code-engine) | The in-process agent engine: sessions, tools, models, permissions, MCP, LSP and subagents. |
| [`crates/code-tui`](crates/code-tui) | The terminal user interface. |
| [`crates/code-acp`](crates/code-acp) | The Agent Client Protocol stdio server. |

## Contributing

Issues and pull requests are welcome. Before you open a PR, please check that the workspace builds and passes lint:

```sh
cargo build
cargo clippy --workspace --all-targets
```

For larger changes, open an issue first so the approach can be discussed.

## License

[MIT](LICENSE) © FlyCockpit LLC.

Portions of cockpit-code (including parts of the permission policy and the ACP front end) are adapted from FlyCockpit's Cockpit.
