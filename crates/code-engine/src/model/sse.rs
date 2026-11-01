//! Incremental SSE lines: LF, CRLF, CR, and one optional UTF-8 BOM.
use crate::schema::EngineError;
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    started: bool,
    skip_lf: bool,
}
impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, EngineError> {
        const MAX: usize = 16 * 1024 * 1024;
        let mut frames = vec![];
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let line = std::str::from_utf8(&self.pending)
                    .map_err(|_| EngineError::unavailable("invalid UTF-8 in SSE"))?;
                let line = if !self.started {
                    line.trim_start_matches('\u{feff}')
                } else {
                    line
                };
                self.started = true;
                let line = line.trim_start();
                if line.is_empty() {
                    if !self.data.is_empty() {
                        frames.push(self.data.join("\n"));
                        self.data.clear();
                        self.data_bytes = 0;
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    let data = data.strip_prefix(' ').unwrap_or(data);
                    self.data_bytes += data.len() + 1;
                    self.data.push(data.into());
                }
                self.pending.clear();
            } else {
                self.pending.push(byte);
            }
            if self.pending.len() + self.data_bytes > MAX {
                return Err(EngineError::bad("SSE frame exceeds 16 MiB"));
            }
        }
        Ok(frames)
    }
}
