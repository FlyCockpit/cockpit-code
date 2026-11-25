# Agent notes

- [`deslop-please`](https://github.com/deterministic-machines/deslop-please) lints code structure (module cycles, tangles, field leaks, shallow modules, long files, wide directories); fix what it finds from first principles, never by moves that only hide a finding.
- Run it at the workspace root: `deslop-please --allow .deslop-please.allow`.
- `.deslop-please.allow` lists existing debt that is muted: fixing an entry never fails, a new finding fails.
- `deslop-please --update-allow .deslop-please.allow` regenerates the file and drops any comments in it.
