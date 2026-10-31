//! Terminal helpers: ANSI colour support, width detection, number formatting.

use std::io::IsTerminal;
use std::time::Duration;

/// Colour and escape-sequence support for stdout.
#[derive(Clone, Copy)]
pub struct Term {
    /// Stdout is a terminal that understands escape sequences (cursor/line control).
    pub vt: bool,
    /// Colour output (requires `vt` and no NO_COLOR).
    pub color: bool,
}

impl Term {
    pub fn detect() -> Term {
        let tty = std::io::stdout().is_terminal();
        let vt = tty && enable_vt();
        let color = vt && std::env::var_os("NO_COLOR").is_none();
        Term { vt, color }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("1;31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("1;32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("1;33", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}

#[cfg(windows)]
fn enable_vt() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, STD_OUTPUT_HANDLE,
    };
    // SAFETY: plain Win32 console calls on the process's own stdout handle.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if h.is_null() || GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        if mode & 0x0004 != 0 {
            return true;
        }
        SetConsoleMode(h, mode | 0x0004) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

/// Console width in columns (fallback 100).
#[cfg(windows)]
pub fn width() -> usize {
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
    };
    // SAFETY: querying the console buffer of our own stdout; struct is zero-initialised POD.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if !h.is_null() && GetConsoleScreenBufferInfo(h, &mut info) != 0 {
            let w = (info.srWindow.Right - info.srWindow.Left + 1) as usize;
            if w >= 20 {
                return w;
            }
        }
    }
    100
}

#[cfg(not(windows))]
pub fn width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(100)
}

pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// `h:mm:ss`, or `m:ss.s` style for short sub-minute values is not needed.
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn fmt_dur_precise(d: Duration) -> String {
    if d.as_secs() >= 60 {
        fmt_dur(d)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Throughput in GB/s (decimal gigabytes).
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    format!("{:.2} GB/s", bytes_per_sec / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1 << 30), "1.00 GiB");
        assert_eq!(fmt_dur(Duration::from_secs(3725)), "1:02:05");
        assert_eq!(fmt_rate(12.345e9), "12.35 GB/s");
    }
}
