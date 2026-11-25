//! Pure string helpers for relative forward-slash paths inside the indexed
//! tree, shared by the per-language import resolvers.

/// Normalize a path made of `a/b/../c` components to a clean relative
/// forward-slash path, without touching the filesystem.
pub(super) fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(&"..") | None) {
                    parts.push("..");
                } else {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

pub(super) fn parent_dir(importer: &str) -> &str {
    match importer.rfind('/') {
        Some(i) => &importer[..i],
        None => "",
    }
}

pub(super) fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        normalize(rel)
    } else {
        normalize(&format!("{dir}/{rel}"))
    }
}
