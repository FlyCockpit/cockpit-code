//! Git diff blast-radius hints, adapted from FlyCockpit's Cockpit.
use super::*;
use std::os::unix::process::CommandExt;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
impl Index {
    pub(super) fn change_impact(
        &self,
        root: &Path,
        args: &Value,
        scope: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if git(
            root,
            &["rev-parse".into(), "--is-inside-work-tree".into()],
            cancel,
        )
        .is_err()
        {
            check(cancel)?;
            return Ok(json!({"status":"unavailable","message":"No git worktree at this cwd"}));
        }
        let base = args["base"].as_str();
        let target = args["target"].as_str();
        if target.is_some() && base.is_none() {
            bail!("target requires base");
        }
        let resolve = |r: &str| -> Result<String> {
            if r.is_empty() || r.starts_with('-') || r.contains(['\0', '\n']) {
                bail!("invalid git ref");
            }
            Ok(String::from_utf8(git(
                root,
                &[
                    "rev-parse".into(),
                    "--verify".into(),
                    "--end-of-options".into(),
                    format!("{r}^{{commit}}"),
                ],
                cancel,
            )?)?
            .trim()
            .into())
        };
        let baseline = resolve(base.unwrap_or("HEAD"))?;
        let target_id = if base.is_some() {
            Some(resolve(target.unwrap_or("HEAD"))?)
        } else {
            None
        };
        let depth = args["depth"].as_u64().unwrap_or(1).clamp(1, 3) as usize;
        let cap = limit(args, 20).min(200);
        let include_tests = args["include_tests"].as_bool().unwrap_or(true);
        let mut diff_args = vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--relative".into(),
            "--find-renames".into(),
            "--name-status".into(),
            "-z".into(),
            baseline.clone(),
        ];
        if let Some(target) = &target_id {
            diff_args.push(target.clone());
        }
        diff_args.extend(["--".into(), scope.unwrap_or(".").into()]);
        let raw = git(root, &diff_args, cancel)?;
        let fields = raw
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>();
        let mut changed: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        let mut i = 0;
        while i + 1 < fields.len() {
            let status = fields[i].clone();
            let path = fields[i + 1].clone();
            i += 2;
            if status.starts_with(['R', 'C']) && i < fields.len() {
                changed.insert(fields[i].clone(), (status, Some(path)));
                i += 1;
            } else {
                changed.insert(path, (status, None));
            }
        }
        if base.is_none() {
            for path in git(
                root,
                &[
                    "ls-files".into(),
                    "--others".into(),
                    "--exclude-standard".into(),
                    "-z".into(),
                    "--".into(),
                    scope.unwrap_or(".").into(),
                ],
                cancel,
            )?
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            {
                changed.insert(
                    String::from_utf8_lossy(path).into_owned(),
                    ("?".into(), None),
                );
            }
        }
        let prefix = String::from_utf8(git(
            root,
            &["rev-parse".into(), "--show-prefix".into()],
            cancel,
        )?)?
        .trim()
        .to_owned();
        let mut results = Vec::new();
        for (path, (status, old_path)) in changed.iter().take(cap) {
            check(cancel)?;
            let mut hunks = Vec::new();
            let mut old_hunks = Vec::new();
            let mut binary = false;
            if status == "?" {
                if let Some(f) = self.files.get(path) {
                    hunks.push((1, f.source.lines().count().max(1) as i64));
                }
            } else {
                let mut argv = vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--relative".into(),
                    "--unified=0".into(),
                    "--find-renames".into(),
                    baseline.clone(),
                ];
                if let Some(target) = &target_id {
                    argv.push(target.clone());
                }
                argv.extend(["--".into(), path.clone()]);
                if let Some(old) = old_path {
                    argv.push(old.clone());
                }
                let patch = git(root, &argv, cancel)?;
                let patch = String::from_utf8_lossy(&patch);
                binary = patch
                    .lines()
                    .any(|l| l.starts_with("Binary files") || l.starts_with("GIT binary patch"));
                for line in patch.lines().filter(|l| l.starts_with("@@ ")) {
                    let fields = line.split_whitespace().collect::<Vec<_>>();
                    if fields.len() >= 3 {
                        if let Some(range) = hunk_range(fields[1]) {
                            old_hunks.push(range);
                        }
                        if let Some(range) = hunk_range(fields[2]) {
                            hunks.push(range);
                        }
                    }
                }
            }
            let mut symbols = Vec::new();
            if let Some(file) = self.files.get(path) {
                symbols.extend(
                    file.extraction
                        .symbols
                        .iter()
                        .filter(|s| overlaps(s, &hunks))
                        .cloned(),
                );
            }
            // Retain deleted/renamed definitions from the baseline as impact seeds.
            if !old_hunks.is_empty() {
                let old = old_path.as_deref().unwrap_or(path);
                if let Ok(source) = git(
                    root,
                    &["show".into(), format!("{baseline}:{prefix}{old}")],
                    cancel,
                ) && source.len() <= MAX_FILE_BYTES as usize
                {
                    let ex = lang::extract(Language::from_path(Path::new(old)), &source)?;
                    symbols.extend(ex.symbols.into_iter().filter(|s| overlaps(s, &old_hunks)));
                }
            }
            symbols.sort_by(|a, b| (&a.name, a.line).cmp(&(&b.name, b.line)));
            symbols.dedup_by(|a, b| a.name == b.name && a.line == b.line);
            let mut starts = vec![path.clone()];
            if let Some(old) = old_path {
                starts.push(old.clone());
            }
            let mut importers = self.related(&starts, true, depth, 100, cancel)?;
            // Resolve imports against removed paths too; the current index omits them.
            let mut existing = self.files.keys().cloned().collect::<HashSet<_>>();
            existing.extend(starts.iter().cloned());
            for (importer, file) in &self.files {
                for import in &file.extraction.imports {
                    if let Some(to) = resolve::resolve(
                        file.language,
                        importer,
                        &import.target,
                        &existing,
                        "",
                        &self.js_aliases,
                    ) && starts.contains(&to)
                        && !importers.iter().any(|(p, _)| p == importer)
                    {
                        importers.push((importer.clone(), 1));
                    }
                }
            }
            let names = symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<HashSet<_>>();
            let mut caller_files = HashSet::new();
            let mut calls = Vec::new();
            let mut candidate_count = 0;
            for (caller, file) in &self.files {
                for site in &file.extraction.callsites {
                    if names.contains(site.callee_name.as_str())
                        && !DENYLIST.contains(&site.callee_name.as_str())
                        && (include_tests || !test_path(caller))
                    {
                        caller_files.insert(caller.clone());
                        candidate_count += 1;
                        if calls.len() < cap {
                            calls.push(json!({"path":caller,"line":site.caller_line,"caller":site.caller_symbol,"callee":site.callee_name}));
                        }
                    }
                }
            }
            let mut frontier = caller_files.clone();
            for _ in 1..depth {
                let exposed = frontier
                    .iter()
                    .filter_map(|p| self.files.get(p))
                    .flat_map(|f| f.extraction.symbols.iter().map(|s| s.name.as_str()))
                    .collect::<HashSet<_>>();
                let mut next = HashSet::new();
                for (caller, file) in &self.files {
                    if !caller_files.contains(caller)
                        && (include_tests || !test_path(caller))
                        && file.extraction.callsites.iter().any(|site| {
                            exposed.contains(site.callee_name.as_str())
                                && !DENYLIST.contains(&site.callee_name.as_str())
                        })
                    {
                        next.insert(caller.clone());
                    }
                }
                caller_files.extend(next.iter().cloned());
                frontier = next;
            }
            if !include_tests {
                importers.retain(|(p, _)| !test_path(p));
            }
            let affected = importers.len() + caller_files.len();
            let risk = if !include_tests && test_path(path) {
                "low"
            } else if binary
                || status == "U"
                || status == "D"
                || affected > 10
                || symbols.iter().any(|s| {
                    s.visibility
                        .as_deref()
                        .is_some_and(|v| v.starts_with("pub"))
                })
            {
                "high"
            } else if affected > 0 || !symbols.is_empty() {
                "medium"
            } else {
                "low"
            };
            let mut caller_files = caller_files.into_iter().collect::<Vec<_>>();
            caller_files.sort();
            let counts = json!({"new_hunks":hunks.len(),"old_hunks":old_hunks.len(),"symbols":symbols.len(),"importers":importers.len(),"caller_candidates":candidate_count,"transitive_caller_files":caller_files.len()});
            let nested_truncated = [
                hunks.len(),
                old_hunks.len(),
                symbols.len(),
                importers.len(),
                candidate_count,
                caller_files.len(),
            ]
            .into_iter()
            .any(|n| n > cap);
            hunks.truncate(cap);
            old_hunks.truncate(cap);
            caller_files.truncate(cap);
            importers.truncate(cap);
            calls.truncate(cap);
            results.push(json!({"path":path,"old_path":old_path,"status":status,"binary":binary,"indexed":self.files.contains_key(path),"new_hunks":hunks,"old_hunks":old_hunks,"symbols":symbols.iter().take(cap).map(query::compact_symbol).collect::<Vec<_>>(),"importers":importers,"caller_candidates":calls,"transitive_caller_files":caller_files,"risk":risk,"affected_count":affected,"counts":counts,"truncated":nested_truncated}));
        }
        let truncated = changed.len() > cap || results.iter().any(|f| f["truncated"] == true);
        let mut result = json!({"diff":if base.is_none() {"working tree/index/untracked vs HEAD"} else {"base..target"},"baseline":baseline,"target":target_id,"files":results,"total_files":changed.len(),"truncated":truncated,"heuristic":true,"note":"Name-based candidates and current/baseline definitions are hints. Absence of results is not proof; inspect git diff and source. counts are before truncation."});
        // Per-list limits alone still multiply. Also bound the complete serialized response.
        let budget = (cap * 1200).clamp(2400, 24000);
        result["output_bytes_limit"] = json!(budget);
        let mut size = result.to_string().len();
        while size > budget {
            let files = result["files"].as_array_mut().unwrap();
            let mut shrunk = false;
            for file in files.iter_mut() {
                let mut clipped = false;
                for key in [
                    "new_hunks",
                    "old_hunks",
                    "symbols",
                    "importers",
                    "caller_candidates",
                    "transitive_caller_files",
                ] {
                    let rows = file[key].as_array_mut().unwrap();
                    let keep = rows.len().saturating_mul(budget * 3 / 4) / size;
                    if keep < rows.len() {
                        rows.truncate(keep);
                        clipped = true;
                    }
                }
                if clipped {
                    file["truncated"] = json!(true);
                    shrunk = true;
                }
            }
            if !shrunk {
                if files.is_empty() {
                    break;
                }
                let keep = (files.len().saturating_mul(budget * 3 / 4) / size).min(files.len() - 1);
                files.truncate(keep);
            }
            result["truncated"] = json!(true);
            size = result.to_string().len();
        }
        Ok(result)
    }
}
fn overlaps(symbol: &lang::Symbol, hunks: &[(i64, i64)]) -> bool {
    hunks.iter().any(|(start, count)| {
        symbol.line <= start + count.saturating_sub(1) && symbol.end_line >= *start
    })
}
fn hunk_range(field: &str) -> Option<(i64, i64)> {
    let field = field.get(1..)?;
    let (start, count) = field.split_once(',').unwrap_or((field, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}
fn test_path(path: &str) -> bool {
    path.split('/')
        .any(|p| matches!(p, "tests" | "test" | "__tests__"))
        || path.contains("_test.")
        || path.contains(".test.")
        || path.contains(".spec.")
}
fn git(root: &Path, args: &[String], cancel: &CancellationToken) -> Result<Vec<u8>> {
    // Tempfiles bound memory and avoid pipe deadlocks. No shell or external diff drivers.
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut command = Command::new("git");
    command
        .args([
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .process_group(0);
    let mut child = command.spawn()?;
    let started = Instant::now();
    let status = loop {
        if cancel.is_cancelled()
            || started.elapsed() > Duration::from_secs(15)
            || stdout.metadata()?.len() > 4 * 1024 * 1024
            || stderr.metadata()?.len() > 1024 * 1024
        {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            bail!("git cancelled or output/time limit reached");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    if !status.success() {
        let mut error = String::new();
        stderr.take(4096).read_to_string(&mut error)?;
        bail!("git: {}", error.trim());
    }
    let mut data = Vec::new();
    stdout.take(4 * 1024 * 1024 + 1).read_to_end(&mut data)?;
    if data.len() > 4 * 1024 * 1024 {
        bail!("git output limit reached");
    }
    Ok(data)
}
