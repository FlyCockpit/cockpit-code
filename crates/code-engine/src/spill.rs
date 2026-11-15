//! Bounded provider output: kept in memory up to a cap, spilled to an unnamed
//! 0600 temp file in the session's private `tmp/` directory above it, and
//! refused above an absolute ceiling (`generate.maxOutputBytes`).
//!
//! Spill files are created with `O_TMPFILE` (or created and unlinked at once),
//! so they never have a name to leak: closing the handle frees the space on
//! success, error, cancellation and even a crash.
use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Map, Value};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// In-memory cap for text/JSON (a raw provider response).
pub(crate) const TEXT_CAP: u64 = 64 * 1024 * 1024;
/// In-memory cap for decoded media.
pub(crate) const MEDIA_CAP: u64 = 256 * 1024 * 1024;
/// Strings longer than this in a spilled JSON document stay in the file.
const INLINE_STRING: usize = 64 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub text_cap: u64,
    pub media_cap: u64,
    pub ceiling: u64,
    /// Longest JSON string kept inline when a spilled response is scanned.
    pub inline_string: usize,
}
impl Limits {
    pub(crate) fn new(ceiling: u64) -> Self {
        Self {
            text_cap: TEXT_CAP,
            media_cap: MEDIA_CAP,
            ceiling,
            inline_string: INLINE_STRING,
        }
    }
}

pub(crate) fn ceiling_error(what: &str, ceiling: u64) -> anyhow::Error {
    anyhow!(
        "{what} exceeds generate.maxOutputBytes ({ceiling} bytes); raise it in config.json if this output is expected"
    )
}

/// The session's private temp directory: created 0700 on first spill.
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))?;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        bail!("{} is not owned by this user", path.display());
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A growing buffer: memory up to `cap` bytes, then an unnamed temp file.
pub(crate) struct Spill {
    dir: PathBuf,
    cap: u64,
    ceiling: u64,
    what: &'static str,
    memory: Vec<u8>,
    file: Option<File>,
    len: u64,
}
impl Spill {
    pub(crate) fn new(dir: &Path, cap: u64, ceiling: u64, what: &'static str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            cap,
            ceiling,
            what,
            memory: Vec::new(),
            file: None,
            len: 0,
        }
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let len = self.len + bytes.len() as u64;
        if len > self.ceiling {
            return Err(ceiling_error(self.what, self.ceiling));
        }
        if self.file.is_none() && len > self.cap {
            private_dir(&self.dir)?;
            // Unnamed (O_TMPFILE where supported), but its mode still follows
            // the umask, so it is narrowed to 0600 explicitly.
            let mut file = tempfile::tempfile_in(&self.dir)
                .with_context(|| format!("spill {} to {}", self.what, self.dir.display()))?;
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(&self.memory)?;
            self.memory = Vec::new();
            self.file = Some(file);
        }
        match &mut self.file {
            Some(file) => file.write_all(bytes)?,
            None => {
                // Amortized growth, but never past the cap.
                if self.memory.capacity() < len as usize {
                    let want = (self.memory.capacity() * 2)
                        .max(len as usize)
                        .min(self.cap as usize);
                    self.memory.reserve_exact(want - self.memory.len());
                }
                self.memory.extend_from_slice(bytes);
            }
        }
        self.len = len;
        Ok(())
    }
    pub(crate) fn finish(self) -> Result<Spilled> {
        Ok(match self.file {
            Some(mut file) => {
                file.flush()?;
                Spilled::File {
                    file,
                    len: self.len,
                }
            }
            None => Spilled::Memory(self.memory),
        })
    }
}

pub(crate) enum Spilled {
    Memory(Vec<u8>),
    File { file: File, len: u64 },
}
impl Spilled {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
        }
    }
    pub(crate) fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }
    /// Bytes `start..end`, read without moving any shared file offset.
    fn range(&self, start: u64, end: u64) -> Box<dyn Read + Send + '_> {
        match self {
            Self::Memory(bytes) => Box::new(&bytes[start as usize..end as usize]),
            Self::File { file, .. } => Box::new(Range {
                file,
                pos: start,
                end,
            }),
        }
    }
    /// The first `n` bytes (fewer when shorter).
    pub(crate) fn head(&self, n: usize) -> Result<Vec<u8>> {
        let mut head = Vec::new();
        self.range(0, self.len().min(n as u64))
            .read_to_end(&mut head)?;
        Ok(head)
    }
    pub(crate) fn into_reader(self) -> Result<Box<dyn Read + Send>> {
        Ok(match self {
            Self::Memory(bytes) => Box::new(io::Cursor::new(bytes)),
            Self::File { mut file, .. } => {
                file.seek(SeekFrom::Start(0))?;
                Box::new(file)
            }
        })
    }
}

struct Range<'a> {
    file: &'a File,
    pos: u64,
    end: u64,
}
impl Read for Range<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min((self.end - self.pos) as usize);
        if want == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..want], self.pos)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.pos += n as u64;
        Ok(n)
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid JSON string escape")
}

/// Streams base64 from `input` into `out`; both forms are never held whole.
pub(crate) fn decode_base64(
    input: impl Read,
    out: &mut Spill,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut reader =
        base64::read::DecoderReader::new(input, &base64::engine::general_purpose::STANDARD);
    let mut buf = vec![0; CHUNK];
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(e).context("media output is not valid base64");
            }
            Err(e) => return Err(e).context("read spilled provider response"),
        };
        out.write(&buf[..n])?;
    }
}

fn whitespace_inside() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "whitespace inside base64")
}

/// `str::trim` for a stream: drops leading and trailing Unicode whitespace.
/// Base64 is ASCII, so whitespace followed by more data, and any other
/// non-ASCII character, is an error (`STANDARD.decode` rejects both).
pub(crate) struct Trim<'a, R> {
    inner: BufReader<R>,
    started: bool,
    pending_ws: bool,
    cancel: &'a CancellationToken,
}
impl<'a, R: Read> Trim<'a, R> {
    pub(crate) fn new(inner: R, cancel: &'a CancellationToken) -> Self {
        Self {
            inner: BufReader::with_capacity(CHUNK, inner),
            started: false,
            pending_ws: false,
            cancel,
        }
    }
    /// One non-ASCII character; `None` when it is not valid UTF-8.
    fn char(&mut self) -> io::Result<Option<char>> {
        let mut bytes = [0; 4];
        self.inner.read_exact(&mut bytes[..1])?;
        let len = match bytes[0] {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return Ok(None),
        };
        if self.inner.read_exact(&mut bytes[1..len]).is_err() {
            return Ok(None);
        }
        Ok(std::str::from_utf8(&bytes[..len])
            .ok()
            .and_then(|s| s.chars().next()))
    }
}
impl<R: Read> Read for Trim<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let chunk = self.inner.fill_buf()?;
            let Some(&first) = chunk.first() else {
                return Ok(0);
            };
            if first.is_ascii() && !first.is_ascii_whitespace() && first != b'\x0b' {
                if self.pending_ws {
                    return Err(whitespace_inside());
                }
                self.started = true;
                let run = chunk
                    .iter()
                    .position(|b| !b.is_ascii() || b.is_ascii_whitespace() || *b == b'\x0b')
                    .unwrap_or(chunk.len())
                    .min(buf.len());
                buf[..run].copy_from_slice(&chunk[..run]);
                self.inner.consume(run);
                return Ok(run);
            }
            if first.is_ascii() {
                let run = chunk
                    .iter()
                    .position(|b| !(b.is_ascii_whitespace() || *b == b'\x0b'))
                    .unwrap_or(chunk.len());
                self.inner.consume(run);
            } else if !self.char()?.is_some_and(char::is_whitespace) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "non-ASCII character in base64",
                ));
            }
            self.pending_ws |= self.started;
        }
    }
}

/// Splits an optional `data:<mime>[;...],` prefix off a streamed string.
#[allow(clippy::type_complexity)]
pub(crate) fn data_url<'a>(
    mut reader: Box<dyn Read + Send + 'a>,
    cancel: &CancellationToken,
) -> Result<(Option<String>, Box<dyn Read + Send + 'a>)> {
    let mut prefix = Vec::new();
    let mut byte = [0];
    while prefix.len() < 5 && reader.read(&mut byte)? == 1 {
        prefix.push(byte[0]);
    }
    if prefix != b"data:" {
        return Ok((None, Box::new(io::Cursor::new(prefix).chain(reader))));
    }
    // Only the media type (up to the first `;`) is kept; parameters are
    // skipped without buffering them.
    let mut mime = Vec::new();
    let mut in_params = false;
    let mut reader = BufReader::new(reader);
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            bail!("malformed data URL");
        }
        let Some(end) = chunk.iter().position(|b| *b == b',') else {
            let n = chunk.len();
            if !in_params {
                match chunk.iter().position(|b| *b == b';') {
                    Some(semi) => {
                        mime.extend_from_slice(&chunk[..semi]);
                        in_params = true;
                    }
                    None => mime.extend_from_slice(chunk),
                }
                if mime.len() > INLINE_STRING {
                    bail!("data URL media type is longer than 64 KiB");
                }
            }
            reader.consume(n);
            continue;
        };
        if !in_params {
            let head = &chunk[..end];
            let semi = head.iter().position(|b| *b == b';').unwrap_or(end);
            mime.extend_from_slice(&head[..semi]);
        }
        reader.consume(end + 1);
        break;
    }
    let declared = Some(String::from_utf8_lossy(&mime).into_owned()).filter(|m| !m.is_empty());
    Ok((declared, Box::new(reader)))
}

/// Unescapes the body of a JSON string the scanner already validated.
struct Unescape<R> {
    inner: BufReader<R>,
    pending: Vec<u8>,
}
impl<R: Read> Unescape<R> {
    fn byte(&mut self) -> io::Result<u8> {
        let mut b = [0];
        self.inner.read_exact(&mut b).map_err(|_| invalid())?;
        Ok(b[0])
    }
    fn hex4(&mut self) -> io::Result<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let d = (self.byte()? as char).to_digit(16).ok_or_else(invalid)?;
            v = v * 16 + d;
        }
        Ok(v)
    }
}
impl<R: Read> Read for Unescape<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.pending.is_empty() {
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return Ok(n);
        }
        let chunk = self.inner.fill_buf()?;
        if chunk.is_empty() {
            return Ok(0);
        }
        let run = chunk
            .iter()
            .position(|b| *b == b'\\')
            .unwrap_or(chunk.len());
        if run > 0 {
            let n = run.min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            self.inner.consume(n);
            return Ok(n);
        }
        self.inner.consume(1);
        let mut out = [0; 4];
        let bytes: &[u8] = match self.byte()? {
            b @ (b'"' | b'\\' | b'/') => {
                out[0] = b;
                &out[..1]
            }
            b'b' => b"\x08",
            b'f' => b"\x0c",
            b'n' => b"\n",
            b'r' => b"\r",
            b't' => b"\t",
            b'u' => {
                let c = escaped_char(self.hex4()?, || {
                    if self.byte()? != b'\\' || self.byte()? != b'u' {
                        return Err(invalid());
                    }
                    self.hex4()
                })?;
                c.encode_utf8(&mut out).as_bytes()
            }
            _ => return Err(invalid()),
        };
        self.pending.extend_from_slice(bytes);
        self.read(buf)
    }
}

fn escaped_char(unit: u32, mut low: impl FnMut() -> io::Result<u32>) -> io::Result<char> {
    match unit {
        0xd800..=0xdbff => {
            let low = low()?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(invalid());
            }
            char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)).ok_or_else(invalid)
        }
        0xdc00..=0xdfff => Err(invalid()),
        _ => char::from_u32(unit).ok_or_else(invalid),
    }
}

/// A provider JSON response. In memory it is parsed exactly as before
/// (`from_utf8_lossy` + `serde_json`). Spilled, it is scanned from the file:
/// strings longer than `inline_string` stay there and appear in `value` as
/// unforgeable placeholders that [`Document::text`] resolves to a stream.
pub(crate) struct Document {
    pub value: Value,
    spilled: Option<Spilled>,
    marker: String,
    ranges: Vec<(u64, u64)>,
}
pub(crate) enum Text<'a> {
    Inline(&'a str),
    Stream(Box<dyn Read + Send + 'a>),
}
impl Document {
    pub(crate) fn parse(body: Spilled, limits: Limits, cancel: &CancellationToken) -> Result<Self> {
        let marker = format!("\u{0}cockpit-spilled-string:{}:", uuid::Uuid::new_v4());
        // In memory every string stays inline, as serde_json would keep it;
        // the budget then also covers the body's own strings (at most
        // `text_cap`), so it is twice the cap. Spilled, long strings stay in
        // the file and the budget covers the rest.
        let (inline_string, budget) = if body.is_file() {
            (limits.inline_string, limits.text_cap)
        } else {
            (usize::MAX, limits.text_cap.saturating_mul(2))
        };
        let (value, ranges) = {
            let mut scanner = Scanner {
                input: BufReader::with_capacity(CHUNK, body.range(0, body.len())),
                pos: 0,
                next_check: 0,
                used: 0,
                budget,
                inline_string,
                marker: &marker,
                ranges: Vec::new(),
                cancel,
            };
            let value = match scanner.document() {
                Ok(value) => value,
                // As before: a response that is not JSON is `null`.
                Err(Fail::Syntax) => Value::Null,
                Err(Fail::Io(e)) => {
                    return Err(e).context("read spilled provider response");
                }
                Err(Fail::Budget) => bail!(
                    "provider response JSON needs more than {} MiB of memory to parse",
                    budget >> 20
                ),
                Err(Fail::Cancelled) => bail!("cancelled"),
            };
            (value, std::mem::take(&mut scanner.ranges))
        };
        Ok(Self {
            value,
            spilled: body.is_file().then_some(body),
            marker,
            ranges,
        })
    }
    pub(crate) fn text<'a>(&'a self, s: &'a str) -> Text<'a> {
        let range = s
            .strip_prefix(self.marker.as_str())
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.ranges.get(i));
        match (range, &self.spilled) {
            (Some(&(start, end)), Some(spilled)) => Text::Stream(Box::new(Unescape {
                inner: BufReader::with_capacity(CHUNK, spilled.range(start, end)),
                pending: Vec::new(),
            })),
            _ => Text::Inline(s),
        }
    }
    /// The first `chars` characters of a string value, for diagnostics.
    pub(crate) fn prefix(&self, s: &str, chars: usize) -> String {
        match self.text(s) {
            Text::Inline(s) => s.chars().take(chars).collect(),
            Text::Stream(reader) => {
                let mut bytes = Vec::new();
                let _ = reader.take(chars as u64 * 4).read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(chars)
                    .collect()
            }
        }
    }
}

enum Fail {
    Syntax,
    Io(io::Error),
    Budget,
    Cancelled,
}
impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::InvalidData {
            Self::Syntax
        } else {
            Self::Io(e)
        }
    }
}

/// serde_json's default recursion limit: 127 nested containers parse.
const DEPTH: usize = 127;
/// Memory charged per JSON node against the in-memory budget: a `Value`,
/// doubled for container growth.
const NODE_COST: u64 = 2 * std::mem::size_of::<Value>() as u64;
/// Per object entry: the key `String` plus the map's hash and index slots.
const KEY_COST: u64 = 2 * (std::mem::size_of::<String>() as u64 + 16);
/// The memory charged for `value`: what the scanner charged (and refunds
/// when a duplicate key replaces it), also used to budget retained values.
pub(crate) fn value_cost(value: &Value) -> u64 {
    NODE_COST
        + match value {
            Value::String(s) => s.capacity() as u64,
            Value::Array(items) => items.iter().map(value_cost).sum(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| KEY_COST + k.capacity() as u64 + value_cost(v))
                .sum(),
            _ => 0,
        }
}

/// Bytes scanned between cancellation checks.
const CHECK_EVERY: u64 = 1024 * 1024;

struct Scanner<'a, R> {
    input: BufReader<R>,
    pos: u64,
    next_check: u64,
    used: u64,
    budget: u64,
    inline_string: usize,
    marker: &'a str,
    ranges: Vec<(u64, u64)>,
    cancel: &'a CancellationToken,
}
impl<R: Read> Scanner<'_, R> {
    fn peek(&mut self) -> Result<Option<u8>, Fail> {
        Ok(self.input.fill_buf()?.first().copied())
    }
    fn bump(&mut self) {
        self.input.consume(1);
        self.pos += 1;
    }
    fn next(&mut self) -> Result<u8, Fail> {
        let b = self.peek()?.ok_or(Fail::Syntax)?;
        self.bump();
        Ok(b)
    }
    fn tick(&mut self) -> Result<(), Fail> {
        if self.pos >= self.next_check {
            self.next_check = self.pos + CHECK_EVERY;
            if self.cancel.is_cancelled() {
                return Err(Fail::Cancelled);
            }
        }
        Ok(())
    }
    fn ws(&mut self) -> Result<Option<u8>, Fail> {
        loop {
            self.tick()?;
            let chunk = self.input.fill_buf()?;
            let run = chunk
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                .unwrap_or(chunk.len());
            if run < chunk.len() || chunk.is_empty() {
                let next = chunk.get(run).copied();
                self.input.consume(run);
                self.pos += run as u64;
                return Ok(next);
            }
            self.input.consume(run);
            self.pos += run as u64;
        }
    }
    fn charge(&mut self, n: u64) -> Result<(), Fail> {
        self.used += n;
        if self.used > self.budget {
            return Err(Fail::Budget);
        }
        Ok(())
    }
    fn document(&mut self) -> Result<Value, Fail> {
        let value = self.value(0)?;
        if self.ws()?.is_some() {
            return Err(Fail::Syntax);
        }
        Ok(value)
    }
    fn value(&mut self, depth: usize) -> Result<Value, Fail> {
        self.charge(NODE_COST)?;
        self.tick()?;
        match self.ws()?.ok_or(Fail::Syntax)? {
            b'{' => {
                if depth == DEPTH {
                    return Err(Fail::Syntax);
                }
                self.bump();
                let mut map = Map::new();
                if self.ws()? == Some(b'}') {
                    self.bump();
                    return Ok(Value::Object(map));
                }
                loop {
                    if self.ws()? != Some(b'"') {
                        return Err(Fail::Syntax);
                    }
                    self.charge(KEY_COST)?;
                    let key = self.string(false)?;
                    if self.ws()? != Some(b':') {
                        return Err(Fail::Syntax);
                    }
                    self.bump();
                    let key_cost = KEY_COST + key.capacity() as u64;
                    let value = self.value(depth + 1)?;
                    if let Some(old) = map.insert(key, value) {
                        // serde_json keeps the last duplicate's value; the
                        // replaced value and the second key are freed.
                        self.used = self.used.saturating_sub(value_cost(&old) + key_cost);
                    }
                    match (self.ws()?, self.next()?) {
                        (_, b',') => {}
                        (_, b'}') => return Ok(Value::Object(map)),
                        _ => return Err(Fail::Syntax),
                    }
                }
            }
            b'[' => {
                if depth == DEPTH {
                    return Err(Fail::Syntax);
                }
                self.bump();
                let mut items = Vec::new();
                if self.ws()? == Some(b']') {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    match (self.ws()?, self.next()?) {
                        (_, b',') => {}
                        (_, b']') => return Ok(Value::Array(items)),
                        _ => return Err(Fail::Syntax),
                    }
                }
            }
            b'"' => Ok(Value::String(self.string(true)?)),
            b't' => self.literal(b"true", Value::Bool(true)),
            b'f' => self.literal(b"false", Value::Bool(false)),
            b'n' => self.literal(b"null", Value::Null),
            b'-' | b'0'..=b'9' => {
                let mut token = Vec::new();
                while let Some(b @ (b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) =
                    self.peek()?
                {
                    if self.used + token.len() as u64 > self.budget {
                        return Err(Fail::Budget);
                    }
                    token.push(b);
                    self.bump();
                    self.tick()?;
                }
                match serde_json::from_slice::<Value>(&token) {
                    Ok(number @ Value::Number(_)) => Ok(number),
                    _ => Err(Fail::Syntax),
                }
            }
            _ => Err(Fail::Syntax),
        }
    }
    fn literal(&mut self, word: &[u8], value: Value) -> Result<Value, Fail> {
        for expected in word {
            if self.next()? != *expected {
                return Err(Fail::Syntax);
            }
        }
        Ok(value)
    }
    fn hex4(&mut self) -> Result<u32, Fail> {
        let mut v = 0;
        for _ in 0..4 {
            v = v * 16 + (self.next()? as char).to_digit(16).ok_or(Fail::Syntax)?;
        }
        Ok(v)
    }
    /// A string at the opening quote. Values longer than `inline_string`
    /// (when `may_spill`) become placeholders for their byte range.
    fn string(&mut self, may_spill: bool) -> Result<String, Fail> {
        self.bump();
        let start = self.pos;
        let mut out = Vec::new();
        let mut spilled = false;
        loop {
            self.tick()?;
            let chunk = self.input.fill_buf()?;
            if chunk.is_empty() {
                return Err(Fail::Syntax);
            }
            let run = chunk
                .iter()
                .position(|b| *b == b'"' || *b == b'\\' || *b < 0x20)
                .unwrap_or(chunk.len());
            if !spilled {
                out.extend_from_slice(&chunk[..run]);
            }
            self.input.consume(run);
            self.pos += run as u64;
            if run == 0 {
                match self.next()? {
                    b'"' => break,
                    b'\\' => {
                        let mut utf8 = [0; 4];
                        let bytes: &[u8] = match self.next()? {
                            b @ (b'"' | b'\\' | b'/') => {
                                utf8[0] = b;
                                &utf8[..1]
                            }
                            b'b' => b"\x08",
                            b'f' => b"\x0c",
                            b'n' => b"\n",
                            b'r' => b"\r",
                            b't' => b"\t",
                            b'u' => {
                                let unit = self.hex4()?;
                                let low = if (0xd800..=0xdbff).contains(&unit) {
                                    if self.next()? != b'\\' || self.next()? != b'u' {
                                        return Err(Fail::Syntax);
                                    }
                                    Some(self.hex4()?)
                                } else {
                                    None
                                };
                                let c = escaped_char(unit, || low.ok_or_else(invalid))?;
                                c.encode_utf8(&mut utf8).as_bytes()
                            }
                            _ => return Err(Fail::Syntax),
                        };
                        if !spilled {
                            out.extend_from_slice(bytes);
                        }
                    }
                    _ => return Err(Fail::Syntax),
                }
            }
            if !spilled {
                if may_spill && out.len() > self.inline_string {
                    spilled = true;
                    out = Vec::new();
                } else if self.used + out.len() as u64 > self.budget {
                    return Err(Fail::Budget);
                }
            }
        }
        if spilled {
            let index = self.ranges.len();
            self.ranges.push((start, self.pos - 1));
            let placeholder = format!("{}{index}", self.marker);
            self.charge(placeholder.capacity() as u64)?;
            return Ok(placeholder);
        }
        let mut s = match String::from_utf8(out) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        if s.capacity() - s.len() > 4096 {
            s.shrink_to_fit();
        }
        // Charged after the lossy conversion (which can triple invalid
        // bytes), so `value_cost` refunds exactly what was charged.
        self.charge(s.capacity() as u64)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn limits(text_cap: u64, media_cap: u64, ceiling: u64) -> Limits {
        Limits {
            text_cap,
            media_cap,
            ceiling,
            inline_string: 8,
        }
    }

    fn spilled(dir: &Path, bytes: &[u8]) -> Spilled {
        let mut spill = Spill::new(dir, 0, u64::MAX, "test");
        spill.write(bytes).unwrap();
        let spilled = spill.finish().unwrap();
        assert!(spilled.is_file() || bytes.is_empty());
        spilled
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |d| d.count())
    }

    #[test]
    fn spill_stays_in_memory_up_to_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tmp");
        let mut spill = Spill::new(&dir, 8, 100, "test output");
        spill.write(b"0123").unwrap();
        spill.write(b"4567").unwrap();
        let spilled = spill.finish().unwrap();
        assert!(!spilled.is_file());
        assert_eq!(spilled.head(100).unwrap(), b"01234567");
        // Nothing touched the disk, not even the private directory.
        assert!(!dir.exists());
    }

    #[test]
    fn spill_one_byte_over_the_cap_moves_to_a_private_unnamed_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session").join("tmp");
        let mut spill = Spill::new(&dir, 8, 100, "test output");
        spill.write(b"01234567").unwrap();
        spill.write(b"8").unwrap();
        spill.write(b"9").unwrap();
        let spilled = spill.finish().unwrap();
        assert!(spilled.is_file());
        assert_eq!(spilled.len(), 10);
        let Spilled::File { file, .. } = &spilled else {
            unreachable!()
        };
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // The file has no name, so nothing can leak even on a crash.
        assert_eq!(entries(&dir), 0);
        assert_eq!(spilled.head(4).unwrap(), b"0123");
        let mut all = Vec::new();
        spilled
            .into_reader()
            .unwrap()
            .read_to_end(&mut all)
            .unwrap();
        assert_eq!(all, b"0123456789");
        assert_eq!(entries(&dir), 0);
    }

    #[test]
    fn spill_refuses_more_than_the_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tmp");
        let mut spill = Spill::new(&dir, 4, 10, "generation response");
        spill.write(b"0123456789").unwrap();
        let error = spill.write(b"x").unwrap_err().to_string();
        assert!(
            error.contains("generation response exceeds generate.maxOutputBytes (10 bytes)"),
            "{error}"
        );
        drop(spill);
        assert_eq!(entries(&dir), 0);
    }

    fn stream_decode(input: &[u8], cap: u64) -> Result<Vec<u8>> {
        let root = tempfile::tempdir().unwrap();
        let mut out = Spill::new(root.path(), cap, u64::MAX, "media");
        decode_base64(input, &mut out, &CancellationToken::new())?;
        let mut bytes = Vec::new();
        out.finish()?.into_reader()?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    #[test]
    fn streaming_base64_matches_whole_buffer_decoding() {
        let standard = &base64::engine::general_purpose::STANDARD;
        let mut inputs: Vec<Vec<u8>> = [
            "",
            "AP8=",
            "AP8",
            "AP_=",
            "!!!!",
            "AA==AA==",
            "AP8=====",
            "AAAA",
            "AAA=",
            "AA=A",
            "A===",
            "AP9=",
            "QUJD\nREVG",
            "QUJD REVG",
            "QQ",
            "QQ=",
            "QQ==",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        // Long inputs cross the decoder's internal chunk boundaries.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for len in [1, 2, 3, 100, 1023, 4096, 70_000, 200_001] {
            let bytes = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect::<Vec<u8>>();
            let encoded = standard.encode(&bytes);
            inputs.push(encoded.clone().into_bytes());
            let mut broken = encoded.clone().into_bytes();
            broken[encoded.len() / 2] = b'*';
            inputs.push(broken);
            let mut padded_inside = encoded.into_bytes();
            padded_inside.splice(0..0, *b"AA==");
            inputs.push(padded_inside);
        }
        for input in inputs {
            let whole = standard
                .decode(&input)
                .map_err(|e| format!("media output is not valid base64: {e}"));
            for cap in [0, u64::MAX] {
                // Same bytes, and under the cap the same error text.
                let streamed = stream_decode(&input, cap).map_err(|e| format!("{e:#}"));
                assert_eq!(
                    streamed,
                    whole,
                    "{:?}",
                    String::from_utf8_lossy(&input[..input.len().min(40)])
                );
            }
        }
        let error = format!("{:#}", stream_decode(b"AP_=", u64::MAX).unwrap_err());
        assert_eq!(
            error,
            "media output is not valid base64: Invalid symbol 95, offset 2."
        );
    }

    #[test]
    fn trim_matches_str_trim_for_base64() {
        for (input, expected) in [
            (" \n\tAP8= \r\n", Some(vec![0, 255])),
            ("AP8=", Some(vec![0, 255])),
            ("AP 8=", None),
            ("   ", Some(vec![])),
            ("\x0bAP8=\x0c", Some(vec![0, 255])),
            ("\u{a0}\u{3000}AP8=\u{2029}\u{85} ", Some(vec![0, 255])),
            ("AP8=\u{a0}AP8=", None),
            ("AP\u{e9}=", None),
            ("\u{e9}", None),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut out = Spill::new(root.path(), 0, u64::MAX, "media");
            // One byte per read exercises every boundary.
            let cancel = CancellationToken::new();
            let reader = Trim::new(OneByte(input.as_bytes()), &cancel);
            let got = decode_base64(reader, &mut out, &cancel).ok().map(|()| {
                let mut bytes = Vec::new();
                out.finish()
                    .unwrap()
                    .into_reader()
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                bytes
            });
            assert_eq!(got, expected, "{input:?}");
            let whole = base64::engine::general_purpose::STANDARD
                .decode(input.trim())
                .ok();
            assert_eq!(got, whole, "{input:?}");
        }
    }

    struct OneByte<'a>(&'a [u8]);
    impl Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    fn resolve(document: &Document, value: &Value) -> Value {
        match value {
            Value::String(s) => match document.text(s) {
                Text::Inline(s) => Value::String(s.into()),
                Text::Stream(mut reader) => {
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).unwrap();
                    Value::String(String::from_utf8(bytes).unwrap())
                }
            },
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| resolve(document, v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), resolve(document, v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    #[test]
    fn spilled_json_scan_matches_serde_json() {
        let root = tempfile::tempdir().unwrap();
        let deep_ok = format!("{}{}", "[".repeat(127), "]".repeat(127));
        let deep_bad = format!("{}{}", "[".repeat(128), "]".repeat(128));
        let documents = [
            r#"{"a":1,"b":[true,false,null],"c":"short","d":-1.5e3}"#,
            r#"{"output":[{"type":"image_generation_call","result":"QUJDREVGR0hJSktMTU5PUA=="}]}"#,
            r#"{"s":"esc \" \\ \/ \b\f\n\r\t é 😀 long enough to spill"}"#,
            r#"{"dup":1,"dup":"second value that spills"}"#,
            r#"  [1, 2 , {"k" : "v"} ]  "#,
            r#"{"big":12345678901234567890,"f":0.1}"#,
            r#"{"lone":"\ud800 is a lone surrogate"}"#,
            r#"{"ctl":"raw control char"}"#,
            r#"{"a":01}"#,
            r#"{"a":1,}"#,
            r#"{"a":1} trailing"#,
            r#"{"a":"unterminated"#,
            r#"[1.0.0]"#,
            r#"{"a":tru}"#,
            "",
            &deep_ok,
            &deep_bad,
        ];
        for (i, text) in documents.iter().enumerate() {
            let text = if i == 7 {
                text.replace("raw control", "raw\u{1}control")
            } else {
                text.to_string()
            };
            let expected: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let document = Document::parse(
                spilled(root.path(), text.as_bytes()),
                limits(1 << 20, 1 << 20, u64::MAX),
                &CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(resolve(&document, &document.value), expected, "{text}");
            // In memory it is serde_json itself.
            let memory = Document::parse(
                Spilled::Memory(text.as_bytes().to_vec()),
                limits(1 << 20, 1 << 20, u64::MAX),
                &CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(memory.value, expected);
        }
    }

    #[test]
    fn in_memory_parse_matches_serde_json_on_invalid_utf8_and_duplicates() {
        let mut documents: Vec<Vec<u8>> = [
            &b"{\"a\":\"\xc3\"}"[..],
            b"{\"a\":\"\xe2\x82\"}",
            b"{\"a\":\"\xc3\\u00e9\"}",
            b"{\"a\":\"\\u00e9\xa9\"}",
            b"{\"\xff\":1}",
            b"{\"a\":1}\xff",
            b"\xef\xbb\xbf{\"a\":1}",
            b"{\"a\":\"\xf0\x9f\x98\"}",
        ]
        .iter()
        .map(|d| d.to_vec())
        .collect();
        // A flood of duplicate keys is cheap once replaced values are refunded.
        let mut flood = b"{".to_vec();
        for _ in 0..200_000 {
            flood.extend_from_slice(b"\"x\":0,");
        }
        flood.extend_from_slice(b"\"x\":1}");
        documents.push(flood);
        for bytes in documents {
            let expected: Value =
                serde_json::from_str(&String::from_utf8_lossy(&bytes)).unwrap_or(Value::Null);
            let document = Document::parse(
                Spilled::Memory(bytes.clone()),
                limits(1 << 20, 1 << 20, u64::MAX),
                &CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(
                document.value,
                expected,
                "{:?}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(40)])
            );
        }
    }

    #[test]
    fn duplicate_invalid_utf8_refunds_only_what_was_charged() {
        // Each duplicate "d" holds 3,000 invalid bytes (9,000 after lossy
        // conversion); the retained "k" strings must still hit the budget.
        let root = tempfile::tempdir().unwrap();
        let mut body = b"{".to_vec();
        for i in 0..40 {
            body.extend_from_slice(format!("\"k{i}\":\"{}\",", "A".repeat(3000)).as_bytes());
            for _ in 0..2 {
                body.extend_from_slice(b"\"d\":\"");
                body.extend(std::iter::repeat_n(0xffu8, 3000));
                body.extend_from_slice(b"\",");
            }
        }
        body.extend_from_slice(b"\"end\":0}");
        let error = Document::parse(
            spilled(root.path(), &body),
            Limits {
                inline_string: 1 << 20,
                ..limits(100_000, 100_000, u64::MAX)
            },
            &CancellationToken::new(),
        )
        .err()
        .expect("40 x 3,000 retained bytes exceed a 100,000 byte budget");
        assert!(error.to_string().contains("needs more than"), "{error}");
    }

    #[test]
    fn spilled_json_keeps_long_strings_out_of_memory() {
        let root = tempfile::tempdir().unwrap();
        let long = "Q".repeat(10_000);
        let text = format!(
            r#"{{"result":"{long}","error":null,"x":"\u0000cockpit-spilled-string:fake:0"}}"#
        );
        let document = Document::parse(
            spilled(root.path(), text.as_bytes()),
            Limits {
                inline_string: 64,
                ..limits(4096, 4096, u64::MAX)
            },
            &CancellationToken::new(),
        )
        .unwrap();
        let placeholder = document.value["result"].as_str().unwrap();
        assert!(placeholder.len() < 100);
        assert!(matches!(document.text(placeholder), Text::Stream(_)));
        assert_eq!(document.prefix(placeholder, 3), "QQQ");
        // A provider cannot forge a placeholder: the marker carries a fresh UUID.
        let forged = document.value["x"].as_str().unwrap();
        assert!(matches!(document.text(forged), Text::Inline(_)));
        // Non-media JSON is still bounded by the in-memory budget.
        let many = format!("[{}0]", "0,".repeat(100));
        let error = Document::parse(
            spilled(root.path(), many.as_bytes()),
            limits(1024, 1024, u64::MAX),
            &CancellationToken::new(),
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(
            error,
            "provider response JSON needs more than 0 MiB of memory to parse"
        );
        // The in-memory path is bounded the same way (twice the cap, as it
        // also holds the strings), but parses a string-heavy body fully.
        let memory = |text: &str| {
            Document::parse(
                Spilled::Memory(text.as_bytes().to_vec()),
                limits(1 << 20, 1 << 20, u64::MAX),
                &CancellationToken::new(),
            )
        };
        let nodes = format!("[{}0]", "0,".repeat(100_000));
        assert!(memory(&nodes).is_err());
        let string = format!(r#"{{"result":"{}"}}"#, "Q".repeat((1 << 20) - 20));
        assert_eq!(
            memory(&string).unwrap().value,
            serde_json::from_str::<Value>(&string).unwrap()
        );
    }

    #[allow(clippy::type_complexity)]
    fn data_url_test(
        reader: Box<dyn Read + Send + '_>,
    ) -> Result<(Option<String>, Box<dyn Read + Send + '_>)> {
        data_url(reader, &CancellationToken::new())
    }

    #[test]
    fn data_url_prefix_streams() {
        let (mime, mut rest) =
            data_url_test(Box::new(&b"data:image/webp;base64,AP8="[..])).unwrap();
        assert_eq!(mime.as_deref(), Some("image/webp"));
        let mut tail = String::new();
        rest.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "AP8=");
        let (mime, mut rest) = data_url_test(Box::new(&b"AP8="[..])).unwrap();
        assert_eq!(mime, None);
        let mut tail = String::new();
        rest.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "AP8=");
        assert!(data_url_test(Box::new(&b"data:image/png"[..])).is_err());
        let (mime, _) = data_url_test(Box::new(&b"data:;base64,AP8="[..])).unwrap();
        assert_eq!(mime, None);
        // Parameters are skipped, however long, without being buffered.
        let long = format!("data:image/png;x={};base64,QUJD", "p".repeat(200_000));
        let (mime, mut rest) = data_url_test(Box::new(long.as_bytes())).unwrap();
        assert_eq!(mime.as_deref(), Some("image/png"));
        let mut tail = String::new();
        rest.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "QUJD");
    }

    #[test]
    fn cancellation_stops_parsing_and_decoding() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let text = format!(r#"{{"result":"{}"}}"#, "Q".repeat(100_000));
        let error = Document::parse(
            spilled(root.path(), text.as_bytes()),
            limits(1 << 20, 1 << 20, u64::MAX),
            &cancel,
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "cancelled");
        let mut out = Spill::new(root.path(), 0, u64::MAX, "media");
        let error = decode_base64(&b"QUJD"[..], &mut out, &cancel).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        let mut out = Spill::new(root.path(), 0, u64::MAX, "media");
        let spaces = " ".repeat(1 << 20);
        let error =
            decode_base64(Trim::new(spaces.as_bytes(), &cancel), &mut out, &cancel).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        drop(out);
        assert_eq!(entries(root.path()), 0);
    }

    #[test]
    fn private_dir_refuses_symlinks_and_tightens_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let loose = root.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        private_dir(&loose).unwrap();
        assert_eq!(
            std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&loose, &link).unwrap();
        assert!(private_dir(&link).is_err());
        let mut spill = Spill::new(&link, 0, u64::MAX, "test");
        assert!(spill.write(b"x").is_err());
    }
}
