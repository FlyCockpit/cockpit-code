use super::*;
use std::collections::{BTreeSet, VecDeque};
impl Index {
    pub(super) fn search(
        &self,
        args: &Value,
        scope: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let regex = regex::RegexBuilder::new(required(args, "pattern")?)
            .case_insensitive(args["case_insensitive"] == true)
            .size_limit(2 * 1024 * 1024)
            .build()?;
        let glob = args["glob"]
            .as_str()
            .map(globset::Glob::new)
            .transpose()?
            .map(|g| g.compile_matcher());
        let context = args["context"].as_u64().unwrap_or(0).min(10) as usize;
        let cap = limit(args, 100);
        let mut hits = Vec::new();
        let mut truncated = false;
        'files: for (path, file) in &self.files {
            check(cancel)?;
            if !inside(path, scope)
                || glob.as_ref().is_some_and(|g| {
                    !g.is_match(path)
                        && !g.is_match(Path::new(path).file_name().unwrap_or_default())
                })
            {
                continue;
            }
            let lines = file.source.lines().collect::<Vec<_>>();
            for (i, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                if hits.len() == cap {
                    truncated = true;
                    break 'files;
                }
                let excerpt = (i.saturating_sub(context)..=(i + context).min(lines.len() - 1))
                    .map(|n| json!({"line":n+1,"text":short(lines[n], 600)}))
                    .collect::<Vec<_>>();
                hits.push(json!({"path":path,"line":i+1,"text":short(line,600),"context":excerpt}));
            }
        }
        Ok(json!({"hits":hits,"truncated":truncated}))
    }
    pub(super) fn code(&self, args: &Value, scope: Option<&str>) -> Result<Value> {
        let cap = limit(args, 100);
        match required(args, "kind")? {
            "tree" => {
                let rows = self.files.iter().filter(|(p, _)| inside(p, scope)).take(cap + 1).map(|(path, f)| json!({"path":path,"language":f.language.as_str(),"bytes":f.source.len(),"symbols":f.extraction.symbols.len(),"sha256":format_hash(&f.hash)})).collect::<Vec<_>>();
                Ok(bounded(rows, cap))
            }
            "outline" => {
                let path = scope.context("path is required for outline")?;
                let f = self.files.get(path).context(
                    "file not indexed (missing, ignored, binary, non-UTF-8, symlinked or >2 MiB)",
                )?;
                let name = args["name"].as_str();
                let kind = args["symbol_kind"].as_str();
                let symbols = f
                    .extraction
                    .symbols
                    .iter()
                    .filter(|s| {
                        name.is_none_or(|n| {
                            if args["exact"] == true {
                                s.name == n
                            } else {
                                s.name.starts_with(n)
                            }
                        }) && kind.is_none_or(|k| s.kind == k)
                    })
                    .collect::<Vec<_>>();
                let mut kinds = BTreeMap::<&str, usize>::new();
                for symbol in &f.extraction.symbols {
                    *kinds.entry(&symbol.kind).or_default() += 1;
                }
                let imports = if name.is_none() && kind.is_none() {
                    f.extraction.imports.as_slice()
                } else {
                    &[]
                };
                let total = symbols.len() + imports.len();
                let skip = args["offset"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
                let cap = limit(args, 40).min(100);
                let rows =
                    symbols
                        .into_iter()
                        .map(|s| (true, compact_symbol(s)))
                        .chain(imports.iter().map(|i| {
                            (false, json!({"target":short(&i.target,360),"line":i.line}))
                        }));
                let mut page_symbols = Vec::new();
                let mut page_imports = Vec::new();
                let mut bytes = 0;
                let mut returned = 0;
                for (symbol, row) in rows.skip(skip).take(cap) {
                    let size = row.to_string().len() + 1;
                    if bytes + size > 5000 {
                        break;
                    }
                    bytes += size;
                    returned += 1;
                    if symbol {
                        page_symbols.push(row);
                    } else {
                        page_imports.push(row);
                    }
                }
                let next = skip.saturating_add(returned);
                let truncated = next < total;
                Ok(
                    json!({"path":path,"language":f.language.as_str(),"symbols":page_symbols,"imports":page_imports,
                    "summary":{"total_symbols":f.extraction.symbols.len(),"total_imports":f.extraction.imports.len(),"symbol_kinds":kinds},
                    "total":total,"offset":skip.saturating_add(1),"next_offset":if truncated{Some(next.saturating_add(1))}else{None},
                    "truncated":truncated,"item_bytes_limit":5000,"max_items":100,
                    "parser":if f.language.grammar().is_some(){"tree-sitter"}else{"regex fallback"},
                    "next":"Filter with name/symbol_kind, or continue outline with next_offset. Batch/filter before printing."}),
                )
            }
            "symbol_find" => Ok(bounded(
                self.symbol_rows(
                    required(args, "name")?,
                    args["exact"] == true,
                    args["symbol_kind"].as_str(),
                    scope,
                    cap + 1,
                ),
                cap,
            )),
            "word" => {
                let token = required(args, "token")?;
                let ci = args["case_insensitive"] == true;
                let mut rows = Vec::new();
                for (path, file) in &self.files {
                    if !inside(path, scope) {
                        continue;
                    }
                    let mut seen = BTreeSet::new();
                    for id in &file.extraction.identifiers {
                        if (if ci {
                            id.token.to_lowercase() == token.to_lowercase()
                        } else {
                            id.token == token
                        }) && seen.insert(id.line)
                        {
                            rows.push(json!({"path":path,"line":id.line,"token":id.token}));
                            if rows.len() > cap {
                                return Ok(bounded(rows, cap));
                            }
                        }
                    }
                }
                Ok(bounded(rows, cap))
            }
            other => bail!("unsupported code kind {other}"),
        }
    }
    pub(super) fn graph(
        &self,
        args: &Value,
        scope: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let cap = limit(args, 100);
        let kind = required(args, "kind")?;
        let rows = match kind {
            "deps" | "importers" => {
                let path = scope.context("path is required for deps/importers")?;
                let starts = self
                    .files
                    .keys()
                    .filter(|p| inside(p, Some(path)))
                    .cloned()
                    .collect::<Vec<_>>();
                self.related(
                    &starts,
                    kind == "importers",
                    args["hops"].as_u64().unwrap_or(1).clamp(1, 10) as usize,
                    cap + 1,
                    cancel,
                )?
                .into_iter()
                .map(|(path, hop)| json!({"path":path,"hop":hop}))
                .collect()
            }
            "callers" | "calls" => {
                let name = required(args, "name")?;
                let allowed = |path: &str, symbol: &str| {
                    args["symbol_kind"].as_str().is_none_or(|kind| {
                        self.files.get(path).is_some_and(|f| {
                            f.extraction
                                .symbols
                                .iter()
                                .any(|s| s.name == symbol && s.kind == kind)
                        })
                    })
                };
                self.calls.iter().filter(|c| if kind == "callers" { c.name == name && allowed(&c.to,&c.name) && inside(&c.to,scope) } else { c.caller.as_deref() == Some(name) && allowed(&c.from,name) && inside(&c.from,scope) }).take(cap+1).map(|c| json!({"caller_path":c.from,"caller":c.caller,"line":c.line,"callee_path":c.to,"callee":c.name,"definition_line":c.definition_line})).collect()
            }
            "cycles" => self.cycles(scope, cap + 1, cancel)?,
            "recent" => {
                let mut files = self
                    .files
                    .iter()
                    .filter(|(p, _)| inside(p, scope))
                    .collect::<Vec<_>>();
                files.sort_by(|(pa, a), (pb, b)| b.modified.cmp(&a.modified).then(pa.cmp(pb)));
                files.into_iter().take(cap+1).map(|(path,f)| json!({"path":path,"modified_unix_secs":f.modified.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs()})).collect()
            }
            other => bail!("unsupported graph kind {other}"),
        };
        let mut value = bounded(rows, cap);
        value["heuristic"] = json!(true);
        value["note"] = json!(
            "Imports resolve only within indexed files; ambiguous/dynamic calls and common container methods are omitted."
        );
        Ok(value)
    }
    pub(super) fn related(
        &self,
        starts: &[String],
        reverse: bool,
        depth: usize,
        cap: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<(String, usize)>> {
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.deps {
            let (from, to) = if reverse {
                (&edge.to, &edge.from)
            } else {
                (&edge.from, &edge.to)
            };
            adjacency.entry(from).or_default().push(to);
        }
        let mut seen = starts.iter().cloned().collect::<HashSet<_>>();
        let mut queue = starts
            .iter()
            .map(|p| (p.as_str(), 0))
            .collect::<VecDeque<_>>();
        let mut rows = Vec::new();
        while let Some((path, hop)) = queue.pop_front() {
            check(cancel)?;
            if hop == depth {
                continue;
            }
            for to in adjacency.get(path).into_iter().flatten() {
                if seen.insert((*to).into()) {
                    rows.push(((*to).into(), hop + 1));
                    if rows.len() >= cap {
                        return Ok(rows);
                    }
                    queue.push_back((to, hop + 1));
                }
            }
        }
        Ok(rows)
    }
    // Iterative DFS avoids stack overflow on generated or adversarial import chains.
    fn cycles(
        &self,
        scope: Option<&str>,
        cap: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>> {
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &self.deps {
            if inside(&e.from, scope) && inside(&e.to, scope) {
                adjacency.entry(&e.from).or_default().push(&e.to);
            }
        }
        let mut color = HashMap::new();
        let mut result = Vec::new();
        for start in adjacency.keys() {
            if color.contains_key(start) {
                continue;
            }
            let mut stack = vec![(*start, 0)];
            let mut positions = HashMap::new();
            color.insert(*start, 1);
            positions.insert(*start, 0);
            while let Some((node, next)) = stack.last_mut() {
                check(cancel)?;
                let children = adjacency.get(node).map(Vec::as_slice).unwrap_or_default();
                if *next == children.len() {
                    color.insert(*node, 2);
                    positions.remove(node);
                    stack.pop();
                    continue;
                }
                let child = children[*next];
                *next += 1;
                match color.get(child) {
                    Some(1) => {
                        if let Some(&pos) = positions.get(child) {
                            let mut cycle =
                                stack[pos..].iter().map(|(p, _)| *p).collect::<Vec<_>>();
                            cycle.push(child);
                            result.push(json!({"cycle":cycle}));
                            if result.len() >= cap {
                                return Ok(result);
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        color.insert(child, 1);
                        positions.insert(child, stack.len());
                        stack.push((child, 0));
                    }
                }
            }
        }
        Ok(result)
    }
    pub(super) fn context_pack(
        &self,
        root: &Path,
        args: &Value,
        scope: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let target = args["target"].as_str().unwrap_or("");
        let cap = limit(args, 12).min(50);
        let depth = args["depth"].as_u64().unwrap_or(1).clamp(1, 3) as usize;
        let kind = match args["kind"].as_str().unwrap_or("auto") {
            "auto" if target.is_empty() || target == "." => "overview",
            "auto" if scope.is_some() || root.join(target).exists() => "path",
            "auto" if !self.symbol_rows(target, true, None, None, 1).is_empty() => "symbol",
            "auto" => "query",
            kind => kind,
        };
        let mut paths = BTreeSet::new();
        let mut definitions = Vec::new();
        let mut hits = Value::Null;
        match kind {
            "overview" => {
                let mut ranked = self.files.iter().collect::<Vec<_>>();
                let mut degree = HashMap::<&str, usize>::new();
                for e in &self.deps {
                    *degree.entry(&e.to).or_default() += 1;
                }
                for c in &self.calls {
                    *degree.entry(&c.to).or_default() += 1;
                }
                ranked.sort_by(|(pa, a), (pb, b)| {
                    let score = |p: &str, f: &File| {
                        degree.get(p).copied().unwrap_or(0)
                            + f.extraction.symbols.len().min(10)
                            + if p.ends_with("main.rs")
                                || p.ends_with("lib.rs")
                                || p.ends_with("README.md")
                            {
                                20
                            } else {
                                0
                            }
                    };
                    score(pb, b).cmp(&score(pa, a)).then(pa.cmp(pb))
                });
                paths.extend(ranked.into_iter().take(cap).map(|(p, _)| p.clone()));
            }
            "path" => {
                let path = scope.context("target must name a path for a path packet")?;
                paths.extend(
                    self.files
                        .keys()
                        .filter(|p| inside(p, Some(path)))
                        .take(cap)
                        .cloned(),
                );
            }
            "symbol" => {
                definitions = self.symbol_rows(required(args, "target")?, true, None, None, cap);
                for row in &definitions {
                    if let Some(path) = row["path"].as_str() {
                        paths.insert(path.into());
                    }
                }
            }
            "query" => {
                hits = self.search(&json!({"pattern":regex::escape(required(args,"target")?),"limit":cap,"case_insensitive":true}),None,cancel)?;
                if let Some(rows) = hits["hits"].as_array() {
                    for row in rows {
                        if let Some(path) = row["path"].as_str() {
                            paths.insert(path.into());
                        }
                    }
                }
            }
            other => bail!("unsupported context_pack kind {other}"),
        }
        let starts = paths.into_iter().collect::<Vec<_>>();
        let deps = self.related(&starts, false, depth, cap, cancel)?;
        let importers = self.related(&starts, true, depth, cap, cancel)?;
        let packets = starts.iter().filter_map(|path| self.files.get(path).map(|file| {
            let symbols = file.extraction.symbols.iter().take(12).collect::<Vec<_>>();
            let line = definitions.iter().find(|d| d["path"] == path.as_str()).and_then(|d| d["symbol"]["line"].as_u64()).unwrap_or(1);
            let excerpt = file.source.lines().enumerate().skip(line.saturating_sub(1) as usize).take(8).map(|(i,l)| json!({"line":i+1,"text":short(l,300)})).collect::<Vec<_>>();
            json!({"path":path,"language":file.language.as_str(),"symbols":symbols,"excerpt":excerpt})
        })).collect::<Vec<_>>();
        let callers = self
            .calls
            .iter()
            .filter(|c| starts.contains(&c.to))
            .take(cap)
            .map(|c| json!({"path":c.from,"line":c.line,"caller":c.caller,"callee":c.name}))
            .collect::<Vec<_>>();
        Ok(
            json!({"kind":kind,"target":target,"files":packets,"definitions":definitions,"text_hits":hits,"dependencies":deps,"importers":importers,"callers":callers,"heuristic":true,"bounded":true,"next":"Use read for full source and lsp for semantic precision."}),
        )
    }
}
fn short(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
fn bounded(mut rows: Vec<Value>, cap: usize) -> Value {
    let truncated = rows.len() > cap;
    rows.truncate(cap);
    json!({"items":rows,"truncated":truncated})
}
fn format_hash(hash: &[u8]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stable compact projection; signatures and identifier strings cannot expand a page arbitrarily.
pub(super) fn compact_symbol(s: &lang::Symbol) -> Value {
    json!({"name":short(&s.name,240),"kind":s.kind,"line":s.line,"end_line":s.end_line,
        "parent":s.parent.as_deref().map(|p|short(p,240)),"visibility":s.visibility.as_deref().map(|v|short(v,80)),
        "signature":s.signature.as_deref().map(|v|short(v,180))})
}
