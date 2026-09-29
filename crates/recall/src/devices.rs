//! `recall devices` and `recall authkey` — the owner's commands for the
//! machines enrolled on a server: list them, approve a new one by the code
//! it shows, revoke one; and make, list or revoke the authkeys cloud
//! sessions enrol with (Tailscale's name, for the same thing).
//!
//! Everything here is an admin request, so it needs a device enrolled with
//! the `admin` scope or the operator's `RECALL_TOKEN`. The first device
//! `recall connect` approves with the token is an admin one, which is how
//! these become usable without the token ever being needed again.
//!
//! Approving is the step a person can be talked into getting wrong: someone
//! reads out a code and asks for it to be approved (RFC 8628 §5.4). So
//! `approve` shows what the code would approve before doing anything, asks,
//! and sends the fingerprint it showed, so the server approves that key and
//! no other.

use std::io::{self, IsTerminal};

use clap::Subcommand;
use recall_hooks::client::{self, Client};
use recall_hooks::{exit, ClientConfig};
use recall_wire::devices::{
    normalize_user_code, CODE_TTL_SECONDS, DEFAULT_MAX_DEVICES, MAX_AUTHKEY_DAYS, SCOPE_ADMIN,
    SCOPE_SYNC, SCOPE_WORKER,
};
use recall_wire::{ApproveRequest, AuthkeyRequest, Device};

use crate::project as proj;
use crate::ui::{self, Tone};

/// `recall devices …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Every device: name, scope, whether it is ephemeral, last seen, agent
    List {
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Approve a machine by the code it shows, after checking what it is
    Approve {
        /// The code the machine shows, such as WDJB-MJHT
        code: String,
        /// Give it the admin scope, so it can approve and revoke devices too
        #[arg(long, conflicts_with = "worker")]
        admin: bool,
        /// Give it the worker scope: a recall-worker, which may take merge
        /// jobs and nothing else
        #[arg(long)]
        worker: bool,
        /// The fingerprint the machine shows. Refuses to approve a key with
        /// any other
        #[arg(long)]
        fingerprint: Option<String>,
        /// Approve without asking, for scripts
        #[arg(long, short)]
        yes: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Revoke a device: its requests are refused from now on
    Revoke {
        /// The device's name, or its id (dev_…)
        name: String,
        /// Revoke without asking, for scripts
        #[arg(long, short)]
        yes: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
}

/// `recall authkey …`.
#[derive(Subcommand)]
pub enum KeyCmd {
    /// Make an authkey. It is shown once
    Create {
        /// A label, and the start of the name of every device it enrols
        #[arg(long)]
        tag: Option<String>,
        /// How long it works: days, such as 90d, or weeks, such as 12w (at most 365 days)
        #[arg(long)]
        expires: String,
        /// The most devices it may have enrolled at once
        #[arg(long)]
        max_devices: Option<u32>,
        /// Devices it enrols stay until revoked, instead of being removed once idle
        #[arg(long)]
        persistent: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Every authkey, without the keys themselves
    List {
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Stop an authkey enrolling anything more
    Revoke {
        /// The key's id, from recall authkey list
        id: String,
        /// Revoke every device it enrolled as well, for a key that leaked
        #[arg(long)]
        revoke_devices: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
}

/// What a command's errors are signed with: the command a person typed.
const DEVICES: &str = "recall devices";
const AUTHKEY: &str = "recall authkey";

/// Runs one `recall devices` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => return Ok(refuse(DEVICES, &why, "")),
    };
    let server = server_name(&cfg.url);
    let result = match cmd {
        Cmd::List { json } => list(&cfg, &client, &server, json).await,
        Cmd::Approve {
            code,
            admin,
            worker,
            fingerprint,
            yes,
            json,
        } => {
            let scope = match (admin, worker) {
                (true, _) => SCOPE_ADMIN,
                (_, true) => SCOPE_WORKER,
                _ => SCOPE_SYNC,
            };
            let fingerprint = fingerprint.as_deref();
            approve(&client, &server, &code, scope, fingerprint, yes, json).await
        }
        Cmd::Revoke { name, yes, json } => revoke(&cfg, &client, &server, &name, yes, json).await,
    };
    Ok(finish(DEVICES, result))
}

/// Runs one `recall authkey` command.
pub async fn run_authkey(cmd: KeyCmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => return Ok(refuse(AUTHKEY, &why, "")),
    };
    let server = server_name(&cfg.url);
    let result = match cmd {
        KeyCmd::Create {
            tag,
            expires,
            max_devices,
            persistent,
            json,
        } => {
            create_key(
                &client,
                &server,
                tag.unwrap_or_default(),
                &expires,
                max_devices,
                persistent,
                json,
            )
            .await
        }
        KeyCmd::List { json } => list_keys(&client, &server, json).await,
        KeyCmd::Revoke {
            id,
            revoke_devices,
            json,
        } => revoke_key(&client, &id, revoke_devices, json).await,
    };
    Ok(finish(AUTHKEY, result))
}

/// The exit code, having said what went wrong when something did, as
/// `command`.
fn finish(command: &str, result: Done) -> i32 {
    match result {
        Ok(code) => code,
        Err(Failed::Refused(code)) => code,
        Err(Failed::Server(e)) => server_error(command, &e),
        Err(Failed::Json(e)) => refuse(command, &format!("could not write JSON: {e}"), ""),
    }
}

/// Why a command stopped.
enum Failed {
    /// Already explained, with this exit code.
    Refused(i32),
    /// The server said no, or could not be reached.
    Server(client::Error),
    /// Printing `--json` failed.
    Json(serde_json::Error),
}

impl From<client::Error> for Failed {
    fn from(e: client::Error) -> Self {
        Failed::Server(e)
    }
}

impl From<serde_json::Error> for Failed {
    fn from(e: serde_json::Error) -> Self {
        Failed::Json(e)
    }
}

type Done = Result<i32, Failed>;

/// The client admin requests go out with.
///
/// An admin device signs them. A `sync` device cannot make them, so the
/// operator's token is used instead when this machine has one; without one,
/// the device's own signature is sent anyway, and the server's refusal says
/// what is missing better than a guess here could.
pub(crate) fn admin_client(cfg: &ClientConfig) -> Result<Client, String> {
    if cfg.url.is_empty() {
        return Err("no server: run recall connect first, or set RECALL_URL.".to_string());
    }
    let admin_device = cfg.device.as_ref().is_some_and(|d| d.scope == SCOPE_ADMIN);
    if !admin_device && !cfg.token.is_empty() {
        return Client::new(&cfg.url, &cfg.token).map_err(|e| e.to_string());
    }
    if cfg.device.is_none() {
        return Err(
            "this needs a device enrolled as admin, or the server's RECALL_TOKEN. Run recall \
             connect to enrol this machine."
                .to_string(),
        );
    }
    cfg.client().map_err(|e| e.to_string())
}

/// What the server said, in a line, as `command`, with what to do about
/// the answers a person can do something about.
fn server_error(command: &str, e: &client::Error) -> i32 {
    let reason = e.reason();
    let then = match e {
        client::Error::Status { code: 403, .. } => {
            "This machine's device has the sync scope. Run this on an admin device, or with the \
             server's RECALL_TOKEN set."
                .to_string()
        }
        _ if e.device_gone() => "Run recall connect to enrol this machine again.".to_string(),
        client::Error::Status { code: 409, .. } if e.reason().contains("already exists") => {
            "Nothing was approved. Revoke the device with that name first (recall devices \
             revoke <name>), or have the machine enrol under another name."
                .to_string()
        }
        client::Error::Status { code: 404, .. } if reason.contains("with that code") => format!(
            "Check the code the machine shows. A code lasts {} minutes.",
            CODE_TTL_SECONDS / 60
        ),
        client::Error::Status { code: 404, .. } if reason.contains("no authkey") => {
            "recall authkey list shows their ids.".to_string()
        }
        client::Error::Transport(_) => "Check the server is up: recall doctor".to_string(),
        _ => String::new(),
    };
    eprintln!("{command}: {reason}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    match e {
        client::Error::Status { .. } => exit::SERVER,
        _ => exit::CONFIG,
    }
}

/// Stops with a reason, as `command`, and what to do when there is
/// something.
fn refuse(command: &str, what: &str, then: &str) -> i32 {
    eprintln!("{command}: {what}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    exit::CONFIG
}

fn refused(command: &str, what: &str, then: &str) -> Done {
    Err(Failed::Refused(refuse(command, what, then)))
}

/// Asks `question`, or takes `--yes` for an answer. With neither a
/// terminal nor `--yes` it refuses: a change like this is never made by
/// default.
fn confirmed(question: &str, yes: bool) -> Result<bool, Failed> {
    if yes {
        return Ok(true);
    }
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(Failed::Refused(refuse(
            DEVICES,
            "needs a terminal to ask first.",
            "In a script, pass --yes.",
        )));
    }
    cliclack::confirm(question)
        .initial_value(false)
        .interact()
        .map_err(|_| Failed::Refused(exit::CONFIG))
}

async fn list(cfg: &ClientConfig, client: &Client, server: &str, json: bool) -> Done {
    let list = client.devices().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(exit::OK);
    }
    ui::title("recall devices list", server);
    anstream::println!();
    if list.devices.is_empty() {
        anstream::println!("  No devices yet.");
        next("recall connect", "enrols this machine");
        return Ok(exit::OK);
    }
    // The ones in use first, in the server's order (newest first); the
    // revoked ones after them, dimmed, kept for the record.
    let (live, revoked): (Vec<&Device>, Vec<&Device>) =
        list.devices.iter().partition(|d| d.revoked_at.is_none());
    let mut summary = vec![format!("{} in use", live.len())];
    if !revoked.is_empty() {
        summary.push(ui::toned(
            Tone::Quiet,
            &format!("○ {} revoked", revoked.len()),
        ));
    }
    anstream::println!("  {}", summary.join("   "));
    anstream::println!();

    let this = cfg.device.as_ref().map(|d| d.device_id.as_str());
    let rows: Vec<Row<6>> = live
        .iter()
        .chain(&revoked)
        .map(|d| {
            let mut note = Vec::new();
            if Some(d.id.as_str()) == this {
                note.push("this machine".to_string());
            }
            if d.ephemeral {
                note.push("ephemeral".to_string());
            }
            if let Some(at) = &d.revoked_at {
                note.push(format!("revoked {}", relative(at)));
            }
            Row {
                tone: d.revoked_at.is_some().then_some(Tone::Quiet),
                cells: [
                    d.name.clone(),
                    d.scope.clone(),
                    d.last_seen.as_deref().map_or("never".to_string(), relative),
                    short_hash(&d.fingerprint),
                    ui::clip(&d.agent, AGENT_WIDTH),
                    note.join(", "),
                ],
                under: None,
            }
        })
        .collect();
    table(
        ["NAME", "SCOPE", "LAST SEEN", "FINGERPRINT", "AGENT", ""],
        &rows,
    );
    Ok(exit::OK)
}

/// How wide an agent string may run in `recall devices list`: wide enough
/// for `recall/0.4.10-dev (linux-x86_64)`, the longest the CLI sends.
const AGENT_WIDTH: usize = 34;

async fn approve(
    client: &Client,
    server: &str,
    code: &str,
    scope: &str,
    expected: Option<&str>,
    yes: bool,
    json: bool,
) -> Done {
    let Some(code) = normalize_user_code(code) else {
        return refused(
            DEVICES,
            &format!("{code:?} is not a code."),
            "A code is the eight letters the machine shows, such as WDJB-MJHT.",
        );
    };
    let pending = client.pending(&code).await?;

    // Shown on stderr, so `--json` leaves stdout for the result alone. The
    // code and the fingerprint are shown whole, in bold: they are what the
    // owner compares with what the machine shows.
    title_on_stderr("recall devices approve", server);
    anstream::eprintln!();
    let agent = if pending.agent.is_empty() {
        ui::dim("(none given)")
    } else {
        crate::edit::printable(&pending.agent)
    };
    for (label, value) in [
        ("Code", ui::bold(&pending.user_code)),
        ("Name", ui::bold(&crate::edit::printable(&pending.name))),
        ("Agent", agent),
        ("Fingerprint", ui::bold(&pending.fingerprint)),
        ("Expires in", minutes(pending.expires_in)),
    ] {
        anstream::eprintln!("  {}  {value}", ui::dim(&format!("{label:<11}")));
    }
    anstream::eprintln!();

    if let Some(expected) = expected {
        if !same_fingerprint(expected, &pending.fingerprint) {
            eprintln!(
                "{DEVICES}: the machine waiting with {code} has another fingerprint. Nothing was \
                 approved."
            );
            eprintln!("  given   {}", expected.trim());
            eprintln!("  it has  {}", pending.fingerprint);
            eprintln!(
                "  Someone else may have enrolled with this code. Check the code on the machine."
            );
            return Err(Failed::Refused(exit::CONFIG));
        }
    }

    let question = format!(
        "Approve {} with the {scope} scope? Check the machine shows this fingerprint.",
        pending.name
    );
    if !confirmed(&question, yes)? {
        eprintln!("Nothing was approved.");
        return Ok(exit::CONFIG);
    }

    // The fingerprint that was shown, so the server approves the key it
    // belongs to and no other, whatever may have changed since the lookup.
    let device = client
        .approve(&ApproveRequest {
            user_code: pending.user_code.clone(),
            scope: scope.to_string(),
            fingerprint: Some(pending.fingerprint.clone()),
        })
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&device)?);
    } else {
        done(&format!(
            "Approved {} ({}). It finishes connecting by itself within a few seconds.",
            device.name, device.scope
        ));
    }
    Ok(exit::OK)
}

/// Whether two fingerprints are the same, forgiving what a person typing
/// one might leave off or add: the `SHA256:` prefix and surrounding space.
/// The hash itself is compared exactly: base64 is case-sensitive.
fn same_fingerprint(typed: &str, actual: &str) -> bool {
    let bare = |f: &str| f.trim().trim_start_matches("SHA256:").to_string();
    !bare(typed).is_empty() && bare(typed) == bare(actual)
}

async fn revoke(
    cfg: &ClientConfig,
    client: &Client,
    server: &str,
    name: &str,
    yes: bool,
    json: bool,
) -> Done {
    let list = client.devices().await?;
    let Some(device) = find_device(&list.devices, name) else {
        return refused(
            DEVICES,
            &format!("no device named {name} is enrolled on {server}."),
            "recall devices list shows them.",
        );
    };
    let this = cfg
        .device
        .as_ref()
        .is_some_and(|d| d.device_id == device.id);
    let question = if this {
        format!(
            "Revoke {}? It is this machine, which then stops syncing.",
            device.name
        )
    } else {
        format!(
            "Revoke {}? Its requests are refused from now on.",
            device.name
        )
    };
    if !confirmed(&question, yes)? {
        eprintln!("Nothing was revoked.");
        return Ok(exit::CONFIG);
    }
    let revoked = client.revoke_device(&device.id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&revoked)?);
    } else {
        done(&format!(
            "Revoked {}: its requests are refused from now on.",
            revoked.name
        ));
        if this {
            anstream::println!(
                "  {} That was this machine, so it no longer syncs.",
                ui::toned(Tone::Warn, "!")
            );
            next("recall connect", "enrols it again");
        }
    }
    Ok(exit::OK)
}

/// The unrevoked device called `name`, compared without case the way the
/// server keeps names unique, or the device with that id.
fn find_device<'a>(devices: &'a [Device], name: &str) -> Option<&'a Device> {
    let name = name.trim();
    devices.iter().find(|d| d.id == name).or_else(|| {
        devices
            .iter()
            .find(|d| d.revoked_at.is_none() && d.name.to_lowercase() == name.to_lowercase())
    })
}

async fn create_key(
    client: &Client,
    server: &str,
    tag: String,
    expires: &str,
    max_devices: Option<u32>,
    persistent: bool,
    json: bool,
) -> Done {
    let Some(days) = days(expires) else {
        return refused(
            AUTHKEY,
            &format!("--expires {expires} is not a length of time Recall reads."),
            &format!("Use days or weeks, such as 90d or 12w, at most {MAX_AUTHKEY_DAYS} days."),
        );
    };
    let created = client
        .create_authkey(&AuthkeyRequest {
            tag,
            expires_in_days: days,
            ephemeral: !persistent,
            max_devices,
        })
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&created)?);
        return Ok(exit::OK);
    }
    ui::title("recall authkey create", server);
    anstream::println!();
    let mut made = vec![format!("Made authkey {}", created.id)];
    if !created.tag.is_empty() {
        made.push(format!("tag {}", created.tag));
    }
    made.push(format!("expires {}", relative(&created.expires_at)));
    anstream::println!(
        "  {} {}",
        ui::toned(Tone::Good, Tone::Good.mark()),
        made.join(" · ")
    );
    let most = created.max_devices.unwrap_or(DEFAULT_MAX_DEVICES);
    anstream::println!(
        "    {}",
        ui::dim(&if created.ephemeral {
            format!("It enrols up to {most} ephemeral devices at once, each removed once idle.")
        } else {
            format!("It enrols up to {most} persistent devices at once, each kept until revoked.")
        })
    );
    // Alone on its line and at its start, so a triple-click copies the
    // key and nothing else.
    anstream::println!();
    anstream::println!("{}", ui::bold(&created.key));
    anstream::println!();
    anstream::println!(
        "  {} This is the only time it is shown: the server keeps only its hash.",
        ui::toned(Tone::Warn, "!")
    );
    anstream::println!("    Put it in your cloud environment's variables as RECALL_AUTHKEY.");
    anstream::println!(
        "    Anyone holding it can enrol a machine that reads and writes your memory until it \
         expires."
    );
    next(
        &format!("recall authkey revoke {}", created.id),
        "if it leaks",
    );
    Ok(exit::OK)
}

async fn list_keys(client: &Client, server: &str, json: bool) -> Done {
    let list = client.authkeys().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(exit::OK);
    }
    ui::title("recall authkey list", server);
    anstream::println!();
    if list.authkeys.is_empty() {
        anstream::println!("  No authkeys.");
        next(
            "recall authkey create --tag cloud --expires 90d",
            "makes one for cloud sessions",
        );
        return Ok(exit::OK);
    }
    // An authkey still enrols until it is revoked or expires; the rest are
    // kept for the record, after the ones in use.
    let expired = |k: &&recall_wire::Authkey| {
        crate::doctor::age_of(&k.expires_at).is_some_and(|age| !age.is_negative())
    };
    let (live, past): (Vec<_>, Vec<_>) = list
        .authkeys
        .iter()
        .partition(|k| k.revoked_at.is_none() && !expired(k));
    let revoked = past.iter().filter(|k| k.revoked_at.is_some()).count();
    let mut summary = vec![format!("{} in use", live.len())];
    if revoked > 0 {
        summary.push(ui::toned(Tone::Quiet, &format!("○ {revoked} revoked")));
    }
    if past.len() > revoked {
        summary.push(ui::toned(
            Tone::Quiet,
            &format!("○ {} expired", past.len() - revoked),
        ));
    }
    anstream::println!("  {}", summary.join("   "));
    anstream::println!();

    let rows: Vec<Row<6>> = live
        .iter()
        .chain(&past)
        .map(|k| {
            let note = match &k.revoked_at {
                Some(at) => format!("revoked {}", relative(at)),
                None if expired(k) => "expired".to_string(),
                None => String::new(),
            };
            Row {
                tone: (!note.is_empty()).then_some(Tone::Quiet),
                cells: [
                    k.id.clone(),
                    k.tag.clone(),
                    if k.ephemeral {
                        "ephemeral"
                    } else {
                        "persistent"
                    }
                    .to_string(),
                    k.max_devices.unwrap_or(DEFAULT_MAX_DEVICES).to_string(),
                    relative(&k.expires_at),
                    note,
                ],
                under: None,
            }
        })
        .collect();
    table(["ID", "TAG", "DEVICES", "MAX", "EXPIRES", ""], &rows);
    Ok(exit::OK)
}

async fn revoke_key(client: &Client, id: &str, revoke_devices: bool, json: bool) -> Done {
    let key = client.revoke_authkey(id.trim(), revoke_devices).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&key)?);
        return Ok(exit::OK);
    }
    let named = if key.tag.is_empty() {
        key.id.clone()
    } else {
        format!("{} ({})", key.id, key.tag)
    };
    if revoke_devices {
        done(&format!(
            "Revoked authkey {named} and every device it enrolled: it enrols nothing more."
        ));
    } else {
        done(&format!("Revoked authkey {named}: it enrols nothing more."));
        anstream::println!("  Devices it already enrolled keep working.");
        next(
            &format!("recall authkey revoke {} --revoke-devices", key.id),
            "revokes them too",
        );
    }
    Ok(exit::OK)
}

/// `90d`, `12w` or a bare `90`, in days, within what the server accepts.
fn days(text: &str) -> Option<u32> {
    let text = text.trim().to_ascii_lowercase();
    let (number, unit) = match text.char_indices().last()? {
        (i, 'd') => (&text[..i], 1),
        (i, 'w') => (&text[..i], 7),
        _ => (text.as_str(), 1),
    };
    let days = number.trim().parse::<u32>().ok()?.checked_mul(unit)?;
    (1..=MAX_AUTHKEY_DAYS).contains(&days).then_some(days)
}

/// `14 min`, from seconds.
fn minutes(seconds: u64) -> String {
    match seconds / 60 {
        0 => "under a minute".to_string(),
        m => format!("{m} min"),
    }
}

// ---------------------------------------------------------------------------
// How the owner's commands look: `devices`, `authkey`, `eval` and `audit`
// share these, so the four read alike.
// ---------------------------------------------------------------------------

/// The server a command talks to, as its title names it: its address
/// without the scheme, as `recall audit` names the server it witnesses.
pub(crate) fn server_name(url: &str) -> String {
    recall_hooks::audit::origin(url)
}

/// [`ui::title`], on stderr: for a command whose stdout is kept for
/// `--json` or for the data itself.
pub(crate) fn title_on_stderr(command: &str, about: &str) {
    if about.is_empty() {
        anstream::eprintln!("{}", ui::bold(command));
    } else {
        anstream::eprintln!("{}  {}", ui::bold(command), ui::dim(about));
    }
}

/// A command's one-line result, marked as done.
pub(crate) fn done(text: &str) {
    anstream::println!("{} {text}", ui::toned(Tone::Good, Tone::Good.mark()));
}

/// The command to run next, and what it does, on a line of its own.
pub(crate) fn next(command: &str, what: &str) {
    anstream::println!("{}", next_line(command, what));
}

/// The line [`next`] prints, for a command that says it on stderr.
pub(crate) fn next_line(command: &str, what: &str) -> String {
    let command = ui::accent(&format!("→ {command}"));
    match what.is_empty() {
        true => format!("  {command}"),
        false => format!("  {command}   {}", ui::dim(what)),
    }
}

/// When a timestamp in the API's format was, or will be, from now, in the
/// largest unit that is not zero: `just now`, `12 min ago`, `5 h ago`,
/// `3 days ago`; `in 20 min`, `in 5 h`, `in 90 days`. The one form a time
/// takes in the text of these commands, with the timestamp beside it only
/// where it is evidence (the rewrite `recall audit` found); `--json` keeps
/// the timestamp. One that does not parse is shown as the date it names.
pub(crate) fn relative(stamp: &str) -> String {
    let Some(age) = crate::doctor::age_of(stamp) else {
        return stamp.split('T').next().unwrap_or(stamp).to_string();
    };
    let seconds = age.whole_seconds();
    if seconds >= 0 {
        return match seconds / 60 {
            0 => "just now".to_string(),
            m if m < 60 => format!("{m} min ago"),
            m if m < 48 * 60 => format!("{} h ago", m / 60),
            m => format!("{} days ago", m / (24 * 60)),
        };
    }
    // Ahead, rounded rather than cut: a key made to last 90 days expires
    // in 90 days, not in 89 and some hours.
    match (-seconds + 30) / 60 {
        0 => "in under a minute".to_string(),
        m if m < 60 => format!("in {m} min"),
        m if m < 48 * 60 => format!("in {} h", (m + 30) / 60),
        m => format!("in {} days", (m + 12 * 60) / (24 * 60)),
    }
}

/// A hash or a key fingerprint cut to its first eight characters, which is
/// enough to tell one from another at a glance: `SHA256:ub/crW1gem0…`
/// becomes `ub/crW1g…`. Whole wherever a person has to compare it, and in
/// `--json`.
pub(crate) fn short_hash(hash: &str) -> String {
    let bare = hash.trim_start_matches("SHA256:");
    match bare.char_indices().nth(8) {
        Some((cut, _)) => format!("{}…", &bare[..cut]),
        None => bare.to_string(),
    }
}

/// `text` on lines no wider than `width`, broken between words; a word
/// longer than that has a line to itself.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `1 checkpoint`, `3 checkpoints`.
pub(crate) fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// One row of a [`table`]. A quiet row, one kept for the record such as a
/// revoked device, is dimmed whole; in any other, the first cell (the name
/// or id a person types back) is in bold.
pub(crate) struct Row<const N: usize> {
    /// The mark in the gutter, when the row has one.
    pub tone: Option<Tone>,
    pub cells: [String; N],
    /// A line under the row, dimmed: a failed report's error.
    pub under: Option<String>,
}

/// Columns padded to their widest cell, under a dimmed header, each row
/// with its mark in the gutter. A line stops at its last cell that is not
/// empty, so none ends in spaces. Every cell is made safe for a terminal
/// first: names, tags and agents come from the server.
pub(crate) fn table<const N: usize>(header: [&str; N], rows: &[Row<N>]) {
    let safe = |text: &str| crate::edit::printable(text).replace('\n', " ");
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.cells.iter().map(|c| safe(c)).collect())
        .collect();
    let mut width = [0usize; N];
    for (i, h) in header.iter().enumerate() {
        width[i] = h.chars().count();
    }
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            width[i] = width[i].max(cell.chars().count());
        }
    }
    let laid_out = |row: &[String]| -> Vec<String> {
        let last = row.iter().rposition(|c| !c.is_empty()).unwrap_or(0);
        row[..=last]
            .iter()
            .enumerate()
            .map(|(i, cell)| match i == last {
                true => cell.clone(),
                false => format!("{cell:<w$}", w = width[i]),
            })
            .collect()
    };
    let header: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    anstream::println!("    {}", ui::dim(&laid_out(&header).join("  ")));
    for (row, cells) in rows.iter().zip(&cells) {
        let laid = laid_out(cells);
        let line = match row.tone {
            Some(Tone::Quiet) => ui::dim(&laid.join("  ")),
            _ => laid
                .iter()
                .enumerate()
                .map(|(i, cell)| if i == 0 { ui::bold(cell) } else { cell.clone() })
                .collect::<Vec<_>>()
                .join("  "),
        };
        let mark = row.tone.map_or(" ".to_string(), |t| ui::toned(t, t.mark()));
        anstream::println!("  {mark} {line}");
        if let Some(under) = &row.under {
            anstream::println!("      {}", ui::dim(&safe(under)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_wrap_between_words_and_never_lose_one() {
        let text =
            "tools.md says to search with ripgrep, and the global search.md says to use git \
                    grep in every repository";
        let lines = wrap(text, 30);
        assert!(lines.iter().all(|l| l.chars().count() <= 30), "{lines:?}");
        assert_eq!(lines.join(" "), text);
        assert_eq!(wrap("", 30), Vec::<String>::new());
        assert_eq!(
            wrap("a-very-long-word-that-does-not-fit here", 10),
            ["a-very-long-word-that-does-not-fit", "here"]
        );
    }

    /// A timestamp in the API's format, `minutes` from now: ahead when
    /// positive, behind when negative.
    fn stamp_in(minutes: i64) -> String {
        let fmt = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        (time::OffsetDateTime::now_utc() + time::Duration::minutes(minutes))
            .format(&fmt)
            .unwrap()
    }

    /// Behind is cut to its unit, ahead is rounded: a key made to last 90
    /// days says it expires in 90 days, the moment it is made.
    #[test]
    fn times_read_as_how_long_ago_or_how_long_until() {
        assert_eq!(relative(&stamp_in(0)), "just now");
        assert_eq!(relative(&stamp_in(-5)), "5 min ago");
        assert_eq!(relative(&stamp_in(-3 * 60 - 20)), "3 h ago");
        assert_eq!(relative(&stamp_in(-3 * 24 * 60 - 60)), "3 days ago");
        assert_eq!(relative(&stamp_in(20)), "in 20 min");
        assert_eq!(relative(&stamp_in(24 * 60)), "in 24 h");
        assert_eq!(relative(&stamp_in(90 * 24 * 60)), "in 90 days");
        // One that does not parse is its date, never a made-up age.
        assert_eq!(relative("2026-09-23T12:00:00Z"), "2026-09-23");
    }

    #[test]
    fn hashes_and_fingerprints_are_cut_to_eight_characters() {
        assert_eq!(
            short_hash("SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI"),
            "sWwtG+rR…"
        );
        assert_eq!(
            short_hash("P7nycHqpeL8eJZ+RWy81Z6MMkQdvR7gZxMBnv1lnekY="),
            "P7nycHqp…"
        );
        assert_eq!(short_hash("short"), "short");
    }

    #[test]
    fn expiries_are_days_or_weeks_within_a_year() {
        assert_eq!(days("90d"), Some(90));
        assert_eq!(days("90"), Some(90));
        assert_eq!(days(" 12W "), Some(84));
        assert_eq!(days("365d"), Some(365));
        for bad in ["0d", "366d", "53w", "d", "", "90m", "-1d", "1.5d"] {
            assert_eq!(days(bad), None, "{bad:?}");
        }
    }

    /// Forgiving about the prefix and spaces, exact about the hash: two
    /// keys whose fingerprints differ only in case are different keys.
    #[test]
    fn fingerprints_compare_exactly_apart_from_the_prefix() {
        let fp = "SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI";
        assert!(same_fingerprint(fp, fp));
        assert!(same_fingerprint(
            " sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI ",
            fp
        ));
        assert!(!same_fingerprint(
            "SHA256:swwtg+rrjiy5dk/budttd0wzm2vuk0bm2ksrnswfigi",
            fp
        ));
        assert!(!same_fingerprint("SHA256:", fp));
        assert!(!same_fingerprint("", fp));
    }

    fn device(id: &str, name: &str, revoked: bool) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            revoked_at: revoked.then(|| "2026-09-23T12:00:00.000Z".to_string()),
            ..Default::default()
        }
    }

    /// A revoked device's name is free again, so a name picks the live one.
    #[test]
    fn a_name_finds_the_unrevoked_device_and_an_id_finds_any() {
        let devices = [
            device("dev_new", "laptop", false),
            device("dev_old", "laptop", true),
        ];
        assert_eq!(find_device(&devices, "Laptop").unwrap().id, "dev_new");
        assert_eq!(find_device(&devices, "dev_old").unwrap().id, "dev_old");
        assert!(find_device(&[device("dev_old", "laptop", true)], "laptop").is_none());
    }
}
