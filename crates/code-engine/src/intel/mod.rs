//! Session-local code intelligence. Extraction and import resolution are ported
//! from FlyCockpit's Cockpit; this index replaces its database and daemon coupling.
mod impact;
mod js;
pub(crate) mod lang;
mod query;
mod resolve;
use crate::{
    schema::ToolCall,
    tools::{ToolBox, ToolDefinition},
};
use anyhow::{Context, Result, bail};
use lang::{Extraction, Language};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;

const MAX_FILES: usize = 10_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
// search dispatch remains solely for already frozen legacy sessions. New definitions omit it.
pub(crate) const NAMES: &[&str] = &["search", "code", "graph", "context_pack", "change_impact"];
#[derive(Default)]
pub(crate) struct Manager {
    indexes: Mutex<HashMap<PathBuf, Index>>,
}
#[derive(Default)]
struct Index {
    files: BTreeMap<String, File>,
    deps: Vec<Edge>,
    js_aliases: js::Aliases,
    calls: Vec<Call>,
    warnings: Vec<String>,
}
struct File {
    hash: Vec<u8>,
    source: String,
    language: Language,
    extraction: Extraction,
    modified: SystemTime,
}
#[derive(Clone)]
struct Edge {
    from: String,
    to: String,
    line: i64,
}
#[derive(Clone)]
struct Call {
    from: String,
    caller: Option<String>,
    line: i64,
    to: String,
    name: String,
    definition_line: i64,
}
impl Manager {
    fn execute(
        &self,
        root: &Path,
        scope: Option<&str>,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let mut indexes = self.indexes.lock().unwrap();
        if indexes.len() >= 4 && !indexes.contains_key(root) {
            indexes.clear();
        }
        let index = indexes.entry(root.into()).or_default();
        index.refresh(root, cancel)?;
        check(cancel)?;
        let args = &call.arguments;
        let result = match call.name.as_str() {
            "search" => index.search(args, scope, cancel)?,
            "code" => index.code(args, scope)?,
            "graph" => index.graph(args, scope, cancel)?,
            "context_pack" => index.context_pack(root, args, scope, cancel)?,
            "change_impact" => index.change_impact(root, args, scope, cancel)?,
            _ => bail!("unknown intel tool"),
        };
        Ok(json!({"root":root,"warnings":index.warnings,"result":result}).to_string())
    }
}
fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    Ok(())
}
fn inside(path: &str, scope: Option<&str>) -> bool {
    scope.is_none_or(|s| path == s || path.strip_prefix(s).is_some_and(|r| r.starts_with('/')))
}
fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{key} is required"))
}
fn limit(args: &Value, default: usize) -> usize {
    args["limit"]
        .as_u64()
        .map_or(default, |v| v.clamp(1, 1000) as usize)
}
impl Index {
    fn refresh(&mut self, root: &Path, cancel: &CancellationToken) -> Result<()> {
        self.warnings.clear();
        let mut seen = HashSet::new();
        let mut bytes = 0;
        let mut skipped = 0;
        let resolver = crate::paths::Resolver::new(root.into());
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .follow_links(false)
            .build();
        for entry in walker {
            check(cancel)?;
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if seen.len() >= MAX_FILES || bytes >= MAX_SOURCE_BYTES {
                self.warnings.push(format!("Index bounded to {MAX_FILES} files / {MAX_SOURCE_BYTES} source bytes; use a smaller cwd for full coverage"));
                break;
            }
            // A symlink swapped in during the walk never expands this read's scope.
            if resolver.validate(entry.path()).is_err() {
                skipped += 1;
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) if m.len() <= MAX_FILE_BYTES => m,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            let data = match super::tools::read_bytes(entry.path()) {
                Ok(b) if b.len() <= MAX_FILE_BYTES as usize && !b.contains(&0) => b,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            if bytes + data.len() > MAX_SOURCE_BYTES {
                self.warnings.push("Source byte limit reached".into());
                break;
            }
            let source = match String::from_utf8(data) {
                Ok(s) => s,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            bytes += source.len();
            let rel = entry
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            seen.insert(rel.clone());
            let hash = Sha256::digest(source.as_bytes()).to_vec();
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if let Some(file) = self.files.get_mut(&rel)
                && file.hash == hash
            {
                file.modified = modified;
                continue;
            }
            let language = Language::from_path(entry.path());
            let mut extraction = lang::extract(language, source.as_bytes())?;
            if language.grammar().is_none() {
                extraction.symbols = lang::regex_outline(&source)
                    .into_iter()
                    .map(|(name, line)| lang::Symbol {
                        name,
                        line,
                        end_line: line,
                        kind: "symbol".into(),
                        parent: None,
                        visibility: None,
                        signature: source.lines().nth(line as usize - 1).map(str::to_owned),
                    })
                    .collect();
            }
            extraction
                .symbols
                .sort_by(|a, b| (a.line, &a.name, &a.parent).cmp(&(b.line, &b.name, &b.parent)));
            // The source query captures methods both as functions and container members.
            let methods = extraction
                .symbols
                .iter()
                .filter(|s| s.parent.is_some())
                .map(|s| (s.line, s.name.clone()))
                .collect::<HashSet<_>>();
            extraction
                .symbols
                .retain(|s| s.parent.is_some() || !methods.contains(&(s.line, s.name.clone())));
            self.files.insert(
                rel,
                File {
                    hash,
                    source,
                    language,
                    extraction,
                    modified,
                },
            );
        }
        self.files.retain(|path, _| seen.contains(path));
        if skipped > 0 {
            self.warnings.push(format!(
                "Skipped {skipped} unreadable, non-UTF-8, binary, symlinked or >2 MiB files"
            ));
        }
        self.rebuild(cancel)
    }
    fn rebuild(&mut self, cancel: &CancellationToken) -> Result<()> {
        self.js_aliases = js::Aliases::new(
            self.files
                .iter()
                .map(|(path, f)| (path.as_str(), f.source.as_str())),
        );
        self.deps.clear();
        self.calls.clear();
        let existing = self.files.keys().cloned().collect::<HashSet<_>>();
        // Resolve Go modules relative to their own module root, including nested modules.
        let modules = self
            .files
            .iter()
            .filter(|(p, _)| p.ends_with("go.mod"))
            .filter_map(|(path, f)| {
                f.source
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("module "))
                    .map(|m| {
                        (
                            path.trim_end_matches("go.mod").to_owned(),
                            m.trim().to_owned(),
                        )
                    })
            })
            .collect::<Vec<_>>();
        let mut defs: HashMap<&str, Vec<(&str, &lang::Symbol)>> = HashMap::new();
        for (path, file) in &self.files {
            for symbol in &file.extraction.symbols {
                defs.entry(&symbol.name).or_default().push((path, symbol));
            }
        }
        for (path, file) in &self.files {
            check(cancel)?;
            let module = modules
                .iter()
                .filter(|(dir, _)| path.starts_with(dir))
                .max_by_key(|(dir, _)| dir.len());
            let (dir, prefix) = module
                .map(|(d, p)| (d.as_str(), p.as_str()))
                .unwrap_or(("", ""));
            for import in &file.extraction.imports {
                let resolved = if file.language == Language::Go && !dir.is_empty() {
                    let local = existing
                        .iter()
                        .filter_map(|p| p.strip_prefix(dir).map(str::to_owned))
                        .collect();
                    resolve::resolve(
                        file.language,
                        path.strip_prefix(dir).unwrap_or(path),
                        &import.target,
                        &local,
                        prefix,
                        &self.js_aliases,
                    )
                    .map(|p| format!("{dir}{p}"))
                } else {
                    resolve::resolve(
                        file.language,
                        path,
                        &import.target,
                        &existing,
                        prefix,
                        &self.js_aliases,
                    )
                };
                if let Some(to) = resolved
                    && &to != path
                {
                    self.deps.push(Edge {
                        from: path.clone(),
                        to,
                        line: import.line,
                    });
                }
            }
            // Rust `mod foo;` is a dependency even without a use declaration.
            if file.language == Language::Rust {
                for symbol in &file.extraction.symbols {
                    if symbol.kind == "mod" {
                        let parent = Path::new(path).parent().unwrap_or(Path::new(""));
                        let stem = Path::new(path)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("");
                        let base = if matches!(stem, "lib" | "main" | "mod") {
                            parent.to_path_buf()
                        } else {
                            parent.join(stem)
                        };
                        for candidate in [
                            base.join(format!("{}.rs", symbol.name)),
                            base.join(&symbol.name).join("mod.rs"),
                        ] {
                            let to = candidate.to_string_lossy().replace('\\', "/");
                            if existing.contains(&to) {
                                self.deps.push(Edge {
                                    from: path.clone(),
                                    to,
                                    line: symbol.line,
                                });
                                break;
                            }
                        }
                    }
                }
            }
            for site in &file.extraction.callsites {
                if DENYLIST.contains(&site.callee_name.as_str()) {
                    continue;
                }
                let candidates = defs
                    .get(site.callee_name.as_str())
                    .into_iter()
                    .flatten()
                    .filter(|(_, s)| match site.callee_kind.as_str() {
                        "type_ref" => matches!(
                            s.kind.as_str(),
                            "struct" | "enum" | "type" | "class" | "interface"
                        ),
                        "macro" => false,
                        _ => matches!(s.kind.as_str(), "function" | "method"),
                    })
                    .collect::<Vec<_>>();
                // As upstream: omit ambiguous calls; type precision belongs to LSP.
                if let &[&(to, symbol)] = candidates.as_slice() {
                    self.calls.push(Call {
                        from: path.clone(),
                        caller: site.caller_symbol.clone(),
                        line: site.caller_line,
                        to: to.into(),
                        name: site.callee_name.clone(),
                        definition_line: symbol.line,
                    });
                }
            }
        }
        self.deps
            .sort_by(|a, b| (&a.from, &a.to, a.line).cmp(&(&b.from, &b.to, b.line)));
        self.deps.dedup_by(|a, b| a.from == b.from && a.to == b.to);
        Ok(())
    }
    fn symbol_rows(
        &self,
        name: &str,
        exact: bool,
        kind: Option<&str>,
        scope: Option<&str>,
        cap: usize,
    ) -> Vec<Value> {
        self.files
            .iter()
            .filter(|(p, _)| inside(p, scope))
            .flat_map(|(path, file)| {
                file.extraction
                    .symbols
                    .iter()
                    .filter(move |s| {
                        (if exact {
                            s.name == name
                        } else {
                            s.name.starts_with(name)
                        }) && kind.is_none_or(|k| k == s.kind)
                    })
                    .map(move |s| json!({"path":path,"language":file.language.as_str(),"symbol":s}))
            })
            .take(cap)
            .collect()
    }
}
// Shared upstream denylist: generic container methods provide misleading edges.
const DENYLIST: &[&str] = &[
    "init",
    "new",
    "get",
    "set",
    "append",
    "push",
    "pop",
    "next",
    "lock",
    "unlock",
    "len",
    "clone",
    "iter",
    "map",
    "unwrap",
    "into",
    "from",
    "to_string",
];

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let string = json!({"type":"string"});
    let boolean = json!({"type":"boolean"});
    let count = json!({"type":"integer","minimum":1,"maximum":1000});
    let mut outline_limit = count.clone();
    outline_limit["description"] = json!(
        "Outline defaults to 40 items, at most 100 and 5000 item bytes per page; filter name/symbol_kind, then continue with next_offset. Other code kinds default 100."
    );
    let mut impact_limit = count.clone();
    impact_limit["maximum"] = json!(200);
    impact_limit["description"] = json!(
        "Default 20, max 200. Bounds files and each nested record list; also bounds total serialized result to clamp(limit*1200,2400,24000) bytes. Inspect counts/truncated."
    );
    let def = |name: &str, description: &str, properties: Value, required: Value| ToolDefinition {
        name: name.into(),
        description: description.into(),
        schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    };
    vec![
        def(
            "code",
            "Find syntax definitions with kind=symbol_find and name; kind=outline and path gives names/start/end lines. Prefer over declaration regexes or broad reads for structure. Batch/filter outlines in script before printing, e.g. o=code(kind=\"outline\",path=\"src/main.ts\"); print([(s[\"name\"],s[\"line\"],s[\"end_line\"]) for s in o[\"result\"][\"symbols\"] if s[\"kind\"]==\"function\"]). Spelling-based, not type-resolved. Unsupported languages use regex fallback; inspect truncated/parser and page when needed.",
            json!({"kind":{"type":"string","enum":["tree","outline","symbol_find","word"]},"path":string,"name":string,"token":string,"exact":boolean,"symbol_kind":string,"case_insensitive":boolean,"limit":outline_limit,"offset":{"type":"integer","minimum":1,"description":"Outline: 1-based item offset; continue with next_offset. limit defaults 40, max 100, within a 5000-byte item budget. name/symbol_kind filter before paging."}}),
            json!(["kind"]),
        ),
        def(
            "graph",
            "Get import/dependency or unambiguous caller shortlists. Prefer over repeated manual edge searches across several files/hops. Index caps: 10000 files/64 MiB/2 MiB per file. Dynamic, ambiguous and unresolved alias edges may be absent; absence of results is not proof. Confirm with text/source or available LSP.",
            json!({"kind":{"type":"string","enum":["deps","importers","cycles","callers","calls","recent"]},"path":string,"name":string,"hops":{"type":"integer","minimum":1,"maximum":10},"symbol_kind":string,"limit":count}),
            json!(["kind"]),
        ),
        def(
            "context_pack",
            "Bundle a bounded map for a specific path/symbol/query spanning several files. Use before repeated orientation reads when that target is known. Not a complete project overview or substitute for implementation/source contracts; inspect bounds. Index caps: 10000 files/64 MiB/2 MiB per file; absence of results is not proof.",
            json!({"target":string,"kind":{"type":"string","enum":["auto","overview","path","symbol","query"]},"depth":{"type":"integer","minimum":1,"maximum":3},"limit":count}),
            json!([]),
        ),
        def(
            "change_impact",
            "For a requested diff blast radius, return hunks/symbols and heuristic downstream candidates. State base/target: default worktree/index/untracked vs HEAD; base alone is base..HEAD. Prefer git diff for patch reading; scope narrowly. Index caps: 10000 files/64 MiB/2 MiB per file. Risk and absence of results are not proof.",
            json!({"base":string,"target":string,"path":string,"depth":{"type":"integer","minimum":1,"maximum":3},"include_tests":boolean,"limit":impact_limit}),
            json!([]),
        ),
    ]
}
pub(crate) fn target(resolver: &crate::paths::Resolver, call: &ToolCall) -> Result<PathBuf> {
    let args = &call.arguments;
    if call.name == "context_pack" {
        let target = args["target"].as_str().unwrap_or(".");
        let path = resolver.resolve(target)?;
        if args["kind"] == "path"
            || (args["kind"].as_str().is_none_or(|k| k == "auto") && path.exists())
        {
            return Ok(path);
        }
        return Ok(resolver.cwd.clone());
    }
    resolver.resolve(args["path"].as_str().unwrap_or("."))
}
impl ToolBox {
    pub(crate) async fn intelligence(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let target = target(&self.resolver, call)?;
        self.resolver.validate(&target)?;
        let root = if target.starts_with(&self.cwd) {
            self.cwd.clone()
        } else if target.is_dir() {
            target.clone()
        } else {
            target.parent().context("path has no parent")?.into()
        };
        if !root.starts_with(&self.cwd) && root != target {
            self.authorize_read_path(&root, cancel).await?;
        }
        let scope = if target == root {
            None
        } else {
            Some(
                target
                    .strip_prefix(&root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        };
        let (manager, call, cancel) = (self.intel.clone(), call.clone(), cancel.clone());
        // Always join blocking work, including cancellation, before Session shutdown returns.
        tokio::task::spawn_blocking(move || {
            manager.execute(&root, scope.as_deref(), &call, &cancel)
        })
        .await?
    }
}
