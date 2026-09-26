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
    normalize_user_code, MAX_AUTHKEY_DAYS, SCOPE_ADMIN, SCOPE_SYNC, SCOPE_WORKER,
};
use recall_wire::{ApproveRequest, AuthkeyRequest, Device};

use crate::project as proj;

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

/// Runs one `recall devices` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => return Ok(refuse(&why, "")),
    };
    let result = match cmd {
        Cmd::List { json } => list(&cfg, &client, json).await,
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
            approve(&client, &code, scope, fingerprint.as_deref(), yes, json).await
        }
        Cmd::Revoke { name, yes, json } => revoke(&cfg, &client, &name, yes, json).await,
    };
    Ok(finish(result))
}

/// Runs one `recall authkey` command.
pub async fn run_authkey(cmd: KeyCmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => return Ok(refuse(&why, "")),
    };
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
                tag.unwrap_or_default(),
                &expires,
                max_devices,
                persistent,
                json,
            )
            .await
        }
        KeyCmd::List { json } => list_keys(&client, json).await,
        KeyCmd::Revoke {
            id,
            revoke_devices,
            json,
        } => revoke_key(&client, &id, revoke_devices, json).await,
    };
    Ok(finish(result))
}

/// The exit code, having said what went wrong when something did.
fn finish(result: Done) -> i32 {
    match result {
        Ok(code) => code,
        Err(Failed::Refused(code)) => code,
        Err(Failed::Server(e)) => server_error(&e),
        Err(Failed::Json(e)) => refuse(&format!("could not write JSON: {e}"), ""),
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

/// What the server said, in a line, with what to do about the answers a
/// person can do something about.
fn server_error(e: &client::Error) -> i32 {
    let reason = e.reason();
    let then = match e {
        client::Error::Status { code: 403, .. } => {
            "This machine's device has the sync scope. Run this on an admin device, or with the \
             server's RECALL_TOKEN set."
        }
        _ if e.device_gone() => "Run recall connect to enrol this machine again.",
        client::Error::Status { code: 409, .. } if e.reason().contains("already exists") => {
            "Nothing was approved. Revoke the device with that name first (recall devices \
             revoke <name>), or have the machine enrol under another name."
        }
        client::Error::Transport(_) => "Check the server is up: recall doctor",
        _ => "",
    };
    eprintln!("recall devices: {reason}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    match e {
        client::Error::Status { .. } => exit::SERVER,
        _ => exit::CONFIG,
    }
}

/// Stops with a reason, and what to do when there is something.
fn refuse(what: &str, then: &str) -> i32 {
    eprintln!("recall devices: {what}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    exit::CONFIG
}

fn refused(what: &str, then: &str) -> Done {
    Err(Failed::Refused(refuse(what, then)))
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
            "needs a terminal to ask first.",
            "In a script, pass --yes.",
        )));
    }
    cliclack::confirm(question)
        .initial_value(false)
        .interact()
        .map_err(|_| Failed::Refused(exit::CONFIG))
}

async fn list(cfg: &ClientConfig, client: &Client, json: bool) -> Done {
    let list = client.devices().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(exit::OK);
    }
    if list.devices.is_empty() {
        println!("No devices yet. recall connect enrols this machine.");
        return Ok(exit::OK);
    }
    let this = cfg.device.as_ref().map(|d| d.device_id.as_str());
    let rows: Vec<[String; 6]> = list
        .devices
        .iter()
        .map(|d| {
            let mut note = Vec::new();
            if Some(d.id.as_str()) == this {
                note.push("this machine".to_string());
            }
            if let Some(at) = &d.revoked_at {
                note.push(format!("revoked {}", day(at)));
            }
            [
                d.name.clone(),
                d.scope.clone(),
                if d.ephemeral { "yes" } else { "no" }.to_string(),
                d.last_seen.as_deref().map_or("never".to_string(), ago),
                d.agent.clone(),
                note.join(", "),
            ]
        })
        .collect();
    table(
        &["NAME", "SCOPE", "EPHEMERAL", "LAST SEEN", "AGENT", ""],
        &rows,
    );
    Ok(exit::OK)
}

async fn approve(
    client: &Client,
    code: &str,
    scope: &str,
    expected: Option<&str>,
    yes: bool,
    json: bool,
) -> Done {
    let Some(code) = normalize_user_code(code) else {
        return refused(
            &format!("{code:?} is not a code."),
            "A code is the eight letters the machine shows, such as WDJB-MJHT.",
        );
    };
    let pending = client.pending(&code).await?;

    // Shown on stderr, so `--json` leaves stdout for the result alone.
    eprintln!("Code         {}", pending.user_code);
    eprintln!("Name         {}", pending.name);
    eprintln!(
        "Agent        {}",
        if pending.agent.is_empty() {
            "(none given)"
        } else {
            &pending.agent
        }
    );
    eprintln!("Fingerprint  {}", pending.fingerprint);
    eprintln!("Expires in   {}", minutes(pending.expires_in));

    if let Some(expected) = expected {
        if !same_fingerprint(expected, &pending.fingerprint) {
            return refused(
                &format!(
                    "the machine waiting with {code} has fingerprint {}, not {}. Nothing was \
                     approved.",
                    pending.fingerprint,
                    expected.trim()
                ),
                "Someone else may have enrolled with this code. Check the code on the machine.",
            );
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
        println!(
            "Approved {} ({}). It finishes connecting by itself within a few seconds.",
            device.name, device.scope
        );
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

async fn revoke(cfg: &ClientConfig, client: &Client, name: &str, yes: bool, json: bool) -> Done {
    let list = client.devices().await?;
    let Some(device) = find_device(&list.devices, name) else {
        return refused(
            &format!("no device named {name} is enrolled."),
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
        println!(
            "Revoked {}. Its requests are refused from now on.",
            revoked.name
        );
        if this {
            println!("That was this machine: recall connect enrols it again.");
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
    tag: String,
    expires: &str,
    max_devices: Option<u32>,
    persistent: bool,
    json: bool,
) -> Done {
    let Some(days) = days(expires) else {
        return refused(
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
    println!(
        "Authkey {}{}, expires {}",
        created.id,
        if created.tag.is_empty() {
            String::new()
        } else {
            format!(" (tag {})", created.tag)
        },
        day(&created.expires_at)
    );
    println!();
    println!("  {}", created.key);
    println!();
    println!("This is the only time it is shown: the server keeps only its hash.");
    println!("Put it in your cloud environment's variables as RECALL_AUTHKEY.");
    println!(
        "Anyone holding it can enrol a machine that reads and writes your memory, until it \
         expires or: recall authkey revoke {}",
        created.id
    );
    Ok(exit::OK)
}

async fn list_keys(client: &Client, json: bool) -> Done {
    let list = client.authkeys().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(exit::OK);
    }
    if list.authkeys.is_empty() {
        println!("No authkeys. recall authkey create --tag cloud --expires 90d makes one.");
        return Ok(exit::OK);
    }
    let rows: Vec<[String; 6]> = list
        .authkeys
        .iter()
        .map(|k| {
            [
                k.id.clone(),
                k.tag.clone(),
                if k.ephemeral { "yes" } else { "no" }.to_string(),
                k.max_devices.map_or("-".to_string(), |n| n.to_string()),
                day(&k.expires_at),
                k.revoked_at
                    .as_deref()
                    .map_or(String::new(), |at| format!("revoked {}", day(at))),
            ]
        })
        .collect();
    table(&["ID", "TAG", "EPHEMERAL", "MAX", "EXPIRES", ""], &rows);
    Ok(exit::OK)
}

async fn revoke_key(client: &Client, id: &str, revoke_devices: bool, json: bool) -> Done {
    let key = client.revoke_authkey(id.trim(), revoke_devices).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&key)?);
        return Ok(exit::OK);
    }
    println!("Revoked authkey {}: it enrols nothing more.", key.id);
    if revoke_devices {
        println!("Every device it enrolled is revoked too.");
    } else {
        println!(
            "Devices it already enrolled keep working; --revoke-devices revokes them as well."
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

/// `2026-09-23`: the date part of the API's timestamps, which is all a
/// person needs to know about an expiry or a revocation.
pub(crate) fn day(stamp: &str) -> String {
    stamp.split('T').next().unwrap_or(stamp).to_string()
}

/// How long ago a timestamp in the API's format was, in the largest unit
/// that is not zero.
pub(crate) fn ago(stamp: &str) -> String {
    let Some(age) = crate::doctor::age_of(stamp) else {
        return day(stamp);
    };
    let minutes = age.whole_minutes();
    match minutes {
        m if m < 1 => "just now".to_string(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

/// `14 min`, from seconds.
fn minutes(seconds: u64) -> String {
    match seconds / 60 {
        0 => "under a minute".to_string(),
        m => format!("{m} min"),
    }
}

/// Columns padded to their widest cell, the last one left unpadded.
pub(crate) fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
    let mut width = [0usize; N];
    for (i, h) in header.iter().enumerate() {
        width[i] = h.chars().count();
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            width[i] = width[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<w$}  ", w = width[i]));
            }
        }
        println!("{}", out.trim_end());
    };
    line(header.iter().map(|h| h.to_string()).collect());
    for row in rows {
        line(row.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
