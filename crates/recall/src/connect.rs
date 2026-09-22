//! `recall connect` and `recall disconnect` — putting the token somewhere
//! better than a shell profile, and taking it out again.
//!
//! The design is `cargo login`'s, and the reasoning is in
//! `recall_hooks::credentials`. What lives here is the part a person sees:
//! the prompt, the verification, and — just as important — what each
//! command says about the tokens it did *not* touch.

use std::io::IsTerminal;

use recall_hooks::client::{self, Client};
use recall_hooks::config::Source;
use recall_hooks::credentials::{self, Credentials};
use recall_hooks::exit;

use crate::project as proj;

/// `recall connect <url>`.
///
/// Verify, then write. The token is checked against `GET /health` and an
/// authenticated call before anything touches the disk, so the file means
/// "this worked" rather than "we got this far" — the same rule the off-box
/// backup stamp follows. A wrong token leaves nothing behind, and you find
/// out now rather than at the next session start.
pub async fn connect(url: &str) -> anyhow::Result<i32> {
    // The container is ephemeral: a file written here is gone with it, and
    // appearing to succeed is worse than declining. The environment's own
    // variables are the right store there, and they already win.
    if proj::remote_session() {
        eprintln!(
            "recall connect: this is a remote session, and anything saved here is \
             discarded with the container."
        );
        eprintln!(
            "  Set RECALL_URL and RECALL_TOKEN on the cloud environment instead — \
             its variables are a secret store, and they are what Recall reads there."
        );
        return Ok(exit::CONFIG);
    }

    let url = credentials::normalize_url(url);
    if !has_host(&url) {
        eprintln!("recall connect: {url:?} is not a server URL (e.g. https://recall.example.com)");
        return Ok(exit::CONFIG);
    }
    if url.starts_with("http://") && !is_loopback(&url) {
        eprintln!(
            "recall connect: warning — {url} is plain http, so the token crosses the \
             network unencrypted on every request"
        );
    }

    let here = proj::resolve();
    let Some(home) = credentials::home(here.env.lookup()) else {
        eprintln!("recall connect: no home directory to save into — set RECALL_HOME");
        return Ok(exit::CONFIG);
    };
    let path = credentials::file(&home);

    // Before the prompt, so a file this cannot read stops things before the
    // user has typed a secret — and so it is never overwritten.
    let mut saved = match credentials::load(&path) {
        Ok(saved) => saved.unwrap_or_default(),
        Err(e) => {
            eprintln!("recall connect: {e}");
            eprintln!("  Nothing was changed. Move it aside and run recall connect again.");
            return Ok(exit::CONFIG);
        }
    };

    // Deliberately a terminal and nothing else. Reading from stdin for
    // automation waits until something needs it: every other way in is
    // another way out, and automation already has RECALL_TOKEN.
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "recall connect: reads the token from a terminal, without echoing it, and \
             there is no terminal here."
        );
        eprintln!("  Where something else holds the secret, set RECALL_TOKEN instead.");
        return Ok(exit::CONFIG);
    }
    let token = match rpassword::prompt_password(format!("Token for {url}: ")) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            eprintln!("recall connect: could not read the token: {e}");
            return Ok(exit::CONFIG);
        }
    };
    if token.is_empty() {
        eprintln!("recall connect: no token entered — nothing was saved");
        return Ok(exit::CONFIG);
    }

    let client = Client::new(&url, &token)?;
    if let Err(e) = client.health().await {
        eprintln!("recall connect: could not reach {url}: {e}");
        eprintln!("  Nothing was saved.");
        return Ok(exit::CONFIG);
    }
    match client.check_token().await {
        Ok(()) => {}
        Err(client::Error::Status {
            code: 401 | 403, ..
        }) => {
            eprintln!("recall connect: {url} is up, and it rejected that token.");
            eprintln!("  Nothing was saved.");
            return Ok(exit::CONFIG);
        }
        Err(e) => {
            eprintln!("recall connect: {url} answered /health but not an authenticated call: {e}");
            eprintln!("  Nothing was saved.");
            return Ok(exit::CONFIG);
        }
    }

    saved.insert(&url, &token);
    if let Err(e) = credentials::save(&path, &saved) {
        eprintln!("recall connect: the token is valid, but saving it failed: {e}");
        return Ok(exit::CONFIG);
    }

    println!("Connected to {url}.");
    println!("Token saved to {} (readable by you only).", path.display());
    report_environment_overrides(&here, &url);
    Ok(exit::OK)
}

/// `recall disconnect [url]`.
///
/// Removes what `connect` saved. It cannot remove what the environment
/// supplies, and it says so rather than implying otherwise — a command
/// whose job is to make you safer must not manufacture a false sense of it.
pub fn disconnect(url: Option<&str>) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let Some(home) = credentials::home(here.env.lookup()) else {
        eprintln!("recall disconnect: no home directory, so nothing is saved — set RECALL_HOME");
        return Ok(exit::CONFIG);
    };
    let path = credentials::file(&home);

    let mut saved = match credentials::load(&path) {
        Ok(Some(saved)) => saved,
        Ok(None) => Credentials::default(),
        Err(e) => {
            eprintln!("recall disconnect: {e}");
            eprintln!("  Nothing was changed.");
            return Ok(exit::CONFIG);
        }
    };

    let target = match url {
        Some(u) => Some(credentials::normalize_url(u)),
        None => saved.default.clone().or_else(|| {
            // With exactly one server there is nothing to choose between.
            (saved.servers.len() == 1)
                .then(|| saved.servers.keys().next().cloned())
                .flatten()
        }),
    };

    match target {
        None if saved.servers.is_empty() => {
            println!("No token is saved in {}.", path.display());
        }
        None => {
            eprintln!("recall disconnect: more than one server is saved; name one:");
            for u in saved.servers.keys() {
                eprintln!("  recall disconnect {u}");
            }
            return Ok(exit::CONFIG);
        }
        Some(target) => {
            if saved.remove(&target) {
                let result = if saved.servers.is_empty() {
                    credentials::delete(&path)
                } else {
                    credentials::save(&path, &saved)
                };
                if let Err(e) = result {
                    eprintln!(
                        "recall disconnect: could not update {}: {e}",
                        path.display()
                    );
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

/// After `connect`: whatever in the environment will still win over the
/// file, named, because otherwise `connect` looks like it did nothing.
fn report_environment_overrides(here: &proj::Resolved, connected: &str) {
    let cfg = here.config();
    if cfg.url_source == Source::Environment && credentials::normalize_url(&cfg.url) != connected {
        println!();
        println!(
            "Note: RECALL_URL is set to {}, so this machine keeps talking to that server. \
             Unset it to use {connected}.",
            cfg.url
        );
    }
    if cfg.token_source == Source::Environment {
        println!();
        println!(
            "{} It wins over the saved one; remove it to finish the move.",
            environment_token_origin(here, "also supplies")
        );
    }
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
        None => format!(
            "Your shell {verb} RECALL_TOKEN — Recall can see the value but not which \
             file exported it, so check your shell profile."
        ),
    }
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
}
