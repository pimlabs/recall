//! `recall connect` and `recall disconnect` — setting a machine up, and
//! taking its token away again.
//!
//! `connect` is the one command a new machine needs. It walks through what
//! used to be separate steps — the token, this machine's name, wiring the
//! current project, its first sync — and skips each one that has nothing to
//! do: a saved token that still works is not asked for again, and a project
//! already wired is not offered.
//!
//! What lives here is the part a person sees. The files, and why there are
//! two of them, are `recall_hooks::home`'s.

use std::io::{self, IsTerminal};
use std::path::Path;

use recall_hooks::client::{self, Client};
use recall_hooks::config::Source;
use recall_hooks::home::{self, Home};
use recall_hooks::{backfill, exit, settings, Disposition};

use crate::project as proj;
use crate::ui;

/// What `recall connect` was told on the command line.
pub struct Args {
    /// The server. Defaults to the one `config.toml` names.
    pub url: Option<String>,
    /// This machine's name, instead of asking.
    pub name: Option<String>,
    /// Take the default for every question instead of asking it.
    pub yes: bool,
}

/// The flow stopped, with this exit code. What went wrong has been shown by
/// the time one of these exists.
struct Stop(i32);

type Step<T> = Result<T, Stop>;

/// `recall connect [url]`.
///
/// Verify, then write. The token is checked against `GET /health` and an
/// authenticated call before anything touches the disk, so the file means
/// "this worked" rather than "we got this far" — the same rule the off-box
/// backup stamp follows. A wrong token leaves nothing behind, and you find
/// out now rather than at the next session start.
pub async fn connect(args: Args) -> anyhow::Result<i32> {
    match run(args).await {
        Ok(()) => Ok(exit::OK),
        Err(Stop(code)) => Ok(code),
    }
}

async fn run(args: Args) -> Step<()> {
    // The container is ephemeral: a file written here is gone with it, and
    // appearing to succeed is worse than declining. The environment's own
    // variables are the right store there, and they already win.
    if proj::remote_session() {
        return refuse(
            "this is a remote session, so nothing saved here would last.",
            "Set RECALL_URL and RECALL_TOKEN on the cloud environment instead.",
        );
    }

    // Checked before anything is read, so a typo costs nothing.
    let explicit = args.url.as_deref().map(home::normalize_url);
    if let Some(url) = &explicit {
        check_url(url)?;
    }
    if let Some(raw) = &args.name {
        named(raw)?;
    }

    let here = proj::resolve();
    let Some(h) = home::locate(here.env.lookup()) else {
        return refuse("no home directory to save into.", "Set RECALL_HOME.");
    };

    // Before any prompt, so a file this cannot read stops things before the
    // user has typed a secret — and so it is never overwritten.
    let (mut creds, mut config) = match load_both(&h) {
        Ok(both) => both,
        Err(e) => {
            return refuse(
                &e.to_string(),
                "Nothing was changed. Fix or move it aside and run recall connect again.",
            )
        }
    };

    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let url = match explicit.or_else(|| config.server.clone()) {
        Some(url) => url,
        None if interactive => {
            intro();
            let typed: String = answer(
                cliclack::input("Server URL")
                    .placeholder("https://recall.example.com")
                    .validate(|s: &String| {
                        if has_host(&home::normalize_url(s)) {
                            Ok(())
                        } else {
                            Err("a server URL starts with https://")
                        }
                    })
                    .interact(),
            )?;
            home::normalize_url(&typed)
        }
        None => {
            return refuse(
                "no server named, and none saved yet.",
                "Name it: recall connect https://your-recall-host",
            )
        }
    };
    check_url(&url)?;

    // A token has to come from the person unless a saved one still works,
    // and only a terminal can supply it. Deliberately a terminal and nothing
    // else: every other way in is another way out, and automation already
    // has RECALL_TOKEN. Decided before the network is touched.
    let saved = creds.token_for(&url).map(str::to_string);
    if saved.is_none() && !interactive {
        return refuse(
            "needs a terminal to ask for the token.",
            "In a script, set RECALL_TOKEN instead.",
        );
    }

    intro();
    if url.starts_with("http://") && !is_loopback(&url) {
        say_warning(&format!(
            "{} uses plain http, so the token is sent unencrypted",
            host(&url)
        ));
    }

    reach(&url).await?;
    let token = match saved {
        Some(token) if verify(&url, &token, "saved token").await? => token,
        Some(_) => {
            if !interactive {
                return refuse(
                    "needs a new token, but there is no terminal to ask for one.",
                    "Run recall connect in a terminal.",
                );
            }
            ask_token(&url).await?
        }
        None => ask_token(&url).await?,
    };

    let name = machine_name(&args, &config, &here, interactive)?;

    creds.insert(&url, &token);
    config.server = Some(url.clone());
    config.machine.name = Some(name.clone());
    if let Err(e) = h
        .save_credentials(&creds)
        .and_then(|()| h.save_config(&config))
    {
        return refuse(
            &format!("the token is valid, but saving it failed: {e}"),
            "",
        );
    }
    say_success(&format!(
        "Saved to {}",
        ui::tilde(&h.dir().display().to_string())
    ));

    // Resolved again, so what follows reads what was just saved — the way
    // every later command will.
    let here = proj::resolve();
    let wiring = offer_init(&args, interactive)?;
    if wiring == Wiring::JustNow {
        offer_backfill(&here, &args, interactive).await?;
    }

    for line in environment_overrides(&here, &url, &name) {
        say_warning(&line);
    }
    let redundant = redundant_variables(&here);
    if !redundant.is_empty() {
        let _ = cliclack::log::remark(format!(
            "No longer needed, remove from your shell profile: {}",
            redundant.join(", ")
        ));
    }

    let _ = cliclack::outro(match wiring {
        Wiring::NoProject => {
            format!("Connected as {name}. Run recall init in a project to sync it.")
        }
        Wiring::Declined => format!("Connected as {name}. Run recall init to sync this project."),
        Wiring::Already | Wiring::JustNow => format!("Connected as {name}"),
    });
    Ok(())
}

/// Refuses a URL that is not one, before anything else happens.
fn check_url(url: &str) -> Step<()> {
    if has_host(url) {
        Ok(())
    } else {
        refuse(
            &format!("{url:?} is not a server URL."),
            "Include the scheme: recall connect https://recall.example.com",
        )
    }
}

/// `GET /health`: whether anything answers at `url` at all. Asked on its own
/// so that "unreachable" and "wrong token" are never confused.
async fn reach(url: &str) -> Step<()> {
    let spinner = spin(&format!("Connecting to {}", host(url)));
    let result = match Client::new(url, "") {
        Ok(client) => client.health().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok(_) => {
            spinner.stop(format!("Connected to {}", host(url)));
            Ok(())
        }
        Err(e) => {
            spinner.error(format!("Can't reach {}", host(url)));
            refuse(&format!("could not reach {url}: {e}"), "Nothing was saved.")
        }
    }
}

/// Whether the server accepts `token` — `what` is how the spinner names it.
/// `Ok(false)` is a clear rejection; anything else that goes wrong ends the
/// flow, because it says nothing about the token either way.
async fn verify(url: &str, token: &str, what: &str) -> Step<bool> {
    let spinner = spin(&format!("Checking {what}"));
    let result = match Client::new(url, token) {
        Ok(client) => client.check_token().await,
        Err(e) => {
            spinner.error(format!("Couldn't check {what}"));
            return refuse(&e.to_string(), "Nothing was saved.");
        }
    };
    let what = capitalized(what);
    match result {
        Ok(()) => {
            spinner.stop(format!("{what} OK"));
            Ok(true)
        }
        Err(client::Error::Status {
            code: 401 | 403, ..
        }) => {
            spinner.error(format!("{what} rejected"));
            Ok(false)
        }
        Err(e) => {
            spinner.error(format!("Couldn't check {}", what.to_lowercase()));
            refuse(
                &format!("{url} answered /health but not an authenticated call: {e}"),
                "Nothing was saved.",
            )
        }
    }
}

/// How many rejected tokens in a row end the flow. Enough for a mistyped
/// paste; few enough that a loop is not a way to guess.
const TOKEN_ATTEMPTS: usize = 3;

/// Asks for the token until the server accepts one.
async fn ask_token(url: &str) -> Step<String> {
    for attempt in 1..=TOKEN_ATTEMPTS {
        let typed: String = answer(
            cliclack::password("Token  (the server's RECALL_TOKEN)")
                .mask('•')
                .validate(|s: &String| {
                    if s.trim().is_empty() {
                        Err("paste the token, or Ctrl-C to stop")
                    } else {
                        Ok(())
                    }
                })
                .interact(),
        )?;
        let token = typed.trim().to_string();
        if verify(url, &token, "token").await? {
            return Ok(token);
        }
        if attempt < TOKEN_ATTEMPTS {
            let _ = cliclack::log::remark("Try again");
        }
    }
    refuse(
        &format!("{url} is up, and it rejected {TOKEN_ATTEMPTS} tokens in a row."),
        "Nothing was saved.",
    )
}

/// This machine's name: `--name`, else asked with a suggestion filled in,
/// else — with nobody to ask — the suggestion.
fn machine_name(
    args: &Args,
    config: &home::Config,
    here: &proj::Resolved,
    interactive: bool,
) -> Step<String> {
    if let Some(raw) = &args.name {
        return named(raw);
    }
    let suggested = suggested_name(
        config.machine.name.as_deref(),
        here.env.get("RECALL_MACHINE_KEY").as_deref(),
        recall_hooks::config::this_hostname().as_deref(),
    )
    .unwrap_or_else(|| "this-machine".to_string());
    if !interactive || args.yes {
        say_step(&format!("Machine name  {suggested}"));
        return Ok(suggested);
    }
    let typed: String = answer(
        cliclack::input("Machine name")
            .default_input(&suggested)
            .validate(|s: &String| match home::machine_name(s) {
                Some(_) => Ok(()),
                None => Err("letters, digits, '.', '-' and '_' only"),
            })
            .interact(),
    )?;
    Ok(home::machine_name(&typed).unwrap_or(suggested))
}

/// `--name`, checked. Also checked before anything else happens, so a bad
/// one is refused before a token has been typed rather than after.
fn named(raw: &str) -> Step<String> {
    match home::machine_name(raw) {
        Some(name) => Ok(name),
        None => refuse(
            &format!("{raw:?} is not a usable machine name."),
            "Use letters, digits, '.', '-' and '_', at most 64 of them.",
        ),
    }
}

/// The name to offer: the one already saved, else the machine key an
/// earlier setup exported, else the hostname made into a name.
///
/// The hostname comes last because it is the least chosen of the three:
/// `Ekos-MacBook-Pro.local` is what an installer picked, and it is offered
/// as `ekos-macbook-pro` rather than as it is.
fn suggested_name(
    saved: Option<&str>,
    machine_key: Option<&str>,
    host: Option<&str>,
) -> Option<String> {
    saved
        .and_then(home::machine_name)
        .or_else(|| machine_key.and_then(home::machine_name))
        .or_else(|| {
            let host = host?.trim().split('.').next()?.to_ascii_lowercase();
            let cleaned: String = host
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                        c
                    } else {
                        '-'
                    }
                })
                .take(64)
                .collect();
            home::machine_name(cleaned.trim_matches('-'))
        })
}

/// What became of the current project's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wiring {
    /// Not in a git repository, so there is no project to wire.
    NoProject,
    /// Wired before this run.
    Already,
    /// Wired by this run.
    JustNow,
    /// Not wired: the person said no, or there was nobody to ask.
    Declined,
}

/// Offers `recall init` for the project the command was run in.
fn offer_init(args: &Args, interactive: bool) -> Step<Wiring> {
    let Some(root) = proj::git_root() else {
        return Ok(Wiring::NoProject);
    };
    let settings_path = root.join(".claude").join("settings.json");
    let wired = std::fs::read(&settings_path)
        .map(|b| settings::is_wired(&b))
        .unwrap_or(false);
    if wired {
        say_step(&format!("{} already syncs", project_name(&root)));
        return Ok(Wiring::Already);
    }
    let yes = args.yes
        || (interactive
            && answer(
                cliclack::confirm(format!("Sync {}?", project_name(&root)))
                    .initial_value(true)
                    .interact(),
            )?);
    if !yes {
        return Ok(Wiring::Declined);
    }
    if let Err(e) = settings::wire_file(&settings_path) {
        return refuse(
            &format!("could not wire {}: {e}", settings_path.display()),
            "The connection is saved; fix that and run recall init.",
        );
    }
    // Committing it is not a nicety: it is what makes a fresh clone or a
    // cloud session sync without any setup of its own.
    let git = match std::env::current_dir() {
        Ok(cwd) if cwd == root => "git".to_string(),
        _ => format!("git -C {}", ui::tilde(&root.display().to_string())),
    };
    let _ = cliclack::note(
        "Hooks added. Commit them so other clones sync too:",
        format!(
            "{git} add .claude/settings.json\n\
             {git} commit -m \"Enable Recall memory sync\""
        ),
    );
    Ok(Wiring::JustNow)
}

/// Offers the first sync, for memory that was written before Recall was.
async fn offer_backfill(here: &proj::Resolved, args: &Args, interactive: bool) -> Step<()> {
    let has_memory = std::fs::read_dir(here.memory_dir())
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if !has_memory {
        return Ok(());
    }
    let yes = args.yes
        || (interactive
            && answer(
                cliclack::confirm("Upload existing memory?")
                    .initial_value(true)
                    .interact(),
            )?);
    if !yes {
        let _ = cliclack::log::remark("Later: recall backfill");
        return Ok(());
    }
    let later = "The connection is saved; run recall backfill to try again.";
    let ctx = match here.hook_context() {
        Ok(ctx) => ctx,
        Err(e) => return refuse(&e.to_string(), later),
    };
    let spinner = spin("Uploading memory");
    let outcome = match backfill(&ctx).await {
        Ok(outcome) => outcome,
        Err(e) => {
            spinner.error("Upload failed");
            return refuse(&e.to_string(), later);
        }
    };
    let sent = outcome.count(Disposition::Sent);
    let matches = outcome.count(Disposition::Matches);
    spinner.stop(match matches {
        0 => format!("Uploaded {}", files(sent)),
        _ => format!("Uploaded {} · {matches} already there", files(sent)),
    });
    // Every file is accounted for, as `recall backfill` insists; the lists
    // themselves are that command's, and it is safe to run again.
    let left = outcome.entries.len() - sent - matches - outcome.count(Disposition::Internal);
    if left > 0 || outcome.stopped.is_some() {
        say_warning(&format!(
            "{} skipped, run recall backfill to see why",
            files(left)
        ));
    }
    Ok(())
}

/// The project's directory name, for a question that has to fit on a line.
fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string())
}

/// `url` without its scheme — what a person calls a server.
fn host(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// `1 file`, `3 files`.
fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whatever in the environment still wins over what was just saved, named,
/// because otherwise `connect` looks like it did nothing.
fn environment_overrides(here: &proj::Resolved, connected: &str, name: &str) -> Vec<String> {
    let cfg = here.config();
    let mut out = Vec::new();
    if cfg.url_source == Source::Environment && home::normalize_url(&cfg.url) != connected {
        out.push(format!(
            "RECALL_URL in your shell points to {}, unset it to use this server",
            host(&cfg.url)
        ));
    }
    if cfg.token_source == Source::Environment {
        out.push(
            match here.env.declared(&["RECALL_TOKEN"]).into_iter().next() {
                Some(d) => format!(
                    "{} sets RECALL_TOKEN, which overrides the saved one",
                    d.file
                ),
                None => "RECALL_TOKEN in your shell overrides the saved one, remove it from \
                         your shell profile"
                    .to_string(),
            },
        );
    }
    let overridden = crate::status::overrides(here, &cfg);
    for o in overridden.iter().filter(|o| o.setting == "machine.name") {
        out.push(format!(
            "{}={} overrides the name {name}, remove it from your shell profile",
            o.variable, o.environment
        ));
    }
    out
}

/// The machine variables that agree with the name, and so are no longer
/// needed. Not a warning — nothing is wrong today — but a variable left
/// behind is what quietly overrules the next rename.
fn redundant_variables(here: &proj::Resolved) -> Vec<&'static str> {
    let cfg = here.config();
    let overridden = crate::status::overrides(here, &cfg);
    ["RECALL_MACHINE_KEY", "RECALL_SOURCE_ENV"]
        .into_iter()
        .filter(|var| here.env.get(var).is_some_and(|v| !v.trim().is_empty()))
        .filter(|var| !overridden.iter().any(|o| o.variable == *var))
        .collect()
}

/// The flow's first line. Shown once, however many paths reach it.
fn intro() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if !SHOWN.swap(true, Ordering::Relaxed) {
        cliclack::set_theme(Bullets);
        let _ = cliclack::intro("recall connect");
    }
}

/// cliclack's look with round marks instead of its squares and diamonds: a
/// filled bullet for the step being asked or a result, a hollow one for a
/// step that is done, and a bullet to mask the token.
struct Bullets;

const FILLED: console::Emoji = console::Emoji("●", "*");
const HOLLOW: console::Emoji = console::Emoji("○", "o");
const CROSS: console::Emoji = console::Emoji("✗", "x");

impl cliclack::Theme for Bullets {
    fn state_symbol(&self, state: &cliclack::ThemeState) -> String {
        let color = self.state_symbol_color(state);
        let symbol = match state {
            cliclack::ThemeState::Active => FILLED,
            cliclack::ThemeState::Submit => HOLLOW,
            cliclack::ThemeState::Cancel | cliclack::ThemeState::Error(_) => CROSS,
        };
        color.apply_to(symbol).to_string()
    }

    fn active_symbol(&self) -> String {
        console::style(FILLED).green().to_string()
    }

    fn submit_symbol(&self) -> String {
        console::style(HOLLOW).green().to_string()
    }

    fn error_symbol(&self) -> String {
        console::style(CROSS).red().to_string()
    }

    fn password_mask(&self) -> char {
        '•'
    }
}

/// A spinner while the server answers — on a terminal. Anywhere else there
/// is nothing to animate, and cliclack draws nothing at all, including the
/// line the spinner ends on; that line is the result, so it is printed as an
/// ordinary one instead.
struct Spinner(Option<cliclack::ProgressBar>);

impl Spinner {
    fn stop(&self, message: impl std::fmt::Display) {
        match &self.0 {
            Some(bar) => bar.stop(message),
            None => say_step(&message.to_string()),
        }
    }

    fn error(&self, message: impl std::fmt::Display) {
        match &self.0 {
            Some(bar) => bar.error(message),
            None => {
                let _ = cliclack::log::error(message);
            }
        }
    }
}

fn spin(message: &str) -> Spinner {
    if !io::stderr().is_terminal() {
        return Spinner(None);
    }
    let bar = cliclack::spinner();
    bar.start(message);
    Spinner(Some(bar))
}

fn say_step(message: &str) {
    let _ = cliclack::log::step(message);
}

fn say_success(message: &str) {
    let _ = cliclack::log::success(message);
}

fn say_warning(message: &str) {
    let _ = cliclack::log::warning(message);
}

/// An answer to a prompt, or the end of the flow when there is none —
/// Ctrl-C, or a terminal that went away.
fn answer<T>(result: io::Result<T>) -> Step<T> {
    result.map_err(|e| {
        if e.kind() == io::ErrorKind::Interrupted {
            let _ = cliclack::outro_cancel("Stopped. Nothing more was saved.");
        } else {
            eprintln!("recall connect: could not read the answer: {e}");
        }
        Stop(exit::CONFIG)
    })
}

/// Ends the flow: what went wrong and, when there is one, what to do.
///
/// Plain `recall connect: …` lines on stderr rather than the flow's own
/// styling. A refusal is the output most likely to end up in a log or be
/// searched for, and it should read the same wherever it lands.
fn refuse<T>(what: &str, then: &str) -> Step<T> {
    eprintln!("recall connect: {what}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    Err(Stop(exit::CONFIG))
}

/// `recall disconnect [url]`.
///
/// Removes what `connect` saved. It cannot remove what the environment
/// supplies, and it says so rather than implying otherwise — a command
/// whose job is to make you safer must not manufacture a false sense of it.
pub fn disconnect(url: Option<&str>) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let Some(h) = home::locate(here.env.lookup()) else {
        eprintln!("recall disconnect: no home directory, so nothing is saved. Set RECALL_HOME.");
        return Ok(exit::CONFIG);
    };
    let (mut creds, mut config) = match load_both(&h) {
        Ok(both) => both,
        Err(e) => {
            eprintln!("recall disconnect: {e}");
            eprintln!("  Nothing was changed.");
            return Ok(exit::CONFIG);
        }
    };
    let path = h.credentials_path();

    let target = match url {
        Some(u) => Some(home::normalize_url(u)),
        None => config.server.clone().or_else(|| {
            // With exactly one server there is nothing to choose between.
            (creds.servers.len() == 1)
                .then(|| creds.servers.keys().next().cloned())
                .flatten()
        }),
    };

    match target {
        None if creds.servers.is_empty() => {
            println!("No token is saved in {}.", path.display());
        }
        None => {
            eprintln!("recall disconnect: more than one server is saved; name one:");
            for u in creds.servers.keys() {
                eprintln!("  recall disconnect {u}");
            }
            return Ok(exit::CONFIG);
        }
        Some(target) => {
            if creds.remove(&target) {
                let result = if creds.servers.is_empty() {
                    h.delete_credentials()
                } else {
                    h.save_credentials(&creds)
                };
                // The config stops naming a server it has no token for. The
                // machine name stays: it describes this machine, not the
                // connection.
                let result = result.and_then(|()| {
                    if config.server.as_deref() == Some(target.as_str()) {
                        config.server = None;
                        h.save_config(&config)
                    } else {
                        Ok(())
                    }
                });
                if let Err(e) = result {
                    eprintln!("recall disconnect: {e}");
                    return Ok(exit::CONFIG);
                }
                println!("Removed the token for {target} from {}.", path.display());
                // Removing a copy is not revoking the secret, and with one
                // shared token there is no revoking one machine's.
                println!(
                    "The token itself still works on the server. To revoke it, rotate \
                     RECALL_TOKEN there and reconnect every machine."
                );
            } else {
                println!("No token for {target} is saved in {}.", path.display());
            }
        }
    }

    // What is still in effect after this, which the command cannot change.
    let cfg = here.config();
    if cfg.token_source == Source::Environment {
        println!();
        println!("{}", environment_token_origin(&here, "still supplies"));
    }
    Ok(exit::OK)
}

/// Where the environment's `RECALL_TOKEN` comes from, as far as it can be
/// known.
///
/// A settings file can be named, because it was read. The shell cannot: by
/// the time a process sees a variable, which profile exported it is gone,
/// and guessing a file name here would send someone to edit the wrong one
/// and believe they were done.
fn environment_token_origin(here: &proj::Resolved, verb: &str) -> String {
    match here.env.declared(&["RECALL_TOKEN"]).into_iter().next() {
        Some(d) => format!("{} {verb} RECALL_TOKEN.", d.file),
        None => format!("Your shell {verb} RECALL_TOKEN. Remove it from your shell profile."),
    }
}

/// Both files, after moving 0.3.0's JSON file into them, so everything below
/// works on one format. Missing files read as empty.
fn load_both(h: &Home) -> Result<(home::Credentials, home::Config), home::Error> {
    h.migrate_legacy()?;
    Ok((
        h.load_credentials()?.unwrap_or_default(),
        h.load_config()?.unwrap_or_default(),
    ))
}

/// Whether `url` has a scheme Recall speaks and a host after it.
fn has_host(url: &str) -> bool {
    ["https://", "http://"].iter().any(|scheme| {
        url.strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Plain http is fine to a server on this machine — a local test server —
/// and nowhere else.
fn is_loopback(url: &str) -> bool {
    let authority = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_needs_a_scheme_and_a_host() {
        assert!(has_host("https://recall.example.com"));
        assert!(has_host("http://localhost:8787"));
        for bad in ["recall.example.com", "https://", "ftp://x", ""] {
            assert!(!has_host(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_this_machine_is_exempt_from_the_plain_http_warning() {
        for local in [
            "http://localhost:8787",
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
        ] {
            assert!(is_loopback(local), "{local}");
        }
        for remote in ["http://recall.example.com", "http://10.0.0.5:8787"] {
            assert!(!is_loopback(remote), "{remote}");
        }
    }

    #[test]
    fn a_saved_name_is_offered_before_an_old_key_before_the_hostname() {
        let host = Some("Ekos-MacBook-Pro.local");
        assert_eq!(
            suggested_name(Some("jarvis"), Some("machine:mbp"), host).as_deref(),
            Some("jarvis")
        );
        assert_eq!(
            suggested_name(None, Some("machine:mbp"), host).as_deref(),
            Some("mbp")
        );
        assert_eq!(
            suggested_name(None, None, host).as_deref(),
            Some("ekos-macbook-pro")
        );
        // An unusable saved name is passed over rather than offered.
        assert_eq!(
            suggested_name(Some("my laptop"), None, host).as_deref(),
            Some("ekos-macbook-pro")
        );
    }

    #[test]
    fn a_hostname_is_made_into_a_name() {
        assert_eq!(
            suggested_name(None, None, Some("Eko's Mac mini")).as_deref(),
            Some("eko-s-mac-mini")
        );
        assert_eq!(suggested_name(None, None, Some("  ")), None);
        assert_eq!(suggested_name(None, None, Some("...")), None);
        assert_eq!(suggested_name(None, None, None), None);
    }
}
