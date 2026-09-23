//! `recall connect` and `recall disconnect` — setting a machine up, and
//! taking its token away again.
//!
//! `connect` is the one command a new machine needs. It walks through what
//! used to be separate steps — the token, this machine's name, wiring the
//! current project, its first sync — and skips each one that has nothing to
//! do: a saved token that still works is not asked for again, and a project
//! already wired is not offered.
//!
//! Against a server that enrols devices (0.4.1 and later), the token step
//! becomes enrolment: the machine makes a key pair, shows a code and the
//! key's fingerprint, and is approved from a machine already trusted, or,
//! for the first one, with the operator's `RECALL_TOKEN` after asking. Once
//! enrolled it signs its requests and the saved token is removed.
//!
//! What lives here is the part a person sees. The files, and why there are
//! several of them, are `recall_hooks::home`'s.

use std::io::{self, IsTerminal};
use std::path::Path;

use recall_hooks::client::{self, Client, Enrolled, Poll};
use recall_hooks::config::Source;
use recall_hooks::device::{DeviceKey, Signer};
use recall_hooks::home::{self, Home};
use recall_hooks::{backfill, exit, settings, Disposition};
use recall_wire::devices::SCOPE_ADMIN;
use recall_wire::discovery::AUTH_DEVICE_SIG;
use recall_wire::{ApproveRequest, DeviceIdentity, EnrollPending, EnrollPollResponse};

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
            "Set RECALL_URL and RECALL_ENROLL_KEY (or RECALL_TOKEN) on the cloud environment \
             instead.",
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
    let (creds, config) = match load_both(&h) {
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
    // has RECALL_TOKEN. A machine with a device key needs no token, and one
    // told `--yes` may enrol and wait for someone to approve it. Decided
    // before the network is touched.
    let saved = creds.token_for(&url).map(str::to_string);
    let saved_device = h
        .load_devices()
        .ok()
        .flatten()
        .and_then(|d| d.for_url(&url).cloned());
    if saved.is_none() && saved_device.is_none() && !interactive && !args.yes {
        return refuse(
            "needs a terminal to ask for the token.",
            "In a script, set RECALL_TOKEN instead, or pass --yes to enrol this machine and \
             wait for it to be approved.",
        );
    }

    intro();
    if url.starts_with("http://") && !is_loopback(&url) {
        say_warning(&format!(
            "{} uses plain http, so the token is sent unencrypted",
            host(&url)
        ));
    }

    let enrols = reach(&url).await?;
    let setup = Setup {
        args: &args,
        here: &here,
        home: &h,
        url: &url,
        interactive,
    };
    let name = if enrols {
        match setup
            .enroll(creds, config, saved.clone(), saved_device)
            .await?
        {
            Some(name) => name,
            // Kept on the token: enrolling needs a confirmation, and
            // there is nobody to give it.
            None => setup.with_token(load_both_or_stop(&h)?, saved).await?,
        }
    } else {
        setup.with_token((creds, config), saved).await?
    };

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

/// What every step of setting up the connection needs to know.
struct Setup<'a> {
    args: &'a Args,
    here: &'a proj::Resolved,
    home: &'a Home,
    url: &'a str,
    interactive: bool,
}

/// How a machine being enrolled will be approved.
enum Approval {
    /// By this command, with the operator's token: the first device.
    Token(String),
    /// By someone else, from a machine already trusted.
    Elsewhere,
}

impl Setup<'_> {
    /// The shared token, as before devices: asked for or checked, then
    /// saved. What happens against a server that does not enrol devices.
    async fn with_token(
        &self,
        (mut creds, mut config): (home::Credentials, home::Config),
        saved: Option<String>,
    ) -> Step<String> {
        let url = self.url;
        if saved.is_none() && !self.interactive {
            return refuse(
                "needs a terminal to ask for the token.",
                "In a script, set RECALL_TOKEN instead.",
            );
        }
        let token = match saved {
            Some(token) if verify(url, &token, "saved token").await? => token,
            Some(_) => {
                if !self.interactive {
                    return refuse(
                        "needs a new token, but there is no terminal to ask for one.",
                        "Run recall connect in a terminal.",
                    );
                }
                ask_token(url).await?
            }
            None => ask_token(url).await?,
        };

        let name = machine_name(self.args, &config, self.here, self.interactive)?;

        creds.insert(url, &token);
        config.server = Some(url.to_string());
        config.machine.name = Some(name.clone());
        if let Err(e) = self
            .home
            .save_credentials(&creds)
            .and_then(|()| self.home.save_config(&config))
        {
            return refuse(
                &format!("the token is valid, but saving it failed: {e}"),
                "",
            );
        }
        say_success(&format!(
            "Saved to {}",
            ui::tilde(&self.home.dir().display().to_string())
        ));
        Ok(name)
    }

    /// Makes this machine a device of a server that enrols them, or
    /// confirms it already is one. The machine's name, when it is set up;
    /// [`None`] when it should stay on the token, because enrolling needs a
    /// confirmation nobody is there to give.
    async fn enroll(
        &self,
        creds: home::Credentials,
        config: home::Config,
        saved: Option<String>,
        saved_device: Option<home::DeviceEntry>,
    ) -> Step<Option<String>> {
        let url = self.url;

        // Already a device here: nothing to enrol, as long as the server
        // still agrees.
        if let Some(entry) = saved_device {
            match check_device(url, &entry).await? {
                Some(me) => {
                    let entry = home::DeviceEntry {
                        name: me.name,
                        scope: me.scope,
                        ephemeral: me.ephemeral,
                        ..entry
                    };
                    let name = machine_name(self.args, &config, self.here, self.interactive)?;
                    self.save_enrolled(creds, config, entry, &name)?;
                    return Ok(Some(name));
                }
                None => {
                    // Gone for good: revoked, or unknown to the server. The
                    // old key is dropped and a new one enrolled.
                    let _ = self.home.forget_device(url);
                }
            }
        }

        let cfg = self.here.config();
        let env_token = (cfg.token_source == Source::Environment
            && home::normalize_url(&cfg.url) == url)
            .then(|| cfg.token.clone());
        let approval = match self.approval(saved, env_token).await? {
            Some(approval) => approval,
            None => return Ok(None),
        };
        let name = machine_name(self.args, &config, self.here, self.interactive)?;

        let key = match DeviceKey::generate() {
            Ok(key) => key,
            Err(e) => return refuse(&e.to_string(), "Nothing was saved."),
        };
        let fingerprint = key.fingerprint();
        let open = match Client::new(url, "") {
            Ok(client) => client,
            Err(e) => return refuse(&e.to_string(), "Nothing was saved."),
        };
        let pending = match open.enroll(&key.enroll_request(&name, None)).await {
            Ok(Enrolled::Pending(pending)) => pending,
            Ok(Enrolled::Approved(_)) => {
                return refuse(
                    "the server approved this machine without anyone approving it.",
                    "Nothing was saved. That should not happen without an enrolment key.",
                )
            }
            Err(e @ client::Error::Status { code: 409, .. }) => {
                return refuse(
                    &format!("{}.", e.reason()),
                    &format!(
                        "On an admin device: recall devices revoke {name}. Or enrol under \
                         another name: recall connect --name {name}-2"
                    ),
                )
            }
            Err(e) => {
                return refuse(
                    &format!("could not start enrolling: {}", e.reason()),
                    "Nothing was saved.",
                )
            }
        };

        let _ = cliclack::log::info(format!(
            "Enrolling this machine as {name}\n\
             Code         {}\n\
             Fingerprint  {fingerprint}",
            pending.user_code
        ));

        let approved = match approval {
            Approval::Token(token) => {
                self_approve(url, &token, &pending.user_code, &fingerprint).await?;
                match open.poll(&pending.enrollment_id).await {
                    Ok(Poll::Approved(approved)) => approved,
                    Ok(other) => {
                        return refuse(
                            &format!("approved, but the server then answered {other:?}."),
                            "Run recall connect again.",
                        )
                    }
                    Err(e) => {
                        return refuse(
                            &format!("approved, but asking for the result failed: {}", e.reason()),
                            "Run recall connect again.",
                        )
                    }
                }
            }
            Approval::Elsewhere => {
                let _ = cliclack::note(
                    "Approve it from a machine enrolled as admin, or one holding the server's \
                     RECALL_TOKEN",
                    format!(
                        "recall devices approve {}\n\
                         It shows the fingerprint: check it is the one above.",
                        pending.user_code
                    ),
                );
                wait_for_approval(&open, &pending).await?
            }
        };

        let entry = key.entry(&approved.device_id, &name, &approved.scope, false);
        self.save_enrolled(creds, config, entry, &name)?;
        Ok(Some(name))
    }

    /// Decides how the machine is to be approved, asking when there is
    /// someone to ask. [`None`]: not now, stay on the token.
    async fn approval(
        &self,
        saved: Option<String>,
        env_token: Option<String>,
    ) -> Step<Option<Approval>> {
        let url = self.url;
        let (token, what) = match (saved, env_token) {
            (Some(t), _) => (Some(t), "saved token"),
            (None, Some(t)) => (Some(t), "RECALL_TOKEN"),
            (None, None) => (None, ""),
        };
        let confirm = "Approve this machine with the server's RECALL_TOKEN, as an admin device? \
                       That is how your first machine is approved";

        if let Some(token) = token {
            // Nobody to confirm, and not told to go ahead: this machine
            // stays on the token, as it would have before devices.
            if !self.interactive && !self.args.yes {
                say_warning(
                    "Not enrolled: that needs a confirmation. Run recall connect in a \
                     terminal, or with --yes, to enrol this machine",
                );
                return Ok(None);
            }
            if verify(url, &token, what).await? {
                if self.args.yes {
                    say_step("Approving this machine with RECALL_TOKEN, as an admin device");
                    return Ok(Some(Approval::Token(token)));
                }
                let yes = answer(cliclack::confirm(confirm).initial_value(true).interact())?;
                return Ok(Some(if yes {
                    Approval::Token(token)
                } else {
                    Approval::Elsewhere
                }));
            }
            if !self.interactive {
                return refuse(
                    "needs a new token, but there is no terminal to ask for one.",
                    "Run recall connect in a terminal.",
                );
            }
        }

        if !self.interactive {
            // `--yes` with no token: someone approves it from elsewhere.
            return Ok(Some(Approval::Elsewhere));
        }
        let choice = answer(
            cliclack::select("How will this machine be approved?")
                .item(
                    "elsewhere",
                    "From a machine already enrolled as admin",
                    "recall devices approve <code> there",
                )
                .item(
                    "token",
                    "With the server's RECALL_TOKEN, as an admin device",
                    "your first machine",
                )
                .interact(),
        )?;
        if choice == "token" {
            return Ok(Some(Approval::Token(ask_token(url).await?)));
        }
        Ok(Some(Approval::Elsewhere))
    }

    /// Saves an approved device, then everything that follows from it: the
    /// config names the server and the machine, and the shared token, no
    /// longer sent, is no longer kept.
    ///
    /// The key is saved first: an approval is the one thing here that
    /// cannot simply be done again.
    fn save_enrolled(
        &self,
        mut creds: home::Credentials,
        mut config: home::Config,
        entry: home::DeviceEntry,
        name: &str,
    ) -> Step<()> {
        let url = self.url;
        let (device, scope) = (entry.name.clone(), entry.scope.clone());
        if let Err(e) = self.home.save_device(url, entry) {
            return refuse(
                &format!("approved, but saving the device key failed: {e}"),
                &format!("On an admin device: recall devices revoke {device}, then run recall connect again."),
            );
        }
        config.server = Some(url.to_string());
        config.machine.name = Some(name.to_string());
        if let Err(e) = self.home.save_config(&config) {
            return refuse(
                &format!("the device key is saved, but the config is not: {e}"),
                "",
            );
        }
        say_success(&format!(
            "Enrolled as {device} ({scope}), key saved in {}",
            ui::tilde(&self.home.device_path().display().to_string())
        ));

        if creds.remove(url) {
            let saved = if creds.servers.is_empty() {
                self.home.delete_credentials()
            } else {
                self.home.save_credentials(&creds)
            };
            match saved {
                Ok(()) => {
                    let _ = cliclack::log::remark(format!(
                        "Removed the shared token from {}: this machine signs its requests \
                         now. The token still works on the server; rotating RECALL_TOKEN \
                         there is the operator's call.",
                        ui::tilde(&self.home.credentials_path().display().to_string())
                    ));
                }
                Err(e) => say_warning(&format!(
                    "could not remove the shared token, which is no longer needed: {e}"
                )),
            }
        }
        Ok(())
    }
}

/// Asks the server whether this machine's saved device is still one.
/// [`Some`] with what the server knows it as; [`None`] when it is revoked
/// or unknown, which only a new enrolment mends. Anything else ends the
/// flow: it says nothing about the device either way.
async fn check_device(url: &str, entry: &home::DeviceEntry) -> Step<Option<DeviceIdentity>> {
    let spinner = spin("Checking this machine's device key");
    let client = match Signer::from_entry(entry)
        .map_err(|e| e.to_string())
        .and_then(|s| {
            Client::new(url, "")
                .map(|c| c.with_signer(s))
                .map_err(|e| e.to_string())
        }) {
        Ok(client) => client,
        Err(e) => {
            spinner.error("Device key unusable");
            say_warning(&format!("{e}; enrolling again"));
            return Ok(None);
        }
    };
    match client.me().await {
        Ok(me) => {
            spinner.stop(format!("Enrolled as {} ({})", me.name, me.scope));
            Ok(Some(me))
        }
        Err(e) if e.device_gone() => {
            spinner.error("Device key no longer accepted");
            say_warning(&format!(
                "The server refused this machine's device key ({}), so it is enrolled again",
                e.reason().trim_start_matches("unauthorized: ")
            ));
            Ok(None)
        }
        Err(e) => {
            spinner.error("Couldn't check the device key");
            refuse(
                &format!("could not check this machine's device key: {}", e.reason()),
                "Nothing was changed.",
            )
        }
    }
}

/// Approves this machine's own code with the operator's token, bound to
/// the fingerprint this machine computed, so the server approves exactly
/// the key that was enrolled.
async fn self_approve(url: &str, token: &str, code: &str, fingerprint: &str) -> Step<()> {
    let spinner = spin("Approving with RECALL_TOKEN");
    let req = ApproveRequest {
        user_code: code.to_string(),
        scope: SCOPE_ADMIN.to_string(),
        fingerprint: Some(fingerprint.to_string()),
    };
    let result = match Client::new(url, token) {
        Ok(client) => client.approve(&req).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(_) => {
            spinner.stop("Approved as an admin device");
            Ok(())
        }
        Err(e) => {
            spinner.error("Approval failed");
            refuse(
                &format!("could not approve this machine: {}", e.reason()),
                "Nothing was saved. Run recall connect again.",
            )
        }
    }
}

/// Polls until someone approves the code, as RFC 8628 §3.5 says: every
/// `interval` seconds, five more after each `slow_down`, and never past
/// the code's expiry.
async fn wait_for_approval(client: &Client, pending: &EnrollPending) -> Step<EnrollPollResponse> {
    let spinner = spin(&format!("Waiting for approval of {}", pending.user_code));
    let mut interval = pending.interval.max(1);
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(pending.expires_in.max(1) + interval);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let stop = |what: &str, then: &str| {
            spinner.error("Not approved");
            refuse::<EnrollPollResponse>(what, then)
        };
        match client.poll(&pending.enrollment_id).await {
            Ok(Poll::Approved(approved)) => {
                spinner.stop(format!("Approved, {} scope", approved.scope));
                return Ok(approved);
            }
            Ok(Poll::Pending) => {}
            Ok(Poll::SlowDown) => interval += 5,
            // Rate limited: the same answer in HTTP's words.
            Err(client::Error::Status { code: 429, .. }) => interval += 5,
            Ok(Poll::Expired) => {
                return stop(
                    "the code expired before anyone approved it.",
                    "Nothing was saved. Run recall connect again for a new code.",
                )
            }
            Ok(Poll::Denied) => return stop("the enrolment was denied.", "Nothing was saved."),
            Ok(Poll::Unknown) => {
                return stop(
                    "the server no longer knows this enrolment.",
                    "Nothing was saved. Run recall connect again.",
                )
            }
            Err(e) => {
                return stop(
                    &format!("could not ask whether it was approved: {}", e.reason()),
                    "Nothing was saved. Run recall connect again.",
                )
            }
        }
        if std::time::Instant::now() > deadline {
            return stop(
                "the code expired before anyone approved it.",
                "Nothing was saved. Run recall connect again for a new code.",
            );
        }
    }
}

/// Both files again, for a flow that changed its mind about enrolling.
fn load_both_or_stop(h: &Home) -> Step<(home::Credentials, home::Config)> {
    load_both(h).or_else(|e| refuse(&e.to_string(), "Nothing was changed."))
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
///
/// Then the discovery document, for whether the server enrols devices:
/// `true` when it does, and `false` for one that does not or is too old to
/// say, which is then connected with the token as before.
async fn reach(url: &str) -> Step<bool> {
    let spinner = spin(&format!("Connecting to {}", host(url)));
    let result = match Client::new(url, "") {
        Ok(client) => match client.health().await {
            Ok(_) => Ok(match client.discover().await {
                Ok(Some(doc)) => doc.accepts(AUTH_DEVICE_SIG) && doc.devices().is_some(),
                _ => false,
            }),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok(enrols) => {
            spinner.stop(format!("Connected to {}", host(url)));
            Ok(enrols)
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
    if cfg.token_source == Source::Environment && cfg.device.is_some() {
        out.push(
            match here.env.declared(&["RECALL_TOKEN"]).into_iter().next() {
                Some(d) => format!(
                    "{} sets RECALL_TOKEN, which this machine no longer needs; remove it",
                    d.file
                ),
                None => "RECALL_TOKEN in your shell is no longer needed here, remove it from \
                         your shell profile"
                    .to_string(),
            },
        );
    } else if cfg.token_source == Source::Environment {
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
    let mut devices = match h.load_devices() {
        Ok(d) => d.unwrap_or_default(),
        Err(e) => {
            eprintln!("recall disconnect: {e}");
            eprintln!("  Nothing was changed.");
            return Ok(exit::CONFIG);
        }
    };
    let path = h.credentials_path();

    // Every server something is saved for, a token or a device key.
    let mut saved: Vec<String> = creds.servers.keys().cloned().collect();
    for u in devices.servers.keys() {
        if !saved.contains(u) {
            saved.push(u.clone());
        }
    }
    let target = match url {
        Some(u) => Some(home::normalize_url(u)),
        None => config.server.clone().or_else(|| {
            // With exactly one server there is nothing to choose between.
            (saved.len() == 1).then(|| saved[0].clone())
        }),
    };

    match target {
        None if saved.is_empty() => {
            println!("No token is saved in {}.", path.display());
        }
        None => {
            eprintln!("recall disconnect: more than one server is saved; name one:");
            for u in &saved {
                eprintln!("  recall disconnect {u}");
            }
            return Ok(exit::CONFIG);
        }
        Some(target) => {
            // The device key first, and on its own: removing it is the one
            // part that changes how this machine appears to the server.
            if let Some(device) = devices.for_url(&target).cloned() {
                devices.remove(&target);
                if let Err(e) = h.save_devices(&devices) {
                    eprintln!("recall disconnect: {e}");
                    return Ok(exit::CONFIG);
                }
                println!(
                    "Removed this machine's device key for {target} from {}.",
                    h.device_path().display()
                );
                // The key is gone from here, not from the server's list,
                // and only an admin can take it off that.
                println!(
                    "The server still lists the device {}. To revoke it: recall devices \
                     revoke {} on an admin device.",
                    device.name, device.name
                );
                if config.server.as_deref() == Some(target.as_str())
                    && creds.token_for(&target).is_none()
                {
                    config.server = None;
                    if let Err(e) = h.save_config(&config) {
                        eprintln!("recall disconnect: {e}");
                        return Ok(exit::CONFIG);
                    }
                }
            }
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
            } else if !saved.contains(&target) {
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
