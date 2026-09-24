//! Answering "where am I, what is this project called, and what does the
//! environment say" — once, for every command that needs it.
//!
//! The "what does the environment say" half is not `std::env`. Claude Code
//! applies the `env` block of the settings files in scope to the hooks it
//! spawns, replacing what the shell exported, so the environment a hook runs
//! under and the one an interactive shell holds are different things.
//! Resolving through [`Resolved`] is what keeps `recall status` describing
//! the first rather than the second.

use std::path::PathBuf;
use std::process::Command;

use recall_hooks::{
    claude, client, declared_env, device, home, project, scope, ClientConfig, Context,
};

/// How long the hooks wait for `device.key`'s lock:
/// [`home::HOOK_LOCK_WAIT`], inside Claude Code's hook timeout, and a
/// second in this crate's tests, so that one can tell it from the hundred
/// seconds a command waits.
const HOOK_LOCK_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    home::HOOK_LOCK_WAIT
};

/// The project root, resolved the way Claude Code resolves it: the git root,
/// falling back to the working directory.
pub fn root() -> PathBuf {
    git_toplevel().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The `origin` remote, or an empty string outside a repository — which
/// makes [`project::key`] fall back to a path-derived key.
pub fn remote() -> String {
    git(&["remote", "get-url", "origin"]).unwrap_or_default()
}

/// The git root, or [`None`] if the working directory isn't in a
/// repository. Distinct from [`root`], which always answers.
pub fn git_root() -> Option<PathBuf> {
    git_toplevel()
}

/// `git rev-parse --show-toplevel`, with its separators made native.
///
/// Git prints `/`-separated paths unconditionally, on every platform
/// including Windows — a stable property of git's own output, not an
/// artifact of this checkout. Everything downstream of [`root`]/[`git_root`]
/// either joins onto it with [`Path::join`](std::path::Path::join), prints
/// it with `display()`, or compares it against a canonicalized path, and all
/// three expect `\` on Windows: left as `/`, a join like
/// `.claude/settings.json` (a single string containing its own separators)
/// does not split into components at all when the base is later
/// canonicalized to a verbatim (`\\?\`) path, and every other join produces
/// the mixed `C:/Users/...\.claude\...` that `recall status` used to print.
/// The project-slug rule this feeds is unaffected either way: `slug()` maps
/// `:`, `/` and `\` to one dash each, see `claude.rs`.
fn git_toplevel() -> Option<PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(native_separators(s)))
}

#[cfg(windows)]
fn native_separators(path: String) -> String {
    path.replace('/', "\\")
}

#[cfg(not(windows))]
fn native_separators(path: String) -> String {
    path
}

/// Whether this is a remote or cloud session, per `CLAUDE_CODE_REMOTE`.
///
/// Read from the process environment rather than through the settings
/// layers: it describes the harness the command is running under, and a
/// settings file claiming otherwise would be describing something it cannot
/// know. The same signal `.claude/hooks/session-start.sh` keys off.
pub fn remote_session() -> bool {
    matches!(
        std::env::var("CLAUDE_CODE_REMOTE").ok().as_deref(),
        Some("true") | Some("1")
    )
}

/// Where a command is, and what the environment says there.
///
/// One type rather than three lookups because the three used to disagree:
/// `recall push` read the Claude variables once for the memory directory and
/// again for its context, and `recall status` read the process environment
/// while the hooks it was reporting on ran under a different one.
pub struct Resolved {
    /// The project root.
    pub root: PathBuf,
    /// The settings files layered over the shell, as Claude Code layers
    /// them. Kept so `recall status` can say *where* a value came from.
    pub env: declared_env::Environment,
}

/// Resolves the current directory's project and environment.
///
/// Never fails: every command here has to be useful on a machine where
/// nothing is configured yet, which is exactly when someone runs them.
pub fn resolve() -> Resolved {
    resolve_at(root())
}

/// The same, for a project root the caller already knows.
///
/// `recall init` takes one on the command line, and resolving the current
/// directory instead would report on a different project than the one it
/// just wired.
pub fn resolve_at(root: PathBuf) -> Resolved {
    let env = declared_env::Environment::discover(&root);
    Resolved { root, env }
}

impl Resolved {
    /// The memory directory Claude Code uses for this project on this
    /// machine.
    ///
    /// `recall push` asks this before it asks whether a server is configured
    /// at all, so that a machine which has cloned a wired project without
    /// being set up yet — the exact case Recall exists for — does not report
    /// a missing token on every unrelated file the user touches.
    pub fn memory_dir(&self) -> PathBuf {
        claude::Env::from_lookup(self.env.lookup()).memory_dir(&self.root.to_string_lossy())
    }

    /// The root every project's memory lives under, which is the same for
    /// all of them. Needed to tell "not a memory file" apart from "memory
    /// for a project that is not this one".
    pub fn memory_root(&self) -> PathBuf {
        claude::Env::from_lookup(self.env.lookup()).memory_root()
    }

    /// Recall's configuration, resolved through [`Resolved::env`].
    ///
    /// Built on demand rather than alongside the rest, and that is
    /// load-bearing: resolving `source_env` falls back to the hostname,
    /// which may fork a `hostname(1)`. `recall push` asks for
    /// [`Resolved::memory_dir`] on every Edit and Write *before* it knows
    /// whether the file concerns it at all, and that path must stay free of
    /// work this size.
    pub fn config(&self) -> ClientConfig {
        // 0.3.0's `credentials.json` becomes the two TOML files, once. Here
        // because every command that reads configuration comes through this,
        // hooks included, so no machine is left on the old file for want of
        // running one particular command. Failing is not fatal: the loader
        // still reads the old file, and `recall doctor` names the problem.
        if let Some(h) = home::locate(self.env.lookup()) {
            let _ = h.migrate_legacy();
        }
        ClientConfig::from_lookup(self.env.lookup())
    }

    /// The key this project syncs under: the declared one if there is a
    /// usable one, otherwise derived from `remote`.
    ///
    /// Takes the configuration rather than building its own, so a caller
    /// that already has one does not pay for a second.
    pub fn project_key(&self, cfg: &ClientConfig, remote: &str) -> String {
        project::key_with_override(
            cfg.project_key.as_deref(),
            remote,
            &self.root.to_string_lossy(),
        )
    }

    /// Everything the hook commands need.
    ///
    /// Fails when the server is not configured, which is the one thing a
    /// hook cannot work around.
    pub fn hook_context(&self) -> anyhow::Result<Context> {
        self.hook_context_for(&self.config())
    }

    /// The same, from a configuration the caller already holds: one a hook
    /// has just enrolled a device into, say.
    pub fn hook_context_for(&self, cfg: &ClientConfig) -> anyhow::Result<Context> {
        // The client first: while `device.key` cannot be read, that is why
        // nothing can be sent, and a missing token is not.
        let client = cfg.client()?;
        cfg.require()?;

        let root_str = self.root.to_string_lossy().to_string();
        Ok(Context {
            memory_dir: cfg.claude.memory_dir(&root_str),
            state_file: cfg.claude.state_file(&root_str),
            scopes: scope::scopes(
                self.project_key(cfg, &remote()),
                cfg.global_key.clone(),
                cfg.machine_key.clone(),
            ),
            source_env: cfg.source_env.clone(),
            client,
        })
    }

    /// Makes `device.key` readable by its owner only when something has
    /// widened it, and says so: the hook that narrows it is the one that
    /// warns, so it is said once.
    ///
    /// Mended rather than refused. ssh refuses a private key others can
    /// read and leaves the fix to the person who ran it, who is right there;
    /// a hook has nobody there, and refusing would stop every sync over
    /// something one `chmod` mends. What the `chmod` cannot mend is a copy
    /// someone already took, which is why it is said rather than done
    /// quietly.
    pub fn protect_device_key(&self, cfg: &ClientConfig, hook: &str) {
        let Some(path) = cfg.device_file.as_deref() else {
            return;
        };
        let shown = || crate::ui::tilde(&path.display().to_string());
        match home::restrict_to_owner(path) {
            Ok(false) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(true) => {
                eprintln!(
                    "{hook}: {} was readable by other users; it is now readable by you only",
                    shown()
                );
                eprintln!(
                    "{hook}:   if anyone else can log in to this machine, revoke its device \
                     (recall devices revoke <name>) and run recall connect"
                );
            }
            Err(e) => eprintln!(
                "{hook}: could not make sure {} is readable by you only: {e}",
                shown()
            ),
        }
    }

    /// Enrols this machine with `RECALL_AUTHKEY` when it has no device
    /// key for the server in effect: a cloud session's first hook.
    ///
    /// Never an error, because it runs inside a hook. What happened is one
    /// line on stderr, prefixed with `hook`, and the configuration comes
    /// back either holding the new device or as it was, in which case the
    /// token, if there is one, is used as before.
    ///
    /// Done holding the device lock, and looked for again once it is held:
    /// hooks run at once, and a new session's first few edits would
    /// otherwise each enrol a device of their own.
    pub async fn enroll_if_needed(&self, cfg: ClientConfig, hook: &str) -> ClientConfig {
        let Some(authkey) = cfg.authkey.clone() else {
            return cfg;
        };
        // With `device.key` unreadable there is no telling whether it holds
        // this server's key, and a new one could not be saved beside it: the
        // device would be enrolled on the server and lost here.
        // `ClientConfig::client` refuses, and says why.
        if cfg.device.is_some() || cfg.url.is_empty() || cfg.device_error.is_some() {
            return cfg;
        }
        let Some(h) = home::locate(self.env.lookup()) else {
            eprintln!("{hook}: RECALL_AUTHKEY is set, but there is no home directory to keep a device key in");
            return cfg;
        };
        let held = match h.lock_devices_within(HOOK_LOCK_WAIT) {
            Ok(held) => held,
            Err(e) => {
                eprintln!(
                    "{hook}: could not enrol with RECALL_AUTHKEY ({e}){}",
                    fallback(&cfg)
                );
                return cfg;
            }
        };
        self.enroll_holding(&held, cfg, &authkey, hook).await
    }

    /// [`Resolved::enroll_if_needed`]'s work, once the lock is held.
    async fn enroll_holding(
        &self,
        held: &home::DevicesLock<'_>,
        mut cfg: ClientConfig,
        authkey: &str,
        hook: &str,
    ) -> ClientConfig {
        // Another hook may have enrolled while this one waited.
        match held.load() {
            Ok(devices) => {
                if let Some(entry) = devices.for_url(&cfg.url) {
                    cfg.device = Some(entry.clone());
                    return cfg;
                }
            }
            Err(e) => {
                // Damaged since the configuration was read: the same as
                // finding it damaged then, nothing sent in its place.
                eprintln!("{hook}: could not enrol with RECALL_AUTHKEY ({e})");
                cfg.device_error = Some(e.to_string());
                return cfg;
            }
        }
        match device::enrol_with_authkey(held, &cfg.url, authkey, &cfg.source_env).await {
            Ok(entry) => {
                eprintln!(
                    "{hook}: enrolled this session as device {} with RECALL_AUTHKEY",
                    entry.name
                );
                cfg.device = Some(entry);
            }
            Err(e) => {
                eprintln!(
                    "{hook}: could not enrol with RECALL_AUTHKEY ({}){}",
                    enroll_failure(&e),
                    fallback(&cfg)
                );
            }
        }
        cfg
    }

    /// After the server refused this machine's device (`refusal` is one of
    /// the two refusals [`client::Error::device_gone`] names): the
    /// configuration to retry with, when there is one, and otherwise a line
    /// or two on stderr saying what to do.
    ///
    /// - Another hook has already enrolled again, so the device saved now is
    ///   not the one refused: retry as that one.
    /// - Revoked: never enrolled again, and the key is left where it is. A
    ///   revocation is the owner cutting this machine off, and one that a
    ///   hook answered by enrolling again with `RECALL_AUTHKEY` would cut
    ///   off nothing that holds one, an admin laptop included.
    /// - Unknown, and ephemeral, with `RECALL_AUTHKEY`: a cloud session's
    ///   device swept away after sitting idle, which is what the authkey is
    ///   for. The old key is forgotten, for this server only, and a new one
    ///   enrolled.
    /// - Unknown otherwise: says to run `recall connect` and keeps the key.
    ///   Only a person drops a device that was not made to be thrown away.
    ///
    /// All of it holding the device lock, so that hooks refused at once
    /// enrol one device between them.
    pub async fn after_refusal(
        &self,
        cfg: &ClientConfig,
        hook: &str,
        refusal: &client::Error,
    ) -> Option<ClientConfig> {
        let refused = cfg.device.as_ref()?;
        let why = refusal.reason();
        let why = why.trim_start_matches("unauthorized: ");
        let h = home::locate(self.env.lookup())?;
        let held = match h.lock_devices_within(HOOK_LOCK_WAIT) {
            Ok(held) => held,
            Err(e) => {
                eprintln!("{hook}: the server refused this machine's device ({why}), and {e}");
                return None;
            }
        };
        let saved = match held.load() {
            Ok(devices) => devices.for_url(&cfg.url).cloned(),
            Err(e) => {
                eprintln!("{hook}: the server refused this machine's device ({why}), and {e}");
                return None;
            }
        };
        if let Some(saved) = saved.filter(|s| s.device_id != refused.device_id) {
            let mut retry = cfg.clone();
            retry.device = Some(saved);
            return Some(retry);
        }

        if refusal.device_revoked() {
            eprintln!(
                "{hook}: the server refused this machine's device {} ({why}), and it is not \
                 enrolled again by itself",
                refused.name
            );
            let then = if remote_session() {
                "a new session enrols afresh with RECALL_AUTHKEY, unless that authkey is revoked \
                 too"
            } else {
                "if it should be, run recall connect to enrol this machine again"
            };
            eprintln!("{hook}:   {then}");
            return None;
        }
        let authkey = match (&cfg.authkey, refused.ephemeral) {
            (Some(authkey), true) => authkey.clone(),
            (None, true) => {
                eprintln!("{hook}: the server no longer accepts this machine's device key ({why})");
                eprintln!(
                    "{hook}:   run recall connect to enrol this machine again, or set \
                     RECALL_AUTHKEY to have a cloud session do it by itself"
                );
                return None;
            }
            (_, false) => {
                eprintln!(
                    "{hook}: the server no longer knows this machine's device {} ({why})",
                    refused.name
                );
                eprintln!("{hook}:   run recall connect to enrol this machine again");
                return None;
            }
        };
        if let Err(e) = held.forget_device(&cfg.url) {
            eprintln!("{hook}: the server no longer knows this session's device ({why}), and {e}");
            return None;
        }
        eprintln!(
            "{hook}: the server no longer knows this session's device ({why}), enrolling again"
        );
        let mut fresh = cfg.clone();
        fresh.device = None;
        let fresh = self.enroll_holding(&held, fresh, &authkey, hook).await;
        fresh.device.is_some().then_some(fresh)
    }
}

/// What a failed enrolment falls back to, for the end of the line that
/// says so.
fn fallback(cfg: &ClientConfig) -> &'static str {
    if cfg.token.is_empty() {
        ""
    } else {
        ", using RECALL_TOKEN instead"
    }
}

/// Why an enrolment with an authkey failed, in a line.
fn enroll_failure(e: &device::Error) -> String {
    match e {
        device::Error::Client(recall_hooks::client::Error::Status { code: 404, .. }) => {
            "this server does not enrol devices; it is older than 0.4.1".to_string()
        }
        device::Error::Client(c) => c.reason(),
        other => other.to_string(),
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A hook waits for `device.key`'s lock no longer than
    /// [`home::HOOK_LOCK_WAIT`], well inside Claude Code's sixty-second hook
    /// timeout; the hundred seconds a command waits are not. Mutation: take
    /// the command's wait in the hooks, as before.
    #[test]
    fn a_hook_waits_briefly_for_the_device_lock() {
        const { assert!(home::HOOK_LOCK_WAIT.as_secs() < 60) };
        let dir = tempfile::tempdir().unwrap();
        let recall = dir.path().join(".recall");
        std::fs::create_dir(&recall).unwrap();
        std::fs::write(recall.join("device.key.lock"), "another hook").unwrap();
        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let home_dir = recall.display().to_string();
            let shell = move |name: &str| (name == "RECALL_HOME").then(|| home_dir.clone());
            let env = declared_env::Environment::from_files(&[], Box::new(shell));
            let here = Resolved { root, env };
            let cfg = ClientConfig {
                url: "http://127.0.0.1:9".into(),
                authkey: Some("rk_test".into()),
                ..Default::default()
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let enrolled = runtime.block_on(here.enroll_if_needed(cfg.clone(), "recall-pull"));
            let _ = tx.send(enrolled.device.is_none());

            // And after a refusal, the other hook path that takes it.
            let key = device::DeviceKey::generate().unwrap();
            let refused = ClientConfig {
                device: Some(key.entry("dev_x", "jarvis", "sync", true)),
                ..cfg
            };
            let refusal = client::Error::Status {
                code: 401,
                body: r#"{"error":"unauthorized: unknown device"}"#.into(),
            };
            let retry = runtime.block_on(here.after_refusal(&refused, "recall-pull", &refusal));
            let _ = tx.send(retry.is_none());
        });
        for path in ["enrolling", "after a refusal"] {
            let gave_up = rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("the hook stopped waiting, {path}"));
            assert!(gave_up, "and did nothing, {path}");
        }
    }
}
