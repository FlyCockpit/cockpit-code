//! Bounded read-time text, separate from stale-write revision hashes.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
const TOTAL: usize = 4 * 1024 * 1024;
const FILE: usize = 256 * 1024;
const ENTRIES: usize = 256;
const DIFF: usize = 2000;
#[derive(Clone, Default)]
pub(crate) struct ReadMemory {
    texts: BTreeMap<PathBuf, (u64, String)>,
    clock: u64,
    bytes: usize,
}
impl ReadMemory {
    pub fn remember(&mut self, path: &Path, bytes: &[u8]) {
        if let Some((_, old)) = self.texts.remove(path) {
            self.bytes -= old.len();
        }
        if bytes.len() > FILE {
            return;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        while self.bytes + bytes.len() > TOTAL || self.texts.len() >= ENTRIES {
            let Some(path) = self
                .texts
                .iter()
                .min_by_key(|(_, (time, _))| time)
                .map(|(p, _)| p.clone())
            else {
                break;
            };
            self.bytes -= self.texts.remove(&path).unwrap().1.len();
        }
        self.clock += 1;
        self.bytes += bytes.len();
        self.texts.insert(path.into(), (self.clock, text.into()));
    }
    pub fn paths(&self) -> Vec<PathBuf> {
        self.texts.keys().cloned().collect()
    }
    pub fn notice(&self, path: &Path) -> String {
        let fallback = || {
            format!(
                "{} changed; re-read it (diff unavailable or larger than 2,000 characters).",
                path.display()
            )
        };
        let Some((_, old)) = self.texts.get(path) else {
            return fallback();
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return fallback();
        };
        if meta.len() > FILE as u64 {
            return fallback();
        }
        let Ok(new) = std::fs::read_to_string(path) else {
            return fallback();
        };
        if old == &new {
            return String::new();
        }
        let diff = similar::TextDiff::configure()
            .algorithm(similar::Algorithm::RawMyers)
            .diff_lines(old, &new)
            .unified_diff()
            .context_radius(2)
            .to_string();
        if diff.chars().count() > DIFF {
            fallback()
        } else {
            format!("{} changed:\n{diff}", path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReadMemory;

    #[test]
    fn moved_blocks_keep_the_shortest_diff_and_original_tie_breaking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let mut memory = ReadMemory::default();
        memory.remember(&path, b"a\nb\nc\nd\n");
        std::fs::write(&path, "c\nd\na\nb\n").unwrap();
        assert_eq!(
            memory.notice(&path),
            format!(
                "{} changed:\n@@ -1,4 +1,4 @@\n+c\n+d\n a\n b\n-c\n-d\n",
                path.display()
            )
        );
    }

    #[test]
    fn changed_file_notice_keeps_expected_myers_unified_diff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let mut memory = ReadMemory::default();
        memory.remember(&path, b"one\ntwo\nthree\n");
        std::fs::write(&path, "one\nchanged\nthree\n").unwrap();
        assert_eq!(
            memory.notice(&path),
            format!(
                "{} changed:\n@@ -1,3 +1,3 @@\n one\n-two\n+changed\n three\n",
                path.display()
            )
        );
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(memory.notice(&path), "");
    }
}
