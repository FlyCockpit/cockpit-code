//! Advisory parse checks only when LSP diagnostics did not cover the write.
use serde::Deserialize;
use std::path::Path;

pub(super) fn check(path: &Path, text: &str) -> String {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "json" | "toml" | "yaml" | "yml") {
        return String::new();
    }
    if text.len() > 8 * 1024 * 1024 {
        return "\n[Syntax check skipped: file exceeds 8 MiB.]".into();
    }
    let error = match extension.as_str() {
        "json" => serde_json::from_str::<serde_json::Value>(text)
            .err()
            .map(|e| ("JSON", e.line(), e.column(), e.to_string())),
        "toml" => toml::from_str::<toml::Value>(text).err().map(|e| {
            let offset = e.span().map_or(0, |s| s.start).min(text.len());
            let line = text[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
            let column = text[..offset]
                .rsplit('\n')
                .next()
                .unwrap_or("")
                .chars()
                .count()
                + 1;
            ("TOML", line, column, e.message().to_owned())
        }),
        _ => serde_yaml_ng::Deserializer::from_str(text).find_map(|document| {
            serde_yaml_ng::Value::deserialize(document).err().map(|e| {
                let (line, column) = e.location().map_or((1, 1), |l| (l.line(), l.column()));
                ("YAML", line, column, e.to_string())
            })
        }),
    };
    error.map_or_else(String::new, |(format, line, column, message)| {
        let message = message.lines().next().unwrap_or("invalid syntax").chars().take(400).collect::<String>();
        format!("\n[{format} syntax error at line {line}, column {column}: {message}. The file was written; correct its syntax.]")
    })
}
