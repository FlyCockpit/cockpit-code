# mcps/<id>.json

One MCP server per file; the file stem is the server id. `/mcp` enables or disables servers per session.

```json
{
  "transport": "stdio",
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-github"],
  "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" },
  "enabledByDefault": false,
  "description": "GitHub issues and PRs",
  "timeouts": { "startupSecs": 30, "callSecs": 120 }
}
```

Remote server:

```json
{
  "transport": "http",
  "url": "https://mcp.example.com/mcp",
  "headers": { "Authorization": "Bearer ${EXAMPLE_MCP_TOKEN}" },
  "description": "Example remote tools"
}
```

| Field | Values | Notes |
|---|---|---|
| `transport` | `stdio` \| `http` \| `sse` | required |
| `command`, `args` | string, string[] | stdio only. `command` expands `${VAR}`; `args` does not. |
| `env` | object | stdio only; `${VAR}` allowed. Credentials go here. |
| `url`, `headers` | string, object | http/sse only; `${VAR}` allowed. Credentials go in `headers`. |
| `enabledByDefault` | bool, default `false` | Initial state in a new session. |
| `description` | string | Shown in `/mcp` and to agents. |
| `timeouts` | `{startupSecs, callSecs}` | |

OAuth is not supported; use header or env tokens. An `/exclusive` session that blocks network also disables `http`/`sse` servers.
