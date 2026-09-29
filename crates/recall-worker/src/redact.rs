//! Finding and masking secrets in memory text.
//!
//! Split out of the evaluation (behind the `client` feature) so
//! [`Redactor`], and the local `claude` call in [`crate::merge`], are
//! usable without the worker's HTTP client: `docs/history/memory-truth.md`
//! decision 2. `recall-server` already depends on this crate the same way
//! for the merge; this module is exactly as unconditional.
//!
//! Everything here reads text and returns text or spans: no I/O, no
//! network, nothing that needs the `client` feature.

use std::collections::HashMap;

use recall_wire::EvaluateFile;

/// One kind of token: a prefix, and what may follow it.
struct Pattern {
    what: &'static str,
    prefix: &'static str,
    allowed: fn(u8) -> bool,
    min: usize,
    max: usize,
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn upper_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn dashed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
fn keyish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}
fn base64ish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+')
}
fn hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}
fn unspaced(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'`' | b',' | b';')
}

/// A token of `what`: `prefix`, then `min` to `max` bytes `allowed` takes.
const fn pattern(
    what: &'static str,
    prefix: &'static str,
    allowed: fn(u8) -> bool,
    min: usize,
    max: usize,
) -> Pattern {
    Pattern {
        what,
        prefix,
        allowed,
        min,
        max,
    }
}

const PATTERNS: &[Pattern] = &[
    pattern("AWS access key", "AKIA", upper_digit, 16, 16),
    pattern("AWS access key", "ASIA", upper_digit, 16, 16),
    pattern("GitHub token", "ghp_", alnum, 36, 255),
    pattern("GitHub token", "gho_", alnum, 36, 255),
    pattern("GitHub token", "ghu_", alnum, 36, 255),
    pattern("GitHub token", "ghs_", alnum, 36, 255),
    pattern("GitHub token", "ghr_", alnum, 36, 255),
    pattern("GitHub token", "github_pat_", word, 40, 255),
    pattern("GitLab token", "glpat-", dashed, 20, 255),
    pattern("Slack token", "xoxb-", dashed, 20, 255),
    pattern("Slack token", "xoxp-", dashed, 20, 255),
    pattern("Slack token", "xoxa-", dashed, 20, 255),
    pattern("Slack token", "xoxr-", dashed, 20, 255),
    pattern("Slack token", "xoxs-", dashed, 20, 255),
    pattern("Slack app token", "xapp-", dashed, 20, 255),
    pattern("Stripe key", "sk_live_", alnum, 20, 255),
    pattern("Stripe key", "rk_live_", alnum, 20, 255),
    pattern("Anthropic API key", "sk-ant-", dashed, 30, 255),
    pattern("OpenAI API key", "sk-proj-", dashed, 20, 255),
    pattern("OpenAI API key", "sk-svcacct-", dashed, 20, 255),
    pattern("OpenAI API key", "sk-admin-", dashed, 20, 255),
    pattern("OpenAI API key", "sk-", alnum, 40, 255),
    pattern("Google API key", "AIza", dashed, 35, 35),
    pattern("npm token", "npm_", alnum, 36, 36),
    pattern("Hugging Face token", "hf_", alnum, 30, 40),
    pattern("Recall authkey", "recall-ak-", alnum, 20, 255),
    pattern("Recall enrolment key", "recall-ek-", keyish, 16, 255),
    pattern("Recall recovery key", "recall-rk-", keyish, 16, 255),
];

/// A value assigned to a name that says it is a secret: `names`, then `:`
/// or `=`, then at least `min` bytes `value` takes, that `plausible`
/// accepts.
struct Named {
    what: &'static str,
    names: &'static [&'static str],
    value: fn(u8) -> bool,
    min: usize,
    plausible: fn(&str) -> bool,
}

fn any_value(_: &str) -> bool {
    true
}

/// Whether what follows `password:` looks like a password rather than a
/// word about one: long enough, of more than one kind of character, and
/// not a placeholder, a path, or the name of where it is kept.
fn plausible_password(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let starts_odd = value.starts_with(['$', '<', '{', '*', '%', '(', '[', '/', '~', '.']);
    let names_a_vault = [
        "1password",
        "bitwarden",
        "keychain",
        "lastpass",
        "keepass",
        "vault",
        "redacted",
        "secret",
    ]
    .iter()
    .any(|v| lower.contains(v));
    let letters = value.bytes().any(|b| b.is_ascii_alphabetic());
    let others = value.bytes().any(|b| !b.is_ascii_alphabetic());
    (8..=128).contains(&value.len())
        && !starts_odd
        && !names_a_vault
        && !lower.contains("://")
        && letters
        && others
}

const NAMED: &[Named] = &[
    Named {
        what: "secret value",
        names: &[
            "secret",
            "token",
            "password",
            "passwd",
            "api_key",
            "apikey",
            "api-key",
            "private_key",
            "access_key",
        ],
        value: hex,
        min: 32,
        plausible: any_value,
    },
    Named {
        what: "AWS secret access key",
        names: &[
            "aws_secret_access_key",
            "secret_access_key",
            "aws_secret_key",
        ],
        value: base64ish,
        min: 40,
        plausible: any_value,
    },
    Named {
        what: "password",
        names: &["password", "passwd", "passphrase"],
        value: unspaced,
        min: 8,
        plausible: plausible_password,
    },
];

/// How far after a name its `:` or `=` may be: a closing quote and some
/// space, and no further, so a name mentioned in prose is not read as an
/// assignment made halfway along the line.
const ASSIGN_WINDOW: usize = 4;

/// Every token in `line`: where it starts and ends, and what it is.
///
/// Linear in the line's length: every search moves on past what it has
/// already read, so a line built to make a scan go back over itself (a
/// prefix repeated inside what it allows, over and over) costs no more
/// than any other line of its length.
pub(crate) fn tokens_in(line: &str) -> Vec<(usize, usize, &'static str)> {
    let bytes = line.as_bytes();
    let mut out: Vec<(usize, usize, &'static str)> = Vec::new();
    let run = |from: usize, allowed: fn(u8) -> bool| {
        bytes[from..]
            .iter()
            .position(|b| !allowed(*b))
            .map_or(bytes.len(), |n| from + n)
    };
    for p in PATTERNS {
        let mut from = 0;
        while let Some(at) = line[from..].find(p.prefix).map(|i| from + i) {
            from = at + p.prefix.len();
            if at > 0 && dashed(bytes[at - 1]) {
                continue;
            }
            let end = run(from, p.allowed);
            let len = end - from;
            // Past the run this read: nothing inside it is read again.
            from = from.max(end);
            // A token ends where the word does: a longer run of the same
            // letters is something else.
            let ends_clean = end == bytes.len() || !word(bytes[end]);
            if (p.min..=p.max).contains(&len) && ends_clean {
                out.push((at, end, p.what));
            }
        }
    }
    // A JSON Web Token: three base64url parts, the first a JSON object.
    let mut from = 0;
    while let Some(at) = line[from..].find("eyJ").map(|i| from + i) {
        from = at + 3;
        if at > 0 && dashed(bytes[at - 1]) {
            continue;
        }
        let mut end = at;
        let mut parts = 0;
        loop {
            let next = run(end, dashed);
            if next - end < 10 {
                break;
            }
            parts += 1;
            end = next;
            if parts == 3 || bytes.get(end) != Some(&b'.') {
                break;
            }
            end += 1;
        }
        from = from.max(end);
        if parts == 3 {
            out.push((at, end, "JSON Web Token"));
        }
    }
    // A value assigned to a name that says it is a secret: every such
    // name on the line, not only the first.
    let lower = line.to_ascii_lowercase();
    for named in NAMED {
        for name in named.names {
            let mut from = 0;
            while let Some(at) = lower[from..].find(name).map(|i| from + i) {
                from = at + name.len();
                let mut i = from;
                while i < bytes.len()
                    && i < from + ASSIGN_WINDOW
                    && matches!(bytes[i], b' ' | b'\t' | b'"' | b'\'')
                {
                    i += 1;
                }
                if !matches!(bytes.get(i), Some(b':' | b'=')) {
                    continue;
                }
                let mut start = i + 1;
                while start < bytes.len()
                    && matches!(bytes[start], b' ' | b'\t' | b'"' | b'\'' | b'`')
                {
                    start += 1;
                }
                let end = run(start, named.value);
                from = from.max(end);
                let ends_clean = end == bytes.len() || !word(bytes[end]);
                if end - start >= named.min && ends_clean && (named.plausible)(&line[start..end]) {
                    out.push((start, end, named.what));
                }
            }
        }
    }
    // A password in a URL: `scheme://user:password@host`.
    let mut from = 0;
    while let Some(at) = line[from..].find("://").map(|i| from + i) {
        let start = at + 3;
        let end = run(start, |b| {
            b.is_ascii_graphic()
                && !matches!(b, b'/' | b'?' | b'#' | b'@' | b'"' | b'\'' | b'<' | b'>')
        });
        from = end.max(start);
        if bytes.get(end) != Some(&b'@') {
            continue;
        }
        let Some(colon) = line[start..end].find(':').map(|i| start + i) else {
            continue;
        };
        let password = &line[colon + 1..end];
        if !password.is_empty() && !password.starts_with(['$', '<', '{', '*', '%']) {
            out.push((colon + 1, end, "password in a URL"));
        }
    }
    // A private key on one line: its `-----BEGIN … PRIVATE KEY-----`
    // header with the body after it on the same line, its line breaks
    // written `\n` as a JSON string holds them (a cloud service account's
    // `"private_key"`), up to its `-----END …-----` or the end of the body.
    let last_end = line.rfind("-----END ");
    let mut from = 0;
    while let Some(header_end) = key_header_end(&line[from..]).map(|e| from + e) {
        let at = line[from..header_end]
            .rfind("-----BEGIN ")
            .map_or(from, |i| from + i);
        from = header_end;
        let key_text = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\\');
        let body_end = run(header_end, key_text);
        let end = match last_end.filter(|e| *e >= header_end) {
            // The body runs, key text and nothing else, up to the `-----END`:
            // prose that names both markers on one line is not a key.
            Some(_) if line[body_end..].starts_with("-----END ") => line[body_end + 9..]
                .find("-----")
                .map_or(bytes.len(), |i| body_end + 9 + i + 5),
            _ => body_end,
        };
        if body_end >= header_end + 16 {
            out.push((at, end, KEY_WHAT));
            from = end;
        }
    }
    out.sort();
    // Overlaps, such as `sk-proj-` inside a match of `sk-`, count once.
    let mut kept: Vec<(usize, usize, &'static str)> = Vec::new();
    for t in out {
        match kept.last_mut() {
            Some(last) if t.0 < last.1 => last.1 = last.1.max(t.1),
            _ => kept.push(t),
        }
    }
    kept
}

/// What a private key is called, as a token and as a finding.
const KEY_WHAT: &str = "private key";

/// The public prefix a token of `what` starts with, when its kind has one
/// (`ghp_`, `sk-proj-`, `AKIA`, `eyJ`): the longest that fits. [`None`]
/// for a value that is secret from its first character: a password, an
/// AWS secret access key, a hex secret, a URL's password, a private key.
fn public_prefix(token: &str, what: &str) -> Option<&'static str> {
    if what == "JSON Web Token" {
        return Some("eyJ");
    }
    PATTERNS
        .iter()
        .filter(|p| p.what == what && token.starts_with(p.prefix))
        .map(|p| p.prefix)
        .max_by_key(|prefix| prefix.len())
}

/// `token`, a `what`, masked: its length, and its public prefix when its
/// kind has one. Nothing of the secret itself.
pub(crate) fn mask(token: &str, what: &str) -> String {
    let count = token.chars().count();
    match public_prefix(token, what) {
        Some(prefix) => format!("{prefix}… ({count} characters, masked)"),
        None => format!("[{what}, {count} characters, masked]"),
    }
}

/// `line` with each token replaced by `with(token, what)`.
pub(crate) fn replace_tokens(
    line: &str,
    tokens: &[(usize, usize, &str)],
    with: impl Fn(&str, &str) -> String,
) -> String {
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    for (start, end, what) in tokens {
        out.push_str(&line[at..*start]);
        out.push_str(&with(&line[*start..*end], what));
        at = *end;
    }
    out.push_str(&line[at..]);
    out
}

/// Where the first `-----BEGIN … PRIVATE KEY-----` header in `text` ends,
/// wherever on the line it is: after a `> ` or a list marker, inside a
/// JSON string.
fn key_header_end(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find("-----BEGIN ").map(|i| from + i) {
        let name = at + 11;
        let close = text[name..].find("-----").map(|i| name + i)?;
        if text[name..close].ends_with("PRIVATE KEY") {
            return Some(close + 5);
        }
        from = close;
    }
    None
}

/// `line` without the quote markers and list marker before its text.
fn unquoted(line: &str) -> &str {
    let mut t = line.trim_start();
    while let Some(rest) = t.strip_prefix('>') {
        t = rest.trim_start();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    t
}

/// Whether `line` opens a private key block: a `-----BEGIN … PRIVATE
/// KEY-----` header anywhere on it, with nothing but quoting after it (a
/// key on one line is a token instead: see [`tokens_in`]).
fn opens_key(line: &str) -> bool {
    key_header_end(line).is_some_and(|end| {
        line[end..]
            .trim()
            .trim_matches(['"', '\'', ',', '`'])
            .is_empty()
    })
}

/// Whether `line` closes one.
pub(crate) fn closes_key(line: &str) -> bool {
    line.contains("-----END ") && line.contains("PRIVATE KEY")
}

/// Whether `line` could be a line of a key block's body: base64 and
/// nothing else, once its quote or list marker is off, long enough not to
/// be a word.
fn key_body(line: &str) -> bool {
    let t = unquoted(line).trim();
    t.len() >= 16
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// The shortest string [`Redactor`] masks wherever it appears: a token
/// found as a secret in one place is masked in any other text only if it
/// is at least this long, since a shorter one (a one-letter password in a
/// URL) would be masked in every word that holds it. It is still masked
/// wherever [`tokens_in`] finds it in its own right.
const MIN_KNOWN_BYTES: usize = 8;

/// The longest prefix a known secret is looked up by.
const ANCHOR_BYTES: usize = 16;

/// What masks every secret in memory wherever a report quotes it, not
/// only in the `secret` finding that names it: a duplicate, a stale note,
/// a note in the wrong scope or a contradiction may quote the same line.
///
/// Built from every file an evaluation reads: each token `tokens_in`
/// finds, and each line of a private key's body. Every string that goes
/// into `details`, and every note handed to `claude`, passes through
/// [`Redactor::text`], which replaces each of them, and any token it finds
/// itself, with a mask.
///
/// Near-linear in the text, however many secrets it knows: each is looked
/// up by its first `ANCHOR_BYTES` bytes (all of it, when shorter), so
/// each position of the text costs a few hash lookups rather than one
/// comparison per secret. The longest secret starting at a position wins.
#[derive(Debug, Default)]
pub struct Redactor {
    /// Every known secret with what it becomes, by the prefix it is looked
    /// up by; within a prefix, longest first.
    known: HashMap<Vec<u8>, Vec<(String, String)>>,
    /// The prefix lengths in `known`, longest first.
    anchors: Vec<usize>,
    /// Bytes of notes masked for the contradiction prompt, so a test can
    /// hold that to once per file per run. Only the evaluation's
    /// contradiction check (behind `client`) reads this.
    #[cfg(feature = "client")]
    pub(crate) prompt_bytes: std::sync::atomic::AtomicUsize,
}

/// What a line of a private key's body becomes.
const KEY_LINE_MASK: &str = "[a line of a private key, masked]";

impl Redactor {
    /// Learns every secret in `files`: each token `tokens_in` finds, and
    /// each line of a private key's body.
    pub fn new(files: &[EvaluateFile]) -> Self {
        let mut found: HashMap<String, String> = HashMap::new();
        for file in files {
            let lines: Vec<&str> = file.content.lines().collect();
            let mut in_key = false;
            for (n, line) in lines.iter().enumerate() {
                for (start, end, what) in tokens_in(line) {
                    let token = &line[start..end];
                    found
                        .entry(token.to_string())
                        .or_insert_with(|| mask(token, what));
                }
                if opens_key_block(&lines, n) {
                    in_key = true;
                    continue;
                }
                if in_key {
                    if closes_key(line) || !key_body(line) {
                        in_key = false;
                    } else {
                        found.insert(unquoted(line).trim().to_string(), KEY_LINE_MASK.into());
                    }
                }
            }
        }
        let mut known: HashMap<Vec<u8>, Vec<(String, String)>> = HashMap::new();
        for (secret, masked) in found {
            if secret.len() < MIN_KNOWN_BYTES {
                continue;
            }
            let anchor = secret.as_bytes()[..secret.len().min(ANCHOR_BYTES)].to_vec();
            known.entry(anchor).or_default().push((secret, masked));
        }
        for bucket in known.values_mut() {
            bucket.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.cmp(b)));
        }
        let mut anchors: Vec<usize> = known.keys().map(Vec::len).collect();
        anchors.sort_unstable_by(|a, b| b.cmp(a));
        anchors.dedup();
        Self {
            known,
            anchors,
            #[cfg(feature = "client")]
            prompt_bytes: Default::default(),
        }
    }

    /// The longest known secret at the start of `bytes`: its length and
    /// what it becomes.
    fn known_at(&self, bytes: &[u8]) -> Option<(usize, &str)> {
        let mut best: Option<(usize, &str)> = None;
        for &anchor in &self.anchors {
            let Some(prefix) = bytes.get(..anchor) else {
                continue;
            };
            let Some(bucket) = self.known.get(prefix) else {
                continue;
            };
            if let Some((secret, masked)) = bucket
                .iter()
                .find(|(secret, _)| bytes.starts_with(secret.as_bytes()))
            {
                if best.is_none_or(|(len, _)| secret.len() > len) {
                    best = Some((secret.len(), masked));
                }
            }
        }
        best
    }

    /// `text` with every secret this knows of, and every token it finds,
    /// masked, and every line of a private key's body replaced.
    pub fn text(&self, text: &str) -> String {
        // Every known secret, in one pass.
        let bytes = text.as_bytes();
        let mut known = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            // A secret is text, so starts where a character does.
            if text.is_char_boundary(i) {
                if let Some((len, masked)) = self.known_at(&bytes[i..]) {
                    known.extend_from_slice(masked.as_bytes());
                    i += len;
                    continue;
                }
            }
            known.push(bytes[i]);
            i += 1;
        }
        let known = String::from_utf8(known).expect("whole secrets replaced by text");
        // Then any token of its own on each line.
        let mut out = String::with_capacity(known.len());
        for line in known.split_inclusive('\n') {
            let body = line.trim_end_matches(['\n', '\r']);
            let ending = &line[body.len()..];
            let found = tokens_in(body);
            out.push_str(&replace_tokens(body, &found, mask));
            out.push_str(ending);
        }
        out
    }

    /// A note's content as the contradiction prompt holds it: masked, and
    /// counted. Only the evaluation's contradiction check (behind
    /// `client`) calls this.
    #[cfg(feature = "client")]
    pub(crate) fn prompt_text(&self, content: &str) -> String {
        self.prompt_bytes
            .fetch_add(content.len(), std::sync::atomic::Ordering::Relaxed);
        self.text(content)
    }
}

/// Whether line `n` of `lines` opens a private key block: a key header
/// with nothing after it, and the next line a line of a key's body. Prose
/// that ends with a header, or a list of headers, is not a key.
pub(crate) fn opens_key_block(lines: &[&str], n: usize) -> bool {
    opens_key(lines[n]) && lines.get(n + 1).is_some_and(|next| key_body(next))
}
