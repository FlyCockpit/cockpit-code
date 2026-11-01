//! Trusted, bounded guidance source data; selection is a separate private fork.
use crate::{Control, Session, schema::EngineError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GuidanceMetadata {
    #[serde(deserialize_with = "strings")]
    pub when: Vec<String>,
    #[serde(deserialize_with = "strings")]
    pub languages: Vec<String>,
    #[serde(deserialize_with = "strings")]
    pub globs: Vec<String>,
}
fn strings<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Values {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Values::deserialize(d)? {
        Values::One(v) => vec![v],
        Values::Many(v) => v,
    })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceItem {
    pub id: String,
    pub title: String,
    pub text: String,
    pub file: PathBuf,
    pub metadata: GuidanceMetadata,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceLibrary {
    pub items: BTreeMap<String, GuidanceItem>,
    pub warnings: Vec<String>,
}
impl GuidanceLibrary {
    pub fn search(
        &self,
        query: &str,
        language: Option<&str>,
        path: Option<&Path>,
    ) -> Vec<GuidanceItem> {
        let query = query.to_lowercase();
        self.items
            .values()
            .filter(|item| {
                (query.is_empty()
                    || format!(
                        "{} {} {} {} {}",
                        item.id,
                        item.title,
                        item.text,
                        item.metadata.when.join(" "),
                        item.metadata.languages.join(" ")
                    )
                    .to_lowercase()
                    .split_whitespace()
                    .any(|word| query.split_whitespace().any(|q| word.contains(q))))
                    && language.is_none_or(|lang| {
                        item.metadata.languages.is_empty()
                            || item
                                .metadata
                                .languages
                                .iter()
                                .any(|l| l.eq_ignore_ascii_case(lang))
                    })
                    && path.is_none_or(|path| {
                        item.metadata.globs.is_empty()
                            || item.metadata.globs.iter().any(|g| {
                                globset::Glob::new(g)
                                    .is_ok_and(|g| g.compile_matcher().is_match(path))
                            })
                    })
            })
            .take(64)
            .cloned()
            .collect()
    }
    fn add(&mut self, path: &Path, content: &str) {
        match parse(path, content) {
            Ok(items) => {
                for item in items {
                    if self.items.len() >= 512 {
                        self.warnings.push(
                            "Guidance library exceeds 512 items; extra items omitted.".into(),
                        );
                        break;
                    }
                    if self.items.contains_key(&item.id) {
                        self.warnings.push(format!(
                            "Duplicate guidance id {} in {}; keeping the first item.",
                            item.id,
                            path.display()
                        ));
                    } else {
                        self.items.insert(item.id.clone(), item);
                    }
                }
            }
            Err(error) => self.warnings.push(error.message),
        }
    }
}
fn parse(path: &Path, content: &str) -> Result<Vec<GuidanceItem>, EngineError> {
    let content = content.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (metadata, body) = if let Some(rest) = content.strip_prefix("---\n") {
        let end = rest.find("\n---").ok_or_else(|| {
            EngineError::bad(format!(
                "{}: guidance frontmatter has no closing delimiter",
                path.display()
            ))
        })?;
        let metadata: GuidanceMetadata = serde_yaml_ng::from_str(&rest[..end]).map_err(|_| {
            EngineError::bad(format!("{}: invalid guidance frontmatter", path.display()))
        })?;
        (metadata, &rest[end + 4..])
    } else {
        (GuidanceMetadata::default(), content.as_str())
    };
    if metadata.when.len() > 32 || metadata.languages.len() > 32 || metadata.globs.len() > 32 {
        return Err(EngineError::bad(format!(
            "{}: guidance metadata lists are capped at 32",
            path.display()
        )));
    }
    if metadata
        .when
        .iter()
        .chain(&metadata.languages)
        .chain(&metadata.globs)
        .any(|s| s.chars().count() > 256)
    {
        return Err(EngineError::bad(format!(
            "{}: guidance metadata values are capped at 256 characters",
            path.display()
        )));
    }
    for glob in &metadata.globs {
        globset::Glob::new(glob)
            .map_err(|_| EngineError::bad(format!("{}: invalid guidance glob", path.display())))?;
    }
    static ITEMS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = ITEMS.get_or_init(|| {
        regex::Regex::new(r"(?m)^- \*\*([A-Za-z][A-Za-z0-9_.:-]{0,63})[ \t]+([^*\r\n]+)\*\*[ \t]*")
            .unwrap()
    });
    let matches = re.captures_iter(body).collect::<Vec<_>>();
    let mut items = Vec::new();
    for (i, m) in matches.iter().enumerate() {
        let full = m.get(0).unwrap();
        let end = matches.get(i + 1).unwrap_or(m).get(0).unwrap().start();
        let end = if i + 1 == matches.len() {
            body.len()
        } else {
            end
        };
        let text = body[full.end()..end].trim();
        if m[2].chars().count() > 256 || text.is_empty() || text.chars().count() > 2048 {
            return Err(EngineError::bad(format!(
                "{}: guidance item {} must have 1–2048 characters",
                path.display(),
                &m[1]
            )));
        }
        items.push(GuidanceItem {
            id: m[1].into(),
            title: m[2].trim().trim_end_matches('.').into(),
            text: text.into(),
            file: path.into(),
            metadata: metadata.clone(),
        });
    }
    if items.is_empty() {
        return Err(EngineError::bad(format!(
            "{}: no id'd guidance items (- **ID Title.** text)",
            path.display()
        )));
    }
    Ok(items)
}
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|s| s == "md") && p.is_file())
        .take(65)
        .collect::<Vec<_>>();
    paths.sort();
    paths
}
fn read(path: &Path) -> Result<String, EngineError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(128 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|e| EngineError::bad(format!("{}: {e}", path.display())))?;
    if bytes.len() > 128 * 1024 {
        return Err(EngineError::bad(format!(
            "{}: guidance files are capped at 128 KiB",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| EngineError::bad(format!("{}: guidance must be UTF-8", path.display())))
}
impl Control {
    pub(crate) async fn load_guidance_library(
        &self,
        config: &Path,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<GuidanceLibrary, EngineError> {
        let mut library = GuidanceLibrary::default();
        let mut total = 0usize;
        for (dir, project) in [
            (config.join("guidance"), false),
            (cwd.join(".cockpit-code/guidance"), true),
        ] {
            let paths = files(&dir);
            if paths.len() > 64 {
                library.warnings.push(format!(
                    "{}: more than 64 guidance files; extra files omitted",
                    dir.display()
                ));
            }
            for path in paths.into_iter().take(64) {
                if cancel.is_cancelled() {
                    return Err(EngineError::cancelled());
                }
                let content = if project {
                    self.project_library_guidance(&path, cancel).await?
                } else {
                    match read(&path) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            library.warnings.push(e.message);
                            None
                        }
                    }
                };
                if let Some(content) = content {
                    total = total.saturating_add(content.len());
                    if total > 2 * 1024 * 1024 {
                        library
                            .warnings
                            .push("Guidance source exceeds 2 MiB; remaining files omitted.".into());
                        return Ok(library);
                    }
                    library.add(&path, &content);
                }
            }
        }
        Ok(library)
    }
}
impl Session {
    /// Untrusted project files are skipped and request trust asynchronously.
    pub async fn guidance_library(&self) -> Result<GuidanceLibrary, EngineError> {
        self.control
            .load_guidance_library(
                &self.directories.config,
                &self.cwd(),
                &CancellationToken::new(),
            )
            .await
    }
}
pub fn starter_files() -> [(&'static str, &'static str); 4] {
    [
        (
            "correctness.md",
            include_str!("builtin/guidance/correctness.md"),
        ),
        ("rust.md", include_str!("builtin/guidance/rust.md")),
        (
            "interfaces.md",
            include_str!("builtin/guidance/interfaces.md"),
        ),
        ("data.md", include_str!("builtin/guidance/data.md")),
    ]
}
