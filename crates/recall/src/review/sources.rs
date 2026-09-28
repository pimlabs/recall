//! Layer 2's sources beyond the checkout and git: what this machine says
//! about itself (`status::Report`, collected once), what its server says it
//! is (`GET /health` and `GET /.well-known/recall`, asked once, inside that
//! collection), what the project's compose files carry, and a short list of
//! environment variables. See `docs/design/memory-truth.md`'s Layer 2 table.
//!
//! Everything is read first, into [`Facts`], and judged after: the judging
//! below is pure, so the tests hand it facts rather than a machine.
//!
//! **What leaves this machine.** Only what `recall status` already sends,
//! and to the server this machine is configured for: `/health` and the
//! discovery document, with no pull, no enrolment check and no audit
//! witness check. A host a note names is asked anything only with
//! `--probe-hosts`, and then only `GET /.well-known/recall`, carrying no
//! credential (design decision 3): a note's text must not decide by default
//! where this machine sends requests.
//!
//! **What is never read.** A variable's value, unless it is one of
//! [`VALUE_VARS`]: the ones `recall status` already prints. A token named
//! in a note is judged by its name alone, and its value never reaches the
//! report.

use std::collections::BTreeMap;
use std::path::Path;

use recall_hooks::config::Source;

use super::{observed, Class, Observed, Signal};

/// Variables whose *value* the review may read and compare: the non-secret
/// ones `recall status` already reports, and nothing else.
pub(super) const VALUE_VARS: &[&str] = &[
    "HOME",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_REMOTE_MEMORY_DIR",
];

/// How long the review waits on the configured server, in all. Far below
/// `recall status`'s: a review asks two unauthenticated questions, and a
/// server that cannot answer them this fast is reported as unavailable,
/// not waited on.
pub(super) const SERVER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// What the configured server said, or why it said nothing.
#[derive(Debug, Clone, Default)]
pub(super) struct Server {
    /// `RECALL_URL` in effect, normalised; [`None`] when none is configured.
    pub url: Option<String>,
    /// Where that URL came from, as a person would say it.
    pub url_source: &'static str,
    /// The URL's host, lower-cased, without a port.
    pub host: Option<String>,
    /// Whether `GET /health` answered.
    pub answered: bool,
    /// Why it did not, when it did not.
    pub error: Option<String>,
    /// The commit `/health` reports.
    pub commit: Option<String>,
    /// The version the discovery document reports.
    pub version: Option<String>,
    /// The capabilities the discovery document lists, by name.
    pub capabilities: Vec<String>,
}

/// What `deploy/docker-compose*.yml` carry, read as YAML.
#[derive(Debug, Clone, Default)]
pub(super) struct Compose {
    /// The files read, relative to the repository root.
    pub files: Vec<String>,
    /// Every name a file gives a service, container, image or label
    /// namespace, with where it was found, as a sentence.
    pub names: BTreeMap<String, String>,
    /// Every variable a service's `environment` passes, with the file.
    pub vars: BTreeMap<String, String>,
}

/// What `--probe-hosts` found at one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Probe {
    /// It served a discovery document: a Recall server, at this version.
    Recall(String),
    /// It did not, for this reason.
    NotRecall(String),
}

/// Everything layer 2 reads beyond the checkout, read once per run.
#[derive(Debug, Clone, Default)]
pub(super) struct Facts {
    /// The configured server.
    pub server: Server,
    /// Whether this is a cloud session, per `CLAUDE_CODE_REMOTE`.
    pub remote_session: bool,
    /// [`VALUE_VARS`], as a hook here would see them.
    pub env: Vec<(&'static str, Option<String>)>,
    /// Every variable the `recall` client reads, by name.
    pub known_vars: Vec<&'static str>,
    /// The project's compose files, when the checkout has any.
    pub compose: Option<Compose>,
    /// What each probed host answered. Empty without `--probe-hosts`.
    pub probes: BTreeMap<String, Probe>,
}

impl Facts {
    /// What these facts say, one sentence each, for layer 3's fact sheet:
    /// facts that can be cited, then context that cannot (a server that did
    /// not answer says nothing about what the server is). Only what the
    /// report may already show: the configured server as `recall status`
    /// prints it, the kind of session, the values of [`VALUE_VARS`], the
    /// compose files' names, and what probes found. One fact per sentence,
    /// so a changed commit does not unseat an answer that cited the URL.
    pub fn sheet(&self) -> (Vec<String>, Vec<String>) {
        let mut out = Vec::new();
        let mut context = Vec::new();
        let s = &self.server;
        match &s.url {
            None => out.push(
                "No Recall server is configured on this machine (RECALL_URL is unset)".into(),
            ),
            Some(url) => {
                out.push(format!(
                    "This machine's configured Recall server is {url} (RECALL_URL, from {})",
                    s.url_source
                ));
                if s.answered {
                    out.push(format!("/health answers at {url}"));
                    if let Some(c) = &s.commit {
                        out.push(format!("The configured server is built from commit {c}"));
                    }
                    if let Some(v) = &s.version {
                        out.push(format!("The configured server reports version {v}"));
                    }
                    if !s.capabilities.is_empty() {
                        out.push(format!(
                            "The configured server lists the capabilities {}",
                            s.capabilities.join(", ")
                        ));
                    }
                } else {
                    context.push(format!(
                        "/health at {url} did not answer this run ({})",
                        s.error.as_deref().unwrap_or("no reason given")
                    ));
                }
            }
        }
        out.push(if self.remote_session {
            "This check runs in a Claude Code cloud session (CLAUDE_CODE_REMOTE=true)".into()
        } else {
            "This check does not run in a Claude Code cloud session (CLAUDE_CODE_REMOTE is not \
             true)"
                .into()
        });
        for (name, value) in &self.env {
            if *name == "CLAUDE_CODE_REMOTE" {
                continue;
            }
            out.push(match value {
                Some(v) => format!("{name} is {v} on this machine"),
                None => format!("{name} is not set on this machine"),
            });
        }
        if let Some(c) = &self.compose {
            let names: Vec<&str> = c.names.keys().map(String::as_str).collect();
            out.push(format!(
                "The project's compose files ({}) name: {}",
                c.files.join(", "),
                names.join(", ")
            ));
            if !c.vars.is_empty() {
                let vars: Vec<&str> = c.vars.keys().map(String::as_str).collect();
                out.push(format!(
                    "The project's compose files pass these variables to a service: {}",
                    vars.join(", ")
                ));
            }
        }
        for (host, probe) in &self.probes {
            out.push(match probe {
                Probe::Recall(v) => format!("{host} answers as a Recall server, version {v}"),
                Probe::NotRecall(why) => {
                    format!("{host} does not answer as a Recall server ({why})")
                }
            });
        }
        (out, context)
    }

    fn env(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, v)| v.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn said(source: Source) -> &'static str {
    match source {
        Source::Environment => "the environment",
        Source::CredentialsFile => "credentials.toml",
        Source::ConfigFile => "config.toml",
        Source::Unset => "unset",
    }
}

/// Collects `status::Report` once, as `recall status` would but asking the
/// server only what it is, and reads what the review needs out of it.
pub(super) async fn read(
    here: &crate::project::Resolved,
    cfg: &recall_hooks::ClientConfig,
    git_root: Option<&Path>,
) -> Facts {
    let rep = crate::status::collect_for_review(here, cfg, SERVER_DEADLINE).await;
    let url = (!cfg.url.is_empty()).then(|| recall_hooks::home::normalize_url(&cfg.url));
    let server = Server {
        host: url.as_deref().map(host_of),
        url,
        url_source: said(rep.url_source),
        answered: rep.server_ok,
        error: rep.server_error.clone(),
        commit: rep.git_commit.clone(),
        version: rep.server_version.clone(),
        capabilities: rep
            .discovery
            .as_ref()
            .map(|d| d.capabilities.keys().cloned().collect())
            .unwrap_or_default(),
    };
    Facts {
        server,
        remote_session: rep.remote_session,
        env: VALUE_VARS
            .iter()
            .map(|name| (*name, here.env.get(name).filter(|v| !v.is_empty())))
            .collect(),
        known_vars: crate::status::known_vars(),
        compose: git_root.and_then(read_compose),
        probes: BTreeMap::new(),
    }
}

/// Reads every `deploy/docker-compose*.yml` in the checkout. [`None`] when
/// there are none, or none parse.
pub(super) fn read_compose(root: &Path) -> Option<Compose> {
    let dir = root.join("deploy");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.starts_with("docker-compose") && (n.ends_with(".yml") || n.ends_with(".yaml"))
        })
        .collect();
    paths.sort();
    let mut compose = Compose::default();
    for name in paths {
        let Ok(text) = std::fs::read_to_string(dir.join(&name)) else {
            continue;
        };
        let rel = format!("deploy/{name}");
        if read_compose_file(&text, &rel, &mut compose) {
            compose.files.push(rel);
        }
    }
    (!compose.files.is_empty()).then_some(compose)
}

/// Reads one compose file into `into`; whether it parsed as one.
fn read_compose_file(text: &str, file: &str, into: &mut Compose) -> bool {
    use yaml_rust2::{Yaml, YamlLoader};
    let Ok(docs) = YamlLoader::load_from_str(text) else {
        return false;
    };
    let Some(Yaml::Hash(services)) = docs.first().map(|d| &d["services"]) else {
        return false;
    };
    let mut name = |key: &str, what: String| {
        into.names.entry(key.to_string()).or_insert(what);
    };
    for (service, body) in services {
        let Some(service) = service.as_str() else {
            continue;
        };
        name(service, format!("`{service}` is a service in {file}"));
        if let Some(container) = body["container_name"].as_str() {
            name(
                container,
                format!("`{container}` is the container of {file}'s `{service}` service"),
            );
        }
        if let Some(image) = body["image"].as_str() {
            let what = format!("`{service}` in {file} runs the image `{image}`");
            name(image, what.clone());
            // `cloudflare/cloudflared:latest` is what a note calls
            // `cloudflared`: the repository's last segment, without a tag.
            let bare = image.rsplit('/').next().unwrap_or(image);
            let bare = bare.split([':', '@']).next().unwrap_or(bare);
            name(bare, what);
        }
        // `traefik.http.routers.recall.rule` says the service is routed by
        // Traefik: the label namespace is the ingress's name.
        let labels: Vec<String> = match &body["labels"] {
            Yaml::Hash(h) => h
                .keys()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect(),
            Yaml::Array(a) => a
                .iter()
                .filter_map(|l| l.as_str())
                .map(|l| l.split('=').next().unwrap_or(l).to_string())
                .collect(),
            _ => Vec::new(),
        };
        for label in labels {
            if let Some((ns, _)) = label.split_once('.') {
                name(
                    ns,
                    format!("{file}'s `{service}` service carries `{ns}.*` labels"),
                );
            }
        }
        let vars: Vec<String> = match &body["environment"] {
            Yaml::Hash(h) => h
                .keys()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect(),
            Yaml::Array(a) => a
                .iter()
                .filter_map(|l| l.as_str())
                .map(|l| l.split('=').next().unwrap_or(l).to_string())
                .collect(),
            _ => Vec::new(),
        };
        for var in vars {
            into.vars
                .entry(var)
                .or_insert_with(|| format!("{file}'s `{service}` service"));
        }
    }
    true
}

/// Asks each of `hosts` for its discovery document, anonymously, and only
/// that. Called only with `--probe-hosts`.
pub(super) async fn probe(hosts: &[String]) -> BTreeMap<String, Probe> {
    let mut out = BTreeMap::new();
    for host in hosts {
        let found = match recall_hooks::client::Client::new(&format!("https://{host}"), "") {
            Err(e) => Probe::NotRecall(e.to_string()),
            Ok(client) => match client.discover().await {
                Ok(Some(doc)) => Probe::Recall(doc.server.version),
                Ok(None) => Probe::NotRecall("it has no /.well-known/recall".to_string()),
                Err(e) => Probe::NotRecall(e.to_string()),
            },
        };
        out.insert(host.clone(), found);
    }
    out
}

// ---------------------------------------------------------------------------
// Judging
// ---------------------------------------------------------------------------

/// The host a hostname or URL anchor names: lower-cased, without scheme,
/// credentials, port or path.
pub(super) fn host_of(value: &str) -> String {
    let rest = value.split_once("://").map_or(value, |(_, r)| r);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let rest = match rest.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => rest,
    };
    rest.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether the anchor names a place (a host, or a URL with nothing after
/// the host) rather than something at one: `https://x/install.sh` is a
/// claim about a file, and says nothing about where the server is.
fn names_a_place(value: &str) -> bool {
    let rest = value.split_once("://").map_or(value, |(_, r)| r);
    match rest.split_once('/') {
        None => true,
        Some((_, path)) => path.is_empty(),
    }
}

/// Whether two hosts belong to the same site: the same last two labels.
/// `recall.pimlabs.id` and `recall-server.pimlabs.id` do; an IP address
/// belongs to no site but itself.
fn same_site(a: &str, b: &str) -> bool {
    let is_ip = |h: &str| h.chars().all(|c| c.is_ascii_digit() || c == '.');
    if is_ip(a) || is_ip(b) {
        return a == b;
    }
    let site = |h: &str| {
        let labels: Vec<&str> = h.split('.').collect();
        labels[labels.len().saturating_sub(2)..].join(".")
    };
    site(a) == site(b)
}

/// Whether a claim naming `host` is a claim about where this machine's
/// Recall server is: it names a place on the configured server's own site,
/// or says `RECALL_URL`. Any other host is someone else's, and the review
/// has no business calling it wrong.
fn about_the_server(value: &str, host: &str, claim: &str, facts: &Facts) -> bool {
    let Some(configured) = facts.server.host.as_deref() else {
        return false;
    };
    names_a_place(value) && (same_site(host, configured) || claim.contains("RECALL_URL"))
}

/// The hosts `--probe-hosts` would ask: named by a `present` or `rule`
/// claim, not the configured server, and each once. Empty without the
/// flag, whatever memory names.
pub(super) fn hosts_to_probe<'a>(
    claims: impl IntoIterator<Item = (Class, &'a str)>,
    facts: &Facts,
    probe_hosts: bool,
) -> Vec<String> {
    if !probe_hosts {
        return Vec::new();
    }
    let mut out: Vec<String> = claims
        .into_iter()
        .filter(|(class, _)| matches!(class, Class::Present | Class::Rule))
        .flat_map(|(_, text)| super::anchors(text))
        .filter(|a| a.kind == super::AnchorKind::Hostname)
        .map(|a| host_of(&a.value))
        .filter(|h| Some(h.as_str()) != facts.server.host.as_deref())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn configured(server: &Server) -> String {
    format!(
        "RECALL_URL ({}) is {}",
        server.url_source,
        server.url.as_deref().unwrap_or_default()
    )
}

/// A hostname or URL anchor, against the configured server and, with
/// `--probe-hosts`, what the named host itself answered.
pub(super) fn evidence_for_host(value: &str, claim: &str, facts: &Facts) -> Observed {
    let host = host_of(value);
    let server = &facts.server;
    let Some(configured_host) = server.host.as_deref() else {
        return observed(
            Signal::Unknown,
            "server",
            format!("no server is configured on this machine to compare `{host}` with"),
        );
    };
    let unanswered = || {
        format!(
            "{}, but /health did not answer there ({})",
            configured(server),
            server.error.as_deref().unwrap_or("no reason given")
        )
    };
    if host == configured_host {
        if !server.answered {
            return observed(Signal::Unknown, "server", unanswered());
        }
        let commit = server
            .commit
            .as_deref()
            .map(|c| format!(", commit {c}"))
            .unwrap_or_default();
        return observed(
            Signal::Confirms,
            "server",
            format!("{}; /health answers there{commit}", configured(server)),
        );
    }
    let about = about_the_server(value, &host, claim, facts);
    match facts.probes.get(&host) {
        Some(Probe::Recall(version)) => observed(
            Signal::Unknown,
            "server",
            format!(
                "`{host}` answers as a Recall server too (version {version}), but {}",
                configured(server)
            ),
        ),
        Some(Probe::NotRecall(why)) if about && server.answered => observed(
            Signal::Contradicts,
            "server",
            format!(
                "`{host}` does not answer as a Recall server ({why}); {}, and /health answers \
                 there",
                configured(server)
            ),
        ),
        Some(Probe::NotRecall(why)) => observed(
            Signal::Unknown,
            "server",
            format!("`{host}` does not answer as a Recall server ({why})"),
        ),
        None if about && server.answered => observed(
            Signal::Contradicts,
            "server",
            format!("{}; /health answers there", configured(server)),
        ),
        None if about => observed(Signal::Unknown, "server", unanswered()),
        None => observed(
            Signal::Unknown,
            "server",
            format!(
                "`{host}` is not this machine's server; only --probe-hosts asks another host \
                 anything"
            ),
        ),
    }
}

/// Which kind of machine a claim about the environment is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cloud,
    Local,
    Unstated,
}

fn kind_of(name: &str, claim: &str) -> Kind {
    if name.starts_with("CLAUDE_CODE_REMOTE") {
        return Kind::Cloud;
    }
    let lower = claim.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |w: &str| words.contains(&w);
    if has("cloud") || lower.contains("remote session") {
        Kind::Cloud
    } else if has("laptop") || has("mac") || has("macbook") || has("desktop") || has("locally") {
        Kind::Local
    } else {
        Kind::Unstated
    }
}

/// `CLAUDE_CODE_REMOTE=true, HOME=/root`: the rest of [`VALUE_VARS`] as set
/// here, so an environment verdict says which machine it was read on.
fn context(facts: &Facts, except: &str) -> String {
    let set: Vec<String> = facts
        .env
        .iter()
        .filter(|(n, _)| *n != except)
        .filter_map(|(n, v)| v.as_deref().map(|v| format!("{n}={v}")))
        .collect();
    if set.is_empty() {
        String::new()
    } else {
        format!("; {}", set.join(", "))
    }
}

/// A `NAME=value` anchor for one of [`VALUE_VARS`], or [`None`] for any
/// other: those are judged by name, and their value is never read.
pub(super) fn evidence_for_env_value(value: &str, claim: &str, facts: &Facts) -> Option<Observed> {
    let (name, claimed) = value.split_once('=')?;
    if claimed.is_empty() || !VALUE_VARS.contains(&name) {
        return None;
    }
    let here_is = if facts.remote_session {
        "this is a cloud session"
    } else {
        "this is not a cloud session"
    };
    match (kind_of(name, claim), facts.remote_session) {
        (Kind::Cloud, false) => {
            return Some(observed(
                Signal::Unknown,
                "environment",
                format!("the claim is about a cloud session, and {here_is}"),
            ))
        }
        (Kind::Local, true) => {
            return Some(observed(
                Signal::Unknown,
                "environment",
                format!("the claim is about a local machine, and {here_is}"),
            ))
        }
        (Kind::Unstated, _) => {
            return Some(observed(
                Signal::Unknown,
                "environment",
                format!(
                    "`{name}` differs from machine to machine, and the claim does not say which \
                     kind it is about"
                ),
            ))
        }
        _ => {}
    }
    let context = context(facts, name);
    Some(match facts.env(name) {
        Some(actual) if actual == claimed => observed(
            Signal::Confirms,
            "environment",
            format!("`{name}` is set to {actual} here{context}"),
        ),
        Some(actual) => observed(
            Signal::Contradicts,
            "environment",
            format!("`{name}` is set to {actual} here, not {claimed}{context}"),
        ),
        None => observed(
            Signal::Contradicts,
            "environment",
            format!("`{name}` is not set here{context}"),
        ),
    })
}

/// A variable named without a value this review may read: whether the
/// client reads it, or the project's compose files pass it to the server.
/// [`None`] when neither says anything, for the caller to try the checkout.
pub(super) fn evidence_for_env_name(
    name: &str,
    facts: &Facts,
    is_project: bool,
) -> Option<Observed> {
    if facts.known_vars.contains(&name) {
        return Some(observed(
            Signal::Confirms,
            "environment",
            format!("`{name}` is a variable the recall client reads"),
        ));
    }
    if !is_project {
        return None;
    }
    let (_, service) = facts.compose.as_ref()?.vars.get_key_value(name)?;
    Some(observed(
        Signal::Confirms,
        "compose",
        format!("`{name}` is passed to {service}"),
    ))
}

/// Inline code that names a capability the server lists, or a service,
/// image, container or ingress the project's compose files carry. [`None`]
/// when neither does, for the caller to search the checkout.
pub(super) fn evidence_for_name(value: &str, facts: &Facts, is_project: bool) -> Option<Observed> {
    if facts.server.capabilities.iter().any(|c| c == value) {
        return Some(observed(
            Signal::Confirms,
            "server",
            format!("the server lists the `{value}` capability"),
        ));
    }
    if !is_project {
        return None;
    }
    let what = facts.compose.as_ref()?.names.get(value)?;
    Some(observed(Signal::Confirms, "compose", what.clone()))
}

/// A version in a claim about the server, against the version the server
/// reports. [`None`] when the claim is not about the server, or the server
/// did not say, for the caller to compare with the release tags instead.
pub(super) fn evidence_for_server_version(
    claimed: (u64, u64, u64),
    claim: &str,
    facts: &Facts,
) -> Option<Observed> {
    let lower = claim.to_ascii_lowercase();
    if !["server", "production", "deployed", "deploy"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return None;
    }
    let version = facts.server.version.as_deref()?;
    let reported = super::parse_semver(version)?;
    Some(if claimed == reported {
        observed(
            Signal::Confirms,
            "server",
            format!("the server reports version {version}"),
        )
    } else if super::asserts_current_version(claim)
        || [" runs ", " running ", " reports "]
            .iter()
            .any(|w| lower.contains(w))
    {
        observed(
            Signal::Contradicts,
            "server",
            format!("the server reports version {version}"),
        )
    } else {
        observed(
            Signal::Unknown,
            "server",
            format!("the server reports version {version}; the claim may be about another time"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine whose server is `recall-server.pimlabs.id`, saved by
    /// `recall connect`, answering.
    fn connected() -> Facts {
        Facts {
            server: Server {
                url: Some("https://recall-server.pimlabs.id".into()),
                url_source: "credentials.toml",
                host: Some("recall-server.pimlabs.id".into()),
                answered: true,
                commit: Some("8ce9fbd".into()),
                version: Some("0.4.8".into()),
                capabilities: vec!["evaluation".into(), "devices".into()],
                ..Default::default()
            },
            known_vars: crate::status::known_vars(),
            ..Default::default()
        }
    }

    /// The same machine as a cloud session: the variables a cloud
    /// environment sets, as the design's worked example has them.
    fn cloud() -> Facts {
        Facts {
            remote_session: true,
            env: vec![
                ("HOME", Some("/root".into())),
                ("CLAUDE_CODE_REMOTE", Some("true".into())),
                (
                    "CLAUDE_CODE_REMOTE_MEMORY_DIR",
                    Some("/home/user/.claude".into()),
                ),
            ],
            ..connected()
        }
    }

    /// The same machine as a laptop: no cloud variables, its own `HOME`.
    fn laptop() -> Facts {
        Facts {
            remote_session: false,
            env: vec![
                ("HOME", Some("/Users/eko".into())),
                ("CLAUDE_CODE_REMOTE", None),
                ("CLAUDE_CODE_REMOTE_MEMORY_DIR", None),
            ],
            ..connected()
        }
    }

    /// The design's L3: a note that puts the server where it used to be is
    /// stale, and says where it is now and that it answers there. Mutation:
    /// compare against nothing (every other host `cant_tell`).
    #[test]
    fn the_old_server_host_is_stale_against_the_configured_one() {
        let o = evidence_for_host(
            "recall.pimlabs.id",
            "Recall's server is live at `recall.pimlabs.id`.",
            &connected(),
        );
        assert_eq!(o.signal, Signal::Contradicts, "{}", o.detail);
        assert_eq!(
            o.detail,
            "RECALL_URL (credentials.toml) is https://recall-server.pimlabs.id; /health answers \
             there"
        );
    }

    #[test]
    fn the_configured_host_is_still_true_while_it_answers() {
        let facts = connected();
        let o = evidence_for_host("https://recall-server.pimlabs.id", "", &facts);
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        assert!(o.detail.contains("commit 8ce9fbd"), "{}", o.detail);

        let down = Facts {
            server: Server {
                answered: false,
                error: Some("connection refused".into()),
                ..facts.server.clone()
            },
            ..facts
        };
        let o = evidence_for_host("recall-server.pimlabs.id", "", &down);
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
        assert!(o.detail.contains("connection refused"), "{}", o.detail);
    }

    /// A host on another site is someone else's, and a URL with a path is a
    /// claim about a file: neither says where the server is, so neither is
    /// called wrong for not being it.
    #[test]
    fn another_sites_host_and_a_url_with_a_path_cant_tell() {
        let facts = connected();
        for (value, claim) in [
            ("github.com", "The code is on `github.com`."),
            (
                "https://recall.pimlabs.id/install.sh",
                "Install from `https://recall.pimlabs.id/install.sh`.",
            ),
        ] {
            let o = evidence_for_host(value, claim, &facts);
            assert_eq!(o.signal, Signal::Unknown, "{value}: {}", o.detail);
        }
    }

    /// With `--probe-hosts`, a host that still answers as a Recall server
    /// is not called wrong for not being this machine's; one that does not
    /// is, when the claim is about the server.
    #[test]
    fn a_probe_decides_between_another_recall_server_and_none() {
        let mut facts = connected();
        facts
            .probes
            .insert("recall.pimlabs.id".into(), Probe::Recall("0.4.8".into()));
        let o = evidence_for_host("recall.pimlabs.id", "live at recall.pimlabs.id", &facts);
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);

        facts.probes.insert(
            "recall.pimlabs.id".into(),
            Probe::NotRecall("404 Not Found".into()),
        );
        let o = evidence_for_host("recall.pimlabs.id", "live at recall.pimlabs.id", &facts);
        assert_eq!(o.signal, Signal::Contradicts, "{}", o.detail);
        assert!(o.detail.contains("404 Not Found"), "{}", o.detail);
    }

    /// Design test row: no request goes to a host other than the configured
    /// server without `--probe-hosts`. Mutation: probe every named host.
    #[test]
    fn nothing_is_probed_without_the_flag() {
        let facts = connected();
        let claims = [
            (Class::Present, "Live at `recall.pimlabs.id`."),
            (Class::Present, "Mirror at `backup.example.org`."),
        ];
        assert!(hosts_to_probe(claims, &facts, false).is_empty());
        assert_eq!(
            hosts_to_probe(claims, &facts, true),
            vec!["backup.example.org", "recall.pimlabs.id"]
        );
    }

    /// Even with the flag: never the configured server (already asked),
    /// never a host only a record names, and each host once.
    #[test]
    fn the_flag_never_probes_the_server_a_record_or_a_host_twice() {
        let facts = connected();
        let claims = [
            (Class::Present, "At `recall-server.pimlabs.id`."),
            (Class::Record, "Until 2026-09 it was at `old.pimlabs.id`."),
            (Class::Present, "`x.example.org` and https://x.example.org"),
        ];
        assert_eq!(hosts_to_probe(claims, &facts, true), vec!["x.example.org"]);
    }

    /// Design test row: the memory directory line is `still_true` in a
    /// cloud environment and `cant_tell` on a laptop. Mutation: decide
    /// environment claims on any machine.
    #[test]
    fn a_cloud_variable_is_decided_only_in_a_cloud_session() {
        let claim = "CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude";
        let o = evidence_for_env_value(claim, claim, &cloud()).unwrap();
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        assert_eq!(
            o.detail,
            "`CLAUDE_CODE_REMOTE_MEMORY_DIR` is set to /home/user/.claude here; HOME=/root, \
             CLAUDE_CODE_REMOTE=true"
        );
        let o = evidence_for_env_value(claim, claim, &laptop()).unwrap();
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
    }

    #[test]
    fn a_cloud_variable_set_differently_or_not_at_all_is_stale() {
        let mut facts = cloud();
        let claim = "CLAUDE_CODE_REMOTE_MEMORY_DIR=/root/.claude";
        let o = evidence_for_env_value(claim, claim, &facts).unwrap();
        assert_eq!(o.signal, Signal::Contradicts, "{}", o.detail);
        facts.env[2].1 = None;
        let o = evidence_for_env_value(claim, claim, &facts).unwrap();
        assert_eq!(o.signal, Signal::Contradicts, "{}", o.detail);
        assert!(o.detail.contains("not set"), "{}", o.detail);
    }

    /// `HOME` is different on every machine: decided only when the claim
    /// says which kind of machine it is about, and only on that kind.
    #[test]
    fn home_is_decided_only_for_the_kind_of_machine_the_claim_names() {
        let o = evidence_for_env_value("HOME=/root", "HOME=/root", &cloud()).unwrap();
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
        let claim = "In a cloud session HOME=/root.";
        let o = evidence_for_env_value("HOME=/root", claim, &cloud()).unwrap();
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        let o = evidence_for_env_value("HOME=/root", claim, &laptop()).unwrap();
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
    }

    /// A value is compared only for the three variables `recall status`
    /// already prints: a token in a note is judged by name, and its value
    /// never reaches the evidence.
    #[test]
    fn a_secret_value_is_never_read_or_repeated() {
        let facts = cloud();
        assert!(evidence_for_env_value("RECALL_TOKEN=s3cret", "", &facts).is_none());
        let o = evidence_for_env_name("RECALL_TOKEN", &facts, true).unwrap();
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        assert!(!o.detail.contains("s3cret"), "{}", o.detail);
    }

    fn compose() -> Compose {
        let mut c = Compose::default();
        assert!(read_compose_file(
            "services:\n\
             \x20 recall-server:\n\
             \x20   image: ghcr.io/pimlabs/recall-server:${RECALL_VERSION:-local}\n\
             \x20   container_name: recall-server\n\
             \x20   environment:\n\
             \x20     RECALL_TOKEN: x\n\
             \x20     RECALL_PORT: \"8787\"\n\
             \x20 cloudflared:\n\
             \x20   image: cloudflare/cloudflared:latest\n\
             \x20   environment:\n\
             \x20     - TUNNEL_TOKEN=${TUNNEL_TOKEN}\n",
            "deploy/docker-compose.yml",
            &mut c,
        ));
        assert!(read_compose_file(
            "services:\n\
             \x20 recall-server:\n\
             \x20   labels:\n\
             \x20     - traefik.enable=true\n\
             \x20     - traefik.http.routers.recall.rule=Host(`x`)\n",
            "deploy/docker-compose.traefik.yml",
            &mut c,
        ));
        c.files = vec![
            "deploy/docker-compose.yml".into(),
            "deploy/docker-compose.traefik.yml".into(),
        ];
        c
    }

    /// Services, images (by their bare name too), label namespaces and
    /// passed variables are what the compose files decide.
    #[test]
    fn compose_names_services_images_ingresses_and_variables() {
        let facts = Facts {
            compose: Some(compose()),
            ..connected()
        };
        for name in ["cloudflared", "recall-server", "traefik"] {
            let o = evidence_for_name(name, &facts, true).unwrap();
            assert_eq!(o.signal, Signal::Confirms, "{name}: {}", o.detail);
            assert_eq!(o.source, "compose");
        }
        let o = evidence_for_env_name("TUNNEL_TOKEN", &facts, true).unwrap();
        assert_eq!(o.source, "compose", "{}", o.detail);
        assert!(evidence_for_name("orbstack", &facts, true).is_none());
        // Outside the project scope the checkout's compose files say
        // nothing.
        assert!(evidence_for_name("cloudflared", &facts, false).is_none());
    }

    #[test]
    fn a_file_that_is_not_compose_is_not_read_as_one() {
        let mut c = Compose::default();
        assert!(!read_compose_file("just: [text", "x.yml", &mut c));
        assert!(!read_compose_file("version: 3\n", "x.yml", &mut c));
    }

    #[test]
    fn a_capability_the_server_lists_is_still_true_in_any_scope() {
        let o = evidence_for_name("evaluation", &connected(), false).unwrap();
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        assert_eq!(o.source, "server");
    }

    /// A version in a claim about the server is compared with what the
    /// server reports; only a claim that says it is what runs now is stale
    /// for naming another.
    #[test]
    fn a_server_version_claim_is_compared_with_what_the_server_reports() {
        let facts = connected();
        let o = evidence_for_server_version((0, 4, 8), "production reports 0.4.8", &facts).unwrap();
        assert_eq!(o.signal, Signal::Confirms, "{}", o.detail);
        let o =
            evidence_for_server_version((0, 4, 7), "the server runs 0.4.7 now", &facts).unwrap();
        assert_eq!(o.signal, Signal::Contradicts, "{}", o.detail);
        let o = evidence_for_server_version((0, 4, 5), "deploy by digest shipped in 0.4.5", &facts)
            .unwrap();
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
        assert!(evidence_for_server_version((0, 4, 5), "shipped in 0.4.5", &facts).is_none());
    }

    #[test]
    fn a_host_is_read_out_of_any_shape_a_note_writes() {
        for (value, host) in [
            ("recall.pimlabs.id", "recall.pimlabs.id"),
            ("https://Recall.PimLabs.id/", "recall.pimlabs.id"),
            ("https://u:p@x.example.org:8443/a?b#c", "x.example.org"),
        ] {
            assert_eq!(host_of(value), host, "{value}");
        }
    }
}
