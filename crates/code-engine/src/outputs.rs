//! Durable tool artifacts. These files belong to the session, not an agent's scope.
use crate::{Control, schema::EngineError};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

impl Control {
    pub(crate) fn retain_text(&self, call_id: &str, text: &str) -> Result<String, EngineError> {
        if text.len() <= 8192 {
            return Ok(text.into());
        }
        let mut source = tempfile::tempfile().map_err(|e| EngineError::bad(e.to_string()))?;
        source
            .write_all(text.as_bytes())
            .map_err(|e| EngineError::bad(e.to_string()))?;
        let path = self.save_output(call_id, &mut source)?;
        preview(&path).map_err(|e| EngineError::bad(e.to_string()))
    }

    pub(crate) fn outputs_dir(&self) -> PathBuf {
        self.journal.lock().unwrap().dir.join("outputs")
    }
    pub(crate) fn save_output(
        &self,
        call_id: &str,
        source: &mut File,
    ) -> Result<PathBuf, EngineError> {
        let mut save = || -> std::io::Result<PathBuf> {
            let dir = self.outputs_dir();
            std::fs::create_dir_all(&dir)?;
            // Provider call IDs are untrusted path components. Encode every unsafe byte.
            let name = call_id
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>();
            let path = dir.join(format!("{name}.txt"));
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            source.seek(SeekFrom::Start(0))?;
            std::io::copy(source, &mut file)?;
            file.flush()?;
            file.sync_all()?;
            Ok(path)
        };
        save().map_err(|e| EngineError::bad(format!("retain full output: {e}")))
    }
}

pub(crate) fn preview(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut lines = 0_u64;
    let mut last = None;
    let mut buf = [0; 16384];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        lines += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
        last = Some(buf[n - 1]);
    }
    lines += u64::from(last.is_some_and(|b| b != b'\n'));
    file.seek(SeekFrom::Start(0))?;
    let mut head = vec![0; bytes.min(2800) as usize];
    file.read_exact(&mut head)?;
    let mut tail = vec![];
    if bytes > head.len() as u64 {
        let size = (bytes - head.len() as u64).min(2800);
        file.seek(SeekFrom::End(-(size as i64)))?;
        file.read_to_end(&mut tail)?;
    }
    Ok(format!(
        "{}{}{}\nfull output ({bytes} bytes, {lines} lines): {} — use read/grep on it",
        String::from_utf8_lossy(&head),
        if tail.is_empty() { "" } else { "\n…\n" },
        String::from_utf8_lossy(&tail),
        path.display()
    ))
}
