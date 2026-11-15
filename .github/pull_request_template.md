## Summary

<!-- What does this change do, and why? Link related issues (for example "Closes #123"). -->

## Checks run

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] Other (describe):

## Checklist

- [ ] Tests cover the changed behavior.
- [ ] The README, `docs/` and the bundled config skill are updated where the change affects them.
- [ ] No secrets, API keys or personal data in code, tests, fixtures or logs.
