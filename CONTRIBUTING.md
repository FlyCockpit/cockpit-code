# Contributing to Cockpit Code

Thanks for your interest in improving Cockpit Code. Please open an issue to discuss larger changes before you start on them.

Everyone taking part is expected to follow the [Code of Conduct](./CODE_OF_CONDUCT.md).

## Reporting security issues

Do not open public issues or pull requests for security problems. Report them privately as described in [SECURITY.md](./SECURITY.md).

## Licensing

Cockpit Code is licensed under the MIT License (see [LICENSE](./LICENSE)). Contributions are accepted under the same license: by submitting a contribution you agree that it is licensed to the project and its users under the MIT License (inbound = outbound). There is no separate contributor license agreement.

## Development setup

You need Rust 1.99 or newer (2024 edition), installed with [rustup](https://rustup.rs). The code is Unix-only (Linux and macOS). The [README](./README.md#development) lists the common commands, and [Workspace layout](./README.md#workspace-layout) describes the crates.

## Checks

Before opening a pull request, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo build -p cockpit-code --no-default-features --locked   # headless and ACP only
```

CI runs the same checks on Linux and macOS, plus `cargo deny check` (licenses, bans, sources and RustSec advisories; see [deny.toml](./deny.toml)). Tests must not need network access, real provider accounts or your real config directory: use temporary directories and fake providers.

## Pull requests

- Keep each pull request focused on one change, and describe what it does and why.
- Keep the diff to the task. Unrelated refactors, reformatting or dependency bumps belong in their own pull request, and a new dependency should be discussed first.
- Add or update tests for behavior you change.
- Update the README, `docs/` and the bundled config skill (`crates/code-engine/skills/cockpit-code-config/`) when your change affects them.
- Fill in the pull request template, including the checks you ran.
