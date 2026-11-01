//! Palette, lifted from Cockpit's excoc instrument-panel tokens (dark) with a
//! matching light variant. Truecolor RGB is used when `COLORTERM` advertises
//! it; otherwise each token falls back to its paired 256-color index.
use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Primary text.
    pub ink: Color,
    /// Secondary text.
    pub fog: Color,
    /// Dimmest text: reasoning, disabled.
    pub dim: Color,
    /// Accent: selection, caret, user bar.
    pub accent: Color,
    /// Borders and rules.
    pub rule: Color,
    /// Selected-row wash; also the hover wash (Cockpit's `HOVER_BG`).
    pub select_bg: Color,
    /// Hover/selection foreground for chips and selected rows (Cockpit's
    /// `BRASS`; darkened in the light theme for contrast).
    pub hover_fg: Color,
    /// Lifted surface (status line, dialogs).
    pub surface: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub heading: Color,
    pub link: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub keyword: Color,
    pub string: Color,
    pub comment: Color,
    pub number: Color,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
}

fn truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        })
        .unwrap_or(false)
}

fn pick(tc: bool, rgb: (u8, u8, u8), index: u8) -> Color {
    if tc {
        Color::Rgb(rgb.0, rgb.1, rgb.2)
    } else {
        Color::Indexed(index)
    }
}

impl Theme {
    pub fn new(dark: bool) -> Self {
        let tc = truecolor();
        let c = |rgb, idx| pick(tc, rgb, idx);
        if dark {
            Self {
                ink: c((0xF4, 0xEF, 0xE6), 255),
                fog: c((0x8A, 0x9B, 0xB0), 109),
                dim: c((0x60, 0x6E, 0x7C), 242),
                accent: c((0xE0, 0xB1, 0x56), 179),
                rule: c((0x4A, 0x5A, 0x6A), 240),
                select_bg: c((0x2C, 0x38, 0x46), 236),
                hover_fg: c((0xE0, 0xB1, 0x56), 179),
                surface: c((0x1C, 0x26, 0x30), 235),
                red: c((0xD9, 0x6A, 0x6A), 167),
                green: c((0x7F, 0xC9, 0x8A), 114),
                yellow: c((0xE0, 0xB1, 0x56), 179),
                heading: c((0x6C, 0xC8, 0xF0), 81),
                link: c((0x5F, 0xAF, 0xFF), 75),
                code_fg: c((0xF0, 0xE6, 0xA8), 229),
                code_bg: c((0x24, 0x2C, 0x36), 236),
                keyword: c((0xC6, 0x8C, 0xF0), 177),
                string: c((0x9C, 0xD6, 0x8A), 150),
                comment: c((0x6A, 0x7A, 0x8A), 245),
                number: c((0xF0, 0xA0, 0x6A), 216),
                diff_add_bg: c((0x1E, 0x3A, 0x28), 22),
                diff_del_bg: c((0x45, 0x22, 0x26), 52),
            }
        } else {
            Self {
                ink: c((0x1F, 0x24, 0x2B), 235),
                fog: c((0x56, 0x63, 0x73), 243),
                dim: c((0x8C, 0x96, 0xA2), 246),
                accent: c((0xA8, 0x6B, 0x00), 136),
                rule: c((0xB8, 0xC0, 0xC8), 250),
                // #8A5700 on #DCE3EB is 4.7:1 (WCAG AA); the accent itself is 3.4:1.
                select_bg: c((0xDC, 0xE3, 0xEB), 254),
                hover_fg: c((0x8A, 0x57, 0x00), 94),
                surface: c((0xEE, 0xF1, 0xF4), 255),
                red: c((0xB4, 0x32, 0x32), 124),
                green: c((0x2E, 0x7D, 0x32), 28),
                yellow: c((0x9A, 0x6A, 0x00), 136),
                heading: c((0x00, 0x5F, 0x9E), 25),
                link: c((0x1A, 0x5F, 0xB4), 26),
                code_fg: c((0x5A, 0x3E, 0x00), 94),
                code_bg: c((0xF2, 0xEE, 0xE2), 255),
                keyword: c((0x7A, 0x2E, 0xA8), 91),
                string: c((0x2E, 0x6E, 0x1E), 28),
                comment: c((0x80, 0x8A, 0x96), 245),
                number: c((0xA8, 0x4A, 0x00), 130),
                diff_add_bg: c((0xDD, 0xF4, 0xDD), 194),
                diff_del_bg: c((0xFA, 0xDD, 0xDD), 224),
            }
        }
    }
    pub fn text(&self) -> Style {
        Style::default().fg(self.ink)
    }
    pub fn muted(&self) -> Style {
        Style::default().fg(self.fog)
    }
    pub fn faint(&self) -> Style {
        Style::default().fg(self.dim)
    }
    pub fn bold(&self) -> Style {
        Style::default().fg(self.ink).add_modifier(Modifier::BOLD)
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

pub fn init(dark: bool) {
    let _ = THEME.set(Theme::new(dark));
}

pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| Theme::new(true))
}

/// Resolve `auto|light|dark`. Auto uses `COLORFGBG`, then an OSC 11 background
/// query to the terminal (raw mode must be on), then falls back to dark.
/// Whether the theme is dark, plus any keystrokes the terminal query read
/// ahead of its reply (typed before the UI was up; they go to the composer).
pub fn detect_dark(setting: &str) -> (bool, Vec<u8>) {
    match setting {
        "light" => return (false, vec![]),
        "dark" => return (true, vec![]),
        _ => {}
    }
    if let Ok(v) = std::env::var("COLORFGBG")
        && let Some(bg) = v.rsplit(';').next().and_then(|s| s.parse::<u8>().ok())
    {
        return (!matches!(bg, 7 | 15), vec![]);
    }
    let (luma, typed) = query_background();
    (luma.map(|l| l < 0.5).unwrap_or(true), typed)
}

/// Ask the terminal for its background color (OSC 11) and return its luminance.
#[cfg(unix)]
fn query_background() -> (Option<f32>, Vec<u8>) {
    let mut typed = vec![];
    let luma = query_background_into(&mut typed);
    (luma, typed)
}

/// The OSC 11 query; bytes before the reply (keystrokes) land in `typed`.
#[cfg(unix)]
fn query_background_into(typed: &mut Vec<u8>) -> Option<f32> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    // Non-blocking reads with a short sleep loop: poll(2) does not support
    // tty devices on macOS.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")
        .ok()?;
    tty.write_all(b"\x1b]11;?\x1b\\").ok()?;
    tty.flush().ok()?;
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while std::time::Instant::now() < deadline {
        let mut chunk = [0u8; 64];
        match tty.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.ends_with(b"\x07") || buf.ends_with(b"\x1b\\") {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    // Everything outside the reply was typed by the user.
    let reply = buf.windows(5).position(|w| w == b"\x1b]11;");
    match reply {
        Some(at) => {
            typed.extend_from_slice(&buf[..at]);
            let rest = &buf[at..];
            let end = rest
                .iter()
                .position(|&b| b == 0x07)
                .map(|i| i + 1)
                .or_else(|| rest.windows(2).position(|w| w == b"\x1b\\").map(|i| i + 2))
                .unwrap_or(rest.len());
            typed.extend_from_slice(&rest[end..]);
            buf.truncate(at + end);
            buf.drain(..at);
        }
        None => {
            typed.extend_from_slice(&buf);
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let rgb = text.split("rgb:").nth(1)?;
    let parts: Vec<f32> = rgb
        .split(['/', '\x07', '\x1b'])
        .take(3)
        .filter_map(|p| {
            let hex: String = p.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            let max = 16f32.powi(hex.len() as i32) - 1.0;
            u32::from_str_radix(&hex, 16).ok().map(|v| v as f32 / max)
        })
        .collect();
    if parts.len() != 3 {
        return None;
    }
    Some(0.2126 * parts[0] + 0.7152 * parts[1] + 0.0722 * parts[2])
}

#[cfg(not(unix))]
fn query_background() -> (Option<f32>, Vec<u8>) {
    (None, vec![])
}
