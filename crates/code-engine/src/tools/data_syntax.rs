//! Advisory parse checks only when LSP diagnostics did not cover the write.
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
        _ => yaml_error(text).map(|(line, column, message)| ("YAML", line, column, message)),
    };
    error.map_or_else(String::new, |(format, line, column, message)| {
        let message = message.lines().next().unwrap_or("invalid syntax").chars().take(400).collect::<String>();
        format!("\n[{format} syntax error at line {line}, column {column}: {message}. The file was written; correct its syntax.]")
    })
}

// serde-saphyr's recursive Serde visitor needs more than a worker thread's
// default stack at depth 128, even though the parser enforces that depth cap.
// A bounded dedicated stack keeps valid deeply nested YAML safe in every caller.
fn yaml_error(text: &str) -> Option<(usize, usize, String)> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("yaml-syntax".into())
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                serde_saphyr::from_multiple_with_options::<serde::de::IgnoredAny>(
                    text,
                    // Syntax checks accept YAML-only values, including non-finite floats and
                    // composite keys; there is no JSON conversion at this call site.
                    serde_saphyr::options! {
                        ignore_binary_tag_for_string: true,
                        // This is a syntax check over an already size-bounded string.
                        // Keep stack protection without imposing document-size budgets.
                        budget: serde_saphyr::budget! {
                            max_depth: 128,
                            flow_nesting_limit: 128,
                            max_nodes: usize::MAX,
                            max_events: usize::MAX,
                            max_reader_input_bytes: None,
                            max_total_scalar_bytes: usize::MAX,
                            max_documents: usize::MAX,
                            max_aliases: usize::MAX,
                            max_anchors: usize::MAX,
                            max_recorded_anchor_events: usize::MAX,
                            max_recorded_anchor_bytes: usize::MAX,
                            enforce_alias_anchor_ratio: false,
                        },
                        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
                        non_finite_float_policy: serde_saphyr::NonFiniteFloatPolicy::PassThrough,
                    },
                )
                .err()
                .map(|e| {
                    let (line, column) = e
                        .location()
                        .map_or((1, 1), |l| (l.line() as usize, l.column() as usize));
                    (line, column, e.to_string())
                })
            });
        match worker {
            Ok(worker) => worker
                .join()
                .unwrap_or_else(|_| Some((1, 1, "YAML syntax checker panicked".into()))),
            Err(error) => Some((
                1,
                1,
                format!("could not start YAML syntax checker: {error}"),
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::check;
    use std::path::Path;

    #[test]
    fn yaml_accepts_scalar_collection_and_stream_shapes() {
        for input in [
            "",
            "# empty\n",
            "---\n",
            "null",
            "[true, false, null, 42, 1.25, 'quoted: text']",
            "root:\n  list: [one, two]\n  map: {enabled: true, missing: null}\n  text: |\n    first\n    second\n",
            "default: &value {name: example}\ncopy: *value\n",
            "? [one, two]\n: value\n",
            "value: !custom tagged\n",
            "value: !!binary SGVsbG8=\n",
            "[.inf, -.inf, .nan]",
            "---\nfirst: 1\n---\n- second\n- third\n---\nnull\n",
        ] {
            assert_eq!(check(Path::new("file.yaml"), input), "", "{input:?}");
        }
    }

    #[test]
    fn yaml_reports_invalid_later_documents_aliases_and_duplicate_keys() {
        for input in [
            "key: [",
            "first: ok\n---\nkey: [",
            "key: *missing",
            "key: 1\nkey: 2\n",
        ] {
            let message = check(Path::new("file.yml"), input);
            assert!(message.contains("YAML syntax error at line"), "{message:?}");
            assert!(message.contains("The file was written"));
        }
    }

    #[test]
    fn yaml_syntax_accepts_large_documents_and_streams() {
        // More than both the default 250k nodes and 1M events, below 8 MiB.
        let input = "- value\n".repeat(1_000_001);
        assert_eq!(check(Path::new("large.yaml"), &input), "");
        let stream = "---\nvalue\n".repeat(1_025);
        assert_eq!(check(Path::new("stream.yaml"), &stream), "");
    }

    #[test]
    fn yaml_syntax_keeps_a_depth_limit_of_128() {
        let at_limit = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert_eq!(check(Path::new("depth.yaml"), &at_limit), "");
        let mapping = (0..128)
            .map(|n| format!("{}key:\n", "  ".repeat(n)))
            .collect::<String>()
            + &format!("{}value\n", "  ".repeat(128));
        assert_eq!(check(Path::new("depth.yaml"), &mapping), "");
        for input in [
            format!("{}0{}", "[".repeat(129), "]".repeat(129)),
            (0..129)
                .map(|n| format!("{}-\n", "  ".repeat(n)))
                .collect::<String>(),
        ] {
            let message = check(Path::new("deep.yaml"), &input);
            assert!(message.contains("YAML syntax error"), "{message}");
        }
    }

    #[test]
    fn toml_parses_documents_and_reports_invalid_values() {
        assert_eq!(
            check(
                Path::new("file.toml"),
                "[table]\nname = 'quoted'\nvalues = [1, 2]\nflag = true\n"
            ),
            ""
        );
        assert_eq!(check(Path::new("file.toml"), ""), "");
        let error = check(Path::new("file.toml"), "name = 'é'\nvalue = [\n");
        assert!(error.contains("TOML syntax error at line"), "{error}");
        assert!(error.contains("column"));
    }
}
