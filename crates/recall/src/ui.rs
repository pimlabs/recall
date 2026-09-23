//! How the human-facing commands look: `doctor`, `status`, `connect`,
//! `init`.
//!
//! One module so the whole CLI has one visual language — the same four
//! marks, the same colours meaning the same thing — rather than each command
//! inventing its own. Everything goes through [`anstream`], which drops the
//! colour codes by itself when output is not a terminal or `NO_COLOR` is set,
//! so piping `recall doctor` into a file or a CI log gives plain text with no
//! escape sequences in it. The marks themselves stay: they are characters,
//! not styling, and they read fine in a log.
//!
//! None of this is a contract. `--json` is the output meant for scripts, and
//! it does not pass through here.

use anstyle::{AnsiColor, Style};

/// What a line reports, and so how it is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Working.
    Good,
    /// Worth a look; nothing is broken.
    Warn,
    /// Broken.
    Bad,
    /// Neither good nor bad — off by choice, or not applicable here.
    Quiet,
}

impl Tone {
    /// The mark in front of a line.
    pub fn mark(self) -> &'static str {
        match self {
            Tone::Good => "✓",
            Tone::Warn => "!",
            Tone::Bad => "✗",
            Tone::Quiet => "○",
        }
    }

    fn style(self) -> Style {
        match self {
            Tone::Good => AnsiColor::Green.on_default(),
            Tone::Warn => AnsiColor::Yellow.on_default().bold(),
            Tone::Bad => AnsiColor::Red.on_default().bold(),
            Tone::Quiet => DIM,
        }
    }
}

const DIM: Style = Style::new().dimmed();
const BOLD: Style = Style::new().bold();
const ACCENT: Style = AnsiColor::Cyan.on_default();

/// `text` in `style`, reset afterwards.
fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// A command's first line: its name, and what it is talking about.
pub fn title(command: &str, about: &str) {
    if about.is_empty() {
        anstream::println!("{}", paint(BOLD, command));
    } else {
        anstream::println!("{}  {}", paint(BOLD, command), paint(DIM, about));
    }
}

/// A group of lines, and the thing it is about when there is one.
pub fn section(name: &str, about: &str) {
    anstream::println!();
    if about.is_empty() {
        anstream::println!("{}", paint(BOLD, name));
    } else {
        anstream::println!("{}  {}", paint(BOLD, name), paint(ACCENT, about));
    }
}

/// One checked thing: a mark, a label padded to `width`, what was found,
/// and — when there is something to do — the fix on the line below.
pub fn check(tone: Tone, label: &str, width: usize, detail: &str, fix: Option<&str>) {
    let detail = match tone {
        Tone::Quiet => paint(DIM, detail),
        _ => detail.to_string(),
    };
    anstream::println!(
        "  {} {:<width$}  {}",
        paint(tone.style(), tone.mark()),
        label,
        detail
    );
    if let Some(fix) = fix {
        anstream::println!(
            "    {:<width$}  {} {}",
            "",
            paint(ACCENT, "→"),
            paint(ACCENT, fix)
        );
    }
}

/// The closing line: one sentence, marked by how things stand.
pub fn verdict(tone: Tone, text: &str) {
    anstream::println!();
    anstream::println!("{} {}", paint(tone.style(), tone.mark()), paint(BOLD, text));
}

/// A `label : value` line from `recall status`, with the label dimmed and a
/// value that reports a problem coloured by how bad it is. The text is
/// exactly what it was before; only the styling is added.
pub fn field_line(line: &str) {
    let Some((label, value)) = line.split_once(" : ").filter(|(l, _)| {
        !l.trim().is_empty()
            && l.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ')
    }) else {
        // Continuation lines and anything else pass through as they are.
        anstream::println!("{line}");
        return;
    };
    let tone = value_tone(value);
    let value = match tone {
        Some(t) => paint(t.style(), value),
        None => value.to_string(),
    };
    anstream::println!("{} {} {}", paint(DIM, label), paint(DIM, ":"), value);
}

/// How bad a status value sounds. Status writes its problems in capitals
/// precisely so they stand out in plain text; this only makes them louder.
fn value_tone(value: &str) -> Option<Tone> {
    if value.contains("UNREACHABLE") || value.contains("UNREADABLE") {
        Some(Tone::Bad)
    } else if value.starts_with("NO")
        || value.contains("NOT ")
        || value.contains("SET BUT")
        || value.contains("readable by other users")
    {
        Some(Tone::Warn)
    } else if value == "(unset)" || value.starts_with("off") {
        Some(Tone::Quiet)
    } else {
        None
    }
}

/// `path` with the home directory written as `~`, for display only.
///
/// The long form is what `--json` carries and what a script should use; a
/// person reading a report does not need `/Users/someone/` repeated on every
/// line to know which home directory is meant.
pub fn tilde(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if home.len() > 1 => text.replace(&home, "~"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_in_status_values_are_toned_by_severity() {
        assert_eq!(value_tone("UNREACHABLE (timed out)"), Some(Tone::Bad));
        assert_eq!(
            value_tone("NO — run 'recall init' in this project"),
            Some(Tone::Warn)
        );
        assert_eq!(
            value_tone("off (set RECALL_GLOBAL_KEY …)"),
            Some(Tone::Quiet)
        );
        assert_eq!(value_tone("(unset)"), Some(Tone::Quiet));
        assert_eq!(value_tone("yes"), None);
        assert_eq!(value_tone("machine:jarvis — 2 file(s), linked"), None);
    }
}
