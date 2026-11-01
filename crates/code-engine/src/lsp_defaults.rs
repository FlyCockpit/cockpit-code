//! Session-start resolution shared by navigation, post-write diagnostics and CLI advice.
use code_config::lsp::{LspConfig, LspServer};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultServer {
    #[serde(flatten)]
    server: LspServer,
    #[serde(default)]
    alternatives: Vec<String>,
    install: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub servers: BTreeMap<String, LspServer>,
    pub hints: Vec<String>,
    pub scan_truncated: bool,
}
fn executable(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let check = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return check(Path::new(command));
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| check(&p.join(command))))
}
fn matches(server: &LspServer, path: &Path) -> bool {
    server.file_globs.iter().any(|pattern| {
        globset::Glob::new(pattern).is_ok_and(|g| {
            let matcher = g.compile_matcher();
            matcher.is_match(path)
                || path
                    .file_name()
                    .is_some_and(|n| matcher.is_match(Path::new(n)))
        })
    })
}
/// Never downloads or starts a server. Only executable binaries and observed project languages qualify.
pub fn resolve(config: &LspConfig, cwd: &Path) -> Detection {
    let defaults: BTreeMap<String, DefaultServer> =
        serde_json::from_str(include_str!("builtin/lsp-defaults.json"))
            .expect("builtin LSP defaults");
    let mut candidates = BTreeMap::new();
    let mut hints = Vec::new();
    for (id, default) in &defaults {
        let mut server = default.server.clone();
        if !executable(&server.command)
            && let Some(command) = default.alternatives.iter().find(|c| executable(c))
        {
            server.command.clone_from(command);
        }
        candidates.insert(id.clone(), server);
    }
    for (id, patch) in &config.servers {
        if patch.enabled == Some(false) {
            candidates.remove(id);
            continue;
        }
        let server = candidates
            .entry(id.clone())
            .or_insert_with(LspServer::default);
        if let Some(v) = &patch.command {
            server.command.clone_from(v);
        }
        if let Some(v) = &patch.args {
            server.args.clone_from(v);
        }
        if let Some(v) = &patch.file_globs {
            server.file_globs.clone_from(v);
        }
        if let Some(v) = &patch.root_markers {
            server.root_markers.clone_from(v);
        }
    }
    let mut observed = std::collections::BTreeSet::new();
    let mut count = 0;
    let mut scan_truncated = false;
    for entry in ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .follow_links(false)
        .build()
        .flatten()
    {
        count += 1;
        if count > 100_000 {
            scan_truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path().strip_prefix(cwd).unwrap_or(entry.path());
        for (id, server) in &candidates {
            if !observed.contains(id) && matches(server, path) {
                observed.insert(id.clone());
            }
        }
    }
    let mut servers = BTreeMap::new();
    for (id, server) in candidates {
        if !observed.contains(&id) {
            continue;
        }
        if executable(&server.command) {
            servers.insert(id, server);
        } else {
            let install = defaults
                .get(&id)
                .map(|d| d.install.as_str())
                .unwrap_or("Install the configured language server and put its command on PATH");
            hints.push(format!("{id}: {} unavailable. {install}", server.command));
        }
    }
    Detection {
        servers,
        hints,
        scan_truncated,
    }
}
