use regex::Regex;
use std::borrow::Cow;
pub fn generic_filter(body: &str) -> String {
    let stripped = strip_ansi(body);
    let mut out_lines: Vec<String> = Vec::new();

    let mut prev: Option<String> = None;
    let mut prev_count: usize = 0;
    for raw_line in stripped.lines() {
        let line = collapse_carriage_returns(raw_line);
        if is_spinner_or_progress(&line) {
            continue;
        }
        match &prev {
            Some(p) if p == &line => {
                prev_count += 1;
            }
            _ => {
                if let Some(p) = prev.take() {
                    push_with_count(&mut out_lines, p, prev_count);
                }
                prev = Some(line);
                prev_count = 1;
            }
        }
    }
    if let Some(p) = prev.take() {
        push_with_count(&mut out_lines, p, prev_count);
    }

    middle_truncate_lines(out_lines)
}

fn push_with_count(out: &mut Vec<String>, line: String, count: usize) {
    if count > 1 {
        out.push(format!("{line}  [×{count}]"));
    } else {
        out.push(line);
    }
}

pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    use std::sync::OnceLock;
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    let re = ANSI_RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap()
    });
    re.replace_all(text, "")
}

fn collapse_carriage_returns(line: &str) -> String {
    if !line.contains('\r') {
        return line.to_string();
    }
    line.split('\r')
        .rfind(|seg| !seg.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

fn is_spinner_or_progress(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false; // blank lines are handled by dedup/truncation, not here
    }
    trimmed.chars().all(|c| {
        c.is_whitespace()
            || ('\u{2800}'..='\u{28FF}').contains(&c)
            || matches!(
                c,
                '|' | '/'
                    | '\\'
                    | '-'
                    | '.'
                    | '·'
                    | '•'
                    | '█'
                    | '▉'
                    | '▊'
                    | '▋'
                    | '▌'
                    | '▍'
                    | '▎'
                    | '▏'
                    | '░'
                    | '▒'
                    | '▓'
                    | '◐'
                    | '◓'
                    | '◑'
                    | '◒'
                    | '○'
                    | '●'
            )
    })
}

fn middle_truncate_lines(lines: Vec<String>) -> String {
    if lines.len() <= 400 {
        return lines.join("\n");
    }
    format!(
        "{}\n… {} lines elided …\n{}",
        lines[..240].join("\n"),
        lines.len() - 400,
        lines[lines.len() - 160..].join("\n")
    )
}
pub enum Family {
    Git,
    Js,
    Go,
    Python,
    Rust,
    Ruby,
    Jvm,
    Dotnet,
    Cloud,
    System,
}

pub fn recognize(command: &str) -> Option<Family> {
    let prog = first_program(command)?;
    Some(match prog.as_str() {
        "git" | "gh" | "glab" | "gt" => Family::Git,
        "npm" | "pnpm" | "npx" | "playwright" | "vitest" | "prettier" | "tsc" | "prisma"
        | "next" => Family::Js,
        "go" | "golangci-lint" => Family::Go,
        "pytest" | "ruff" | "pip" | "pip3" | "mypy" => Family::Python,
        "cargo" => Family::Rust,
        "rspec" | "rubocop" | "rake" => Family::Ruby,
        "gradlew" | "./gradlew" => Family::Jvm,
        "dotnet" => Family::Dotnet,
        "aws" | "curl" | "wget" | "psql" | "docker" => Family::Cloud,
        "find" | "grep" | "ls" | "tree" | "wc" | "env" | "cat" | "head" | "tail" => Family::System,
        _ => return None,
    })
}

pub fn first_program(command: &str) -> Option<String> {
    for tok in command.split_whitespace() {
        if tok.contains('=')
            && !tok.starts_with('=')
            && tok.split('=').next().is_some_and(is_envish)
        {
            continue;
        }
        if tok == "./gradlew" {
            return Some(tok.to_string());
        }
        let base = tok.rsplit('/').next().unwrap_or(tok);
        return Some(base.to_string());
    }
    None
}

fn is_envish(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn command_strategy(family: Family, body: &str) -> String {
    match family {
        Family::Rust => drop_noise(body, is_rust_noise),
        Family::Git => drop_noise(body, is_git_noise),
        Family::Js => drop_noise(body, is_js_noise),
        Family::Go => drop_noise(body, is_go_noise),
        Family::Python => drop_noise(body, is_python_noise),
        Family::Ruby => drop_noise(body, is_ruby_noise),
        Family::Jvm => drop_noise(body, is_jvm_noise),
        Family::Dotnet => drop_noise(body, is_dotnet_noise),
        Family::Cloud => drop_noise(body, is_cloud_noise),
        Family::System => drop_noise(body, is_system_noise),
    }
}

fn drop_noise(body: &str, noise: fn(&str) -> bool) -> String {
    let mut removed = 0;
    let kept: Vec<_> = body
        .lines()
        .filter(|line| {
            if !looks_like_signal(line) && noise(line) {
                removed += 1;
                false
            } else {
                true
            }
        })
        .collect();
    let mut out = kept.join("\n");
    if removed > 0 {
        out.push_str(&format!("\n[{} noise lines omitted]", removed));
    }
    out
}
pub fn looks_like_signal(line: &str) -> bool {
    use std::sync::OnceLock;
    static SIGNAL_RE: OnceLock<Regex> = OnceLock::new();
    let re = SIGNAL_RE.get_or_init(|| {
        Regex::new(
            r#"(?ix)
              \b error \b
            | error \[                  # rustc error[E0382]
            | error :                   # error:
            | \b warn(ing)? \b
            | \b fail(ed|ure|ures|ing)? \b
            | \b panic(ked|s)? \b
            | \b exception \b
            | \b traceback \b
            | \b assert(ion)? \b
            | \b fatal \b
            | \b denied \b
            | \b cannot \b
            | \b unable \s to \b
            | \b not \s found \b
            | \b undefined \b
            | \b unexpected \b
            | ^ \s* -->\s               # rustc/clippy source pointer
            | ^ \s* at\s .* :\d+        # JS/Java stack frame
            | ^ \s* File\s \x22 .* \x22 ,\s line\s \d+   # Python traceback frame
            | \.go:\d+:                 # Go file:line diagnostic
            | ^ \s* FAIL \b
            | ^ \s* FAILED \b
            | ^ \s* E \s                # pytest assertion-detail prefix `E   `
            | ^ \s* ✗
            | ^ \s* ✕
            | ^ \s* ×
            "#,
        )
        .unwrap()
    });
    re.is_match(line)
}
fn is_rust_noise(line: &str) -> bool {
    let t = line.trim_start();
    matches!(
        first_word(t),
        Some(
            "Compiling"
                | "Checking"
                | "Downloading"
                | "Downloaded"
                | "Fresh"
                | "Finished"
                | "Updating"
                | "Blocking"
                | "Installing"
                | "Locking"
                | "Adding"
                | "Removing"
        )
    )
}

fn is_git_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("  (use \"git")          // `(use "git restore ..." to ...)` advice
        || t.starts_with("(use \"git")
        || t == "nothing to commit, working tree clean"
        || t.starts_with("Receiving objects:")
        || t.starts_with("Resolving deltas:")
        || t.starts_with("remote: Counting objects:")
        || t.starts_with("remote: Compressing objects:")
        || t.starts_with("Unpacking objects:")
        || t.starts_with("Enumerating objects:")
}

fn is_js_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("npm notice")
        || t.starts_with("npm WARN deprecated") // deprecation noise; real warns kept by signal
        || t.starts_with("> ")                  // pnpm/npm lifecycle echo `> pkg@1 build`
        || t.starts_with("Progress:")
        || t.starts_with("Packages:")
        || t.starts_with("Downloading ")
        || t.starts_with("added ")
        || t.starts_with("Lockfile is up to date")
        || t.starts_with("Already up to date")
        || t.starts_with("up to date")
        || t == "Done in"
        || t.starts_with("Done in ")
}

fn is_go_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("go: downloading")
        || t.starts_with("go: finding")
        || t.starts_with("go: extracting")
        || t.starts_with("ok  \t")    // `ok  \tpkg\t0.1s` passing package
        || t.starts_with("?   \t")    // `?   \tpkg\t[no test files]`
        || t == "PASS"
        || t.starts_with("=== RUN")
        || t.starts_with("=== PAUSE")
        || t.starts_with("=== CONT")
        || t.starts_with("--- PASS")
}

fn is_python_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("platform ")
        || t.starts_with("rootdir:")
        || t.starts_with("plugins:")
        || t.starts_with("cachedir:")
        || t.starts_with("collecting ")
        || t.starts_with("Requirement already satisfied")
        || t.starts_with("Collecting ")
        || t.starts_with("Downloading ")
        || t.starts_with("Using cached ")
        || t.starts_with("Installing collected packages")
        || (!t.is_empty()
            && t.chars()
                .all(|c| matches!(c, '.' | '%' | '[' | ']' | ' ') || c.is_ascii_digit()))
}

fn is_ruby_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Using ")            // `bundle install` "Using rake 13.0"
        || t.starts_with("Fetching ")
        || t.starts_with("Run options:")
        || t.starts_with("Randomized with seed")
        || (!t.is_empty() && t.chars().all(|c| matches!(c, '.' | 'F' | '*' | 'E' | ' ')))
            && t.contains('.')
}

fn is_jvm_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("> Task :")
        || t.starts_with("> Configure project")
        || t.starts_with("Starting a Gradle Daemon")
        || t.starts_with("Welcome to Gradle")
        || t.starts_with("Daemon will be stopped")
        || t == "BUILD SUCCESSFUL"
        || t.starts_with("BUILD SUCCESSFUL in")
        || t.starts_with("Deprecated Gradle features were used")
        || t.contains("actionable task") // `5 actionable tasks: 5 executed`
}

fn is_dotnet_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Determining projects to restore")
        || t.starts_with("Restored ")
        || t.starts_with("Restore complete")
        || t.starts_with("Nothing to do. None of the projects")
        || t.starts_with("Welcome to .NET")
        || t.starts_with("Telemetry")
        || t.starts_with("----------")
        || t.starts_with("Build succeeded")
}

fn is_cloud_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.ends_with(": Pulling fs layer")
        || t.ends_with(": Waiting")
        || t.ends_with(": Verifying Checksum")
        || t.ends_with(": Download complete")
        || t.ends_with(": Pull complete")
        || t.ends_with(": Already exists")
        || t.contains(": Downloading ")
        || t.contains(": Extracting ")
        || t == "SET"
        || t.starts_with("Time: ")
        || t.starts_with("Saving to:")
        || t.starts_with("Length:")
        || t.starts_with("--") && t.contains("--  http")
}

fn is_system_noise(_line: &str) -> bool {
    false
}

fn first_word(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

pub fn compress_stream(command: &str, body: &str) -> String {
    let generic = generic_filter(body);
    match recognize(command) {
        Some(family) => command_strategy(family, &generic),
        None => generic,
    }
}
