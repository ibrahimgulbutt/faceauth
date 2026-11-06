//! Terminal output: colours when stdout is a terminal, and the enrollment
//! progress line.

use std::io::{IsTerminal, Write};

#[derive(Clone, Copy)]
pub enum Style {
    Ok,
    Warn,
    Error,
    Bold,
    Dim,
}

impl Style {
    pub fn paint(self, text: &str) -> String {
        if !colour() {
            return text.to_string();
        }
        let code = match self {
            Style::Ok => "32",
            Style::Warn => "33",
            Style::Error => "31",
            Style::Bold => "1",
            Style::Dim => "2",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn colour() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// "2026-11-06" from Unix seconds, or "unknown".
pub fn date(secs: u64) -> String {
    if secs == 0 {
        return "unknown".into();
    }
    // Civil-from-days (Howard Hinnant), avoiding a date-time dependency.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Two-line live view on a terminal, plain lines otherwise.
pub struct Progress {
    live: bool,
    drawn: bool,
    last: Option<(usize, String, String)>,
}

impl Progress {
    pub fn new() -> Self {
        Self { live: std::io::stdout().is_terminal(), drawn: false, last: None }
    }

    pub fn start(&mut self, _target: usize) {}

    pub fn update(&mut self, collected: usize, target: usize, instruction: &str, detail: &str, problem: bool) {
        let key = (collected, instruction.to_string(), detail.to_string());
        if self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);
        let bar: String = (0..target).map(|i| if i < collected { '●' } else { '○' }).collect();
        let bar = Style::Ok.paint(&bar);
        let detail = if problem { Style::Warn.paint(detail) } else { Style::Dim.paint(detail) };
        let mut out = std::io::stdout();
        if self.live {
            if self.drawn {
                // Back to the start of our two lines and clear them.
                let _ = write!(out, "\x1b[2A\r\x1b[J");
            }
            let _ = writeln!(out, "  {bar}  {collected}/{target}  {}", Style::Bold.paint(instruction));
            let _ = writeln!(out, "  {detail}");
            self.drawn = true;
        } else {
            let _ = writeln!(out, "[{collected}/{target}] {instruction}  {detail}");
        }
        let _ = out.flush();
    }

    pub fn finish(&mut self) {
        if self.drawn {
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_dates() {
        assert_eq!(super::date(0), "unknown");
        assert_eq!(super::date(1_762_387_200), "2025-11-06");
        assert_eq!(super::date(951_782_400), "2000-02-29");
    }
}
