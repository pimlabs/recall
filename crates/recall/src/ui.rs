//! How the human-facing commands look: `doctor`, `status`, `connect`,
//! `init`, `review`.
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

/// `text` coloured by `tone`, for a mark or a word inside a line.
pub fn toned(tone: Tone, text: &str) -> String {
    paint(tone.style(), text)
}

/// `text` dimmed: what is there for completeness, not to be read first.
pub fn dim(text: &str) -> String {
    paint(DIM, text)
}

/// `text` in bold: a heading, or the one word a line is about.
pub fn bold(text: &str) -> String {
    paint(BOLD, text)
}

/// `text` in the accent colour: a command to run next.
pub fn accent(text: &str) -> String {
    paint(ACCENT, text)
}

/// `text` cut to `max` characters, with an ellipsis when it was cut: a line
/// that fits a terminal, whatever the note says. The full text stays in
/// `--json`.
pub fn clip(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(max.saturating_sub(1)) {
        Some((cut, _)) if one_line.chars().count() > max => {
            format!("{}…", one_line[..cut].trim_end())
        }
        _ => one_line,
    }
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

/// `path` with the home directory written as `~`, for display only.
///
/// The long form is what `--json` carries and what a script should use; a
/// person reading a report does not need `/Users/someone/` repeated on every
/// line to know which home directory is meant.
///
/// Reads the real process environment rather than going through
/// [`recall_hooks::claude::Env`] — this is cosmetic, not a value anything
/// else depends on, so it is not worth threading the settings-layered lookup
/// through for. `USERPROFILE` is the fallback on Windows, which has no `HOME`
/// by default; not verified against a real Windows machine.
pub fn tilde(text: &str) -> String {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| h.len() > 1)
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|h| h.len() > 1));
    match home {
        Some(home) => text.replace(&home, "~"),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Fitting a report to a terminal, and a command's steps. Added with the
// setup and health commands' output (`status`, `doctor`, `init`,
// `backfill`, `connect`, `disconnect`, `promote`).
// ---------------------------------------------------------------------------

/// How many columns a report line may take. Most terminals, and most log
/// viewers, show this much without wrapping it for you, which is worse.
pub const WIDTH: usize = 100;

/// `text` folded into lines of at most `max` characters.
///
/// A line breaks after a clause (`,` `;` `:` `.`), or before a
/// parenthesis, when one falls in its second half, and otherwise at the
/// last space that fits, so a sentence reads in pieces that mean something.
/// Never inside a word: a path or a command longer than `max` stays whole
/// and runs over, because one cut in two no longer works when it is copied.
pub fn fold(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
            continue;
        }
        if line.chars().count() + 1 + word.chars().count() <= max {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        // The last space that ends a clause or opens a parenthesis, when it
        // is late enough in the line not to leave a stub behind.
        let clause = line
            .match_indices(' ')
            .map(|(at, _)| at)
            .rfind(|&at| {
                line[..at].ends_with([',', ';', ':', '.']) || line[at + 1..].starts_with('(')
            })
            .filter(|&at| at >= line.len() / 2);
        match clause {
            Some(at) => {
                let rest = line[at..].trim_start().to_string();
                line.truncate(at);
                lines.push(std::mem::replace(&mut line, rest));
                line.push(' ');
                line.push_str(word);
            }
            None => lines.push(std::mem::replace(&mut line, word.to_string())),
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// [`check`], with `detail` and `fix` folded to fit [`WIDTH`], each line
/// after the first starting in the column its text did.
pub fn check_fitted(tone: Tone, label: &str, width: usize, detail: &str, fix: Option<&str>) {
    // What `check` prints before each: "  ✓ label  " and "    label  → ".
    let (detail_at, fix_at) = (width + 6, width + 8);
    let fitted = |text: &str, at: usize| {
        fold(text, WIDTH.saturating_sub(at).max(40)).join(&format!("\n{}", " ".repeat(at)))
    };
    let fix = fix.map(|f| fitted(f, fix_at));
    check(
        tone,
        label,
        width,
        &fitted(detail, detail_at),
        fix.as_deref(),
    );
}

/// One thing a command did or found, said as a sentence: its mark, the text
/// folded to fit [`WIDTH`], and, when there is something to do about it,
/// what to run on the line below.
pub fn step(tone: Tone, text: &str, next: Option<&str>) {
    let text = fold(text, WIDTH - 4).join("\n    ");
    let text = match tone {
        Tone::Quiet => paint(DIM, &text),
        _ => text,
    };
    anstream::println!("  {} {text}", paint(tone.style(), tone.mark()));
    if let Some(next) = next {
        anstream::println!("    {}", paint(ACCENT, &format!("→ {next}")));
    }
}

/// A "Next" section: each command to run, in the accent colour, with why
/// dimmed beneath it when that is not obvious.
pub fn next_steps(steps: &[(String, Option<String>)]) {
    if steps.is_empty() {
        return;
    }
    section("Next", "");
    for (command, why) in steps {
        anstream::println!("  {}", paint(ACCENT, &format!("→ {command}")));
        if let Some(why) = why {
            for line in fold(why, WIDTH - 4) {
                anstream::println!("    {}", paint(DIM, &line));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_line_is_clipped_to_fit_and_says_so() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one\n two   three", 20), "one two three");
        assert_eq!(clip("abcdefghij klm", 10), "abcdefghi…");
        assert_eq!(clip("abcd efgh", 5), "abcd…");
        assert_eq!(clip("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn a_long_detail_folds_after_a_clause_and_never_inside_a_word() {
        assert_eq!(fold("short", 20), ["short"]);
        assert_eq!(fold("", 20), Vec::<String>::new());
        // After the comma, which falls in the second half of the line.
        assert_eq!(
            fold("the token is set, so every program can read it", 24),
            ["the token is set,", "so every program can", "read it"]
        );
        // No clause late enough: the last space that fits.
        assert_eq!(fold("a, bcd efg hij", 12), ["a, bcd efg", "hij"]);
        // A path longer than the line stays whole.
        assert_eq!(
            fold("key in ~/a/very/long/path/device.key here", 10),
            ["key in", "~/a/very/long/path/device.key", "here"]
        );
        assert_eq!(fold("héllo wörld", 5), ["héllo", "wörld"]);
        // Before a parenthesis rather than inside it.
        assert_eq!(
            fold(
                "key in ~/.recall/device.key (a file readable by you only)",
                40
            ),
            [
                "key in ~/.recall/device.key",
                "(a file readable by you only)"
            ]
        );
    }
}
