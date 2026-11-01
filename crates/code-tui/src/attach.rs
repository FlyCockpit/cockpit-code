//! Attachments (D79): kinds, chip labels, `@path` and pasted-path
//! resolution, the clipboard image, and opening a file in the system
//! viewer. Chips travel as `<attachment>` tags (`paste.rs`).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The input type an attachment needs: image, pdf, audio, video, text (a
/// UTF-8 file, inlined as text for every model) or file (any other binary,
/// needing a model that declares `file` input).
pub fn kind_of(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" => "image",
        "pdf" => "pdf",
        "mp3" | "wav" | "m4a" | "ogg" | "flac" | "aac" | "opus" => "audio",
        "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v" => "video",
        _ if is_text(path) => "text",
        _ => "file",
    }
}

/// UTF-8 without NUL bytes in the first 8 KiB (a cut multibyte character
/// at the end is allowed).
fn is_text(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = vec![0u8; 8192];
    let Ok(n) = std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) else {
        return false;
    };
    let buf = &buf[..n];
    if buf.contains(&0) {
        return false;
    }
    match std::str::from_utf8(buf) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && n == 8192,
    }
}

/// `[image #1 · shot.png]`
pub fn label(n: u32, kind: &str, path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .map_or(path.into(), |n| n.to_string_lossy());
    format!("[{kind} #{n} · {name}]")
}

/// The `@path` form (spaces escaped), for copies and `$EDITOR`.
pub fn mention(path: &str) -> String {
    format!("@{}", path.replace(' ', "\\ "))
}

/// Whether a model with input types `inputs` (empty: text only) takes `kind`.
/// Text files go to every model; other binaries need `file` input.
pub fn supported(kind: &str, inputs: &[String]) -> bool {
    kind == "text" || inputs.iter().any(|i| i == kind)
}

/// A typed or pasted path as an existing file: `~`, `file://`, quotes and
/// shell backslash escapes are understood; relative paths are under `cwd`.
pub fn resolve(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let mut s = raw.trim().to_string();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s = s[1..s.len() - 1].to_string();
    } else {
        // Terminal drag-and-drop escapes spaces and shell characters.
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        s = out;
    }
    if let Some(rest) = s.strip_prefix("file://") {
        s = percent_decode(rest);
    }
    if s.is_empty() {
        return None;
    }
    let path = if let Some(rest) = s.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        let p = PathBuf::from(&s);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let path = std::fs::canonicalize(&path).ok()?;
    path.is_file().then_some(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A paste that is only file paths (one per line, or a drag-and-drop list
/// separated by unescaped spaces) to existing images: those paths.
pub fn pasted_images(text: &str, cwd: &Path) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > 8192 {
        return None;
    }
    let mut out = vec![];
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let one = resolve(line, cwd);
        let parts = match one {
            Some(p) => vec![p],
            None => split_unescaped(line)
                .iter()
                .map(|w| resolve(w, cwd))
                .collect::<Option<Vec<_>>>()?,
        };
        out.extend(parts);
    }
    (!out.is_empty() && out.iter().all(|p| kind_of(p) == "image")).then_some(out)
}

/// Split on spaces that are not backslash-escaped or quoted.
fn split_unescaped(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            c if Some(c) == quote => {
                quote = None;
                cur.push(c);
            }
            ' ' if quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The clipboard's image as a PNG file in `dir`.
pub fn clipboard_image(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = format!(
        "clipboard-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
    );
    let path = dir.join(name);
    let ok = if cfg!(target_os = "macos") {
        let script = format!(
            "set f to open for access POSIX file \"{}\" with write permission\n\
             try\n write (the clipboard as «class PNGf») to f\n close access f\n\
             on error\n close access f\n error \"no image\"\n end try",
            path.display()
        );
        Command::new("osascript")
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    } else {
        let out = Command::new("wl-paste")
            .args(["--type", "image/png"])
            .output()
            .ok()
            .filter(|o| o.status.success() && !o.stdout.is_empty())
            .or_else(|| {
                Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", "image/png", "-o"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success() && !o.stdout.is_empty())
            });
        out.is_some_and(|o| std::fs::write(&path, o.stdout).is_ok())
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if ok && size > 0 {
        Ok(path)
    } else {
        let _ = std::fs::remove_file(&path);
        Err("no image on the clipboard".into())
    }
}

/// Open `path` with the system viewer, detached.
pub fn open(path: &Path) -> Result<(), String> {
    if !path.exists() {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        return Err(format!("{name} no longer exists"));
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{opener}: {e}"))
}

/// `@path` mentions in `text` outside `skip` ranges whose path is an
/// existing file: (start, end, path). With `complete_only`, a mention must
/// be followed by whitespace (the user finished typing it).
pub fn mentions(
    text: &str,
    cwd: &Path,
    skip: &[(usize, usize)],
    complete_only: bool,
) -> Vec<(usize, usize, PathBuf)> {
    let mut out = vec![];
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let at_word_start = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if bytes[i] != b'@' || !at_word_start || skip.iter().any(|(a, b)| *a <= i && i < *b) {
            i += 1;
            continue;
        }
        // The token: up to unescaped whitespace.
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
                continue;
            }
            if bytes[j].is_ascii_whitespace() {
                break;
            }
            j += 1;
        }
        let complete = j < bytes.len();
        if j > i + 1
            && (complete || !complete_only)
            && text.is_char_boundary(j)
            && let Some(path) = resolve(&text[i + 1..j], cwd)
        {
            out.push((i, j, path));
        }
        i = j.max(i + 1);
    }
    out
}
