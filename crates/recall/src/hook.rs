//! `recall push` and `recall pull` — the two commands Claude Code runs, not
//! the user.
//!
//! They are together because they share one rule: **a hook must not be the
//! reason a session breaks.** Everything here that looks over-cautious is
//! that rule applied. `push` runs on every Edit and Write, so it decides
//! whether it has anything to do before it consults configuration at all;
//! `pull` runs at session start, so an unreachable server is a warning on
//! stderr and exit 0, never a failure.

use std::io;
use std::path::PathBuf;

use recall_hooks::{exit, foreign_memory_slug, is_memory_file, payload};

use crate::project;

/// Handles one `PostToolUse` invocation.
pub async fn push() -> anyhow::Result<i32> {
    let hook = payload::parse_post_tool_use(io::stdin().lock());
    if hook.tool_input.file_path.is_empty() {
        return Ok(exit::OK);
    }
    let triggered = PathBuf::from(&hook.tool_input.file_path);

    // Decide whether this is even our business before asking for
    // configuration. A machine that has cloned a wired project without being
    // configured yet — the exact case Recall exists for — would otherwise
    // report a missing token on every unrelated file the user touches.
    let here = project::resolve();
    if !is_memory_file(&here.memory_dir(), &triggered) {
        // Silence is right for an unrelated file and wrong for this one:
        // memory that belongs to another project looks exactly like a
        // successful push from here, and the next pull overwrites it.
        if let Some(slug) = foreign_memory_slug(&here.memory_root(), &here.memory_dir(), &triggered)
        {
            eprintln!(
                "recall-push: that file is memory for {slug}, not for this project, \
                 so nothing was pushed"
            );
            eprintln!(
                "recall-push:   standing in {}; a git worktree has its own memory directory",
                here.root.display()
            );
        }
        return Ok(exit::OK);
    }

    let cfg = here.config();
    here.protect_device_key(&cfg, "recall-push");
    let cfg = here.enroll_if_needed(cfg, "recall-push").await;
    let ctx = here.hook_context_for(&cfg)?;
    let result = match recall_hooks::push(&ctx, &triggered).await {
        // Once, and only for the refusals that name the device.
        Err(err) => match err.server_error().filter(|e| e.device_gone()) {
            Some(refusal) => match here.after_refusal(&cfg, "recall-push", refusal).await {
                Some(cfg) => {
                    let ctx = here.hook_context_for(&cfg)?;
                    recall_hooks::push(&ctx, &triggered).await
                }
                None => Err(err),
            },
            None => Err(err),
        },
        ok => ok,
    };
    match result {
        Ok(res) => {
            if res.pushed.is_some() || !res.deleted.is_empty() {
                eprintln!(
                    "recall-push: pushed {}, deleted {} for {}",
                    usize::from(res.pushed.is_some()),
                    res.deleted.len(),
                    ctx.project_key()
                );
            }
            Ok(exit::OK)
        }
        Err(err) => {
            eprintln!("recall-push: {err}");
            Ok(exit::SERVER)
        }
    }
}

/// Handles one `SessionStart` invocation.
pub async fn pull() -> anyhow::Result<i32> {
    // An unconfigured or unreachable server warns on stderr and exits 0,
    // leaving whatever is already on disk alone.
    let here = project::resolve();
    // A cloud session with RECALL_AUTHKEY and no device key yet becomes
    // a device here, before its first request, with nobody asked anything.
    let cfg = here.config();
    here.protect_device_key(&cfg, "recall-pull");
    let cfg = here.enroll_if_needed(cfg, "recall-pull").await;
    if cfg.device_error.is_none()
        && cfg.device.is_none()
        && cfg.token.is_empty()
        && cfg.authkey.is_some()
    {
        eprintln!("recall-pull: no device key and no RECALL_TOKEN, leaving local memory untouched");
        return Ok(exit::OK);
    }
    let ctx = match here.hook_context_for(&cfg) {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("recall-pull: {err}, leaving local memory untouched");
            return Ok(exit::OK);
        }
    };
    let result = match recall_hooks::pull(&ctx).await {
        Err(err) => match err.server_error().filter(|e| e.device_gone()) {
            Some(refusal) => match here
                .after_refusal(&cfg, "recall-pull", refusal)
                .await
                .and_then(|cfg| here.hook_context_for(&cfg).ok())
            {
                Some(ctx) => recall_hooks::pull(&ctx).await,
                None => Err(err),
            },
            None => Err(err),
        },
        ok => ok,
    };
    match &result {
        Ok(res) => eprintln!("{}", res.describe(ctx.project_key())),
        Err(err) => {
            eprintln!("recall-pull: fetch failed ({err}), leaving local memory untouched")
        }
    }
    warn_if_rewritten(&cfg);
    Ok(exit::OK)
}

/// Says so at every session start while this machine holds a finding that
/// the server's audit log no longer extends a checkpoint it saved: loud,
/// and still exit 0, since a hook must not be what stops a session. Only
/// reads the file; the pull itself saved its checkpoint through the
/// client, and checking is `recall doctor`'s (see `recall_hooks::audit`).
fn warn_if_rewritten(cfg: &recall_hooks::ClientConfig) {
    let Some(file) = &cfg.audit_file else {
        return;
    };
    let witness = recall_hooks::audit::Witness::new(file, &cfg.url);
    if let Ok(recall_hooks::audit::Saved {
        inconsistent: Some(found),
        ..
    }) = witness.load()
    {
        eprintln!(
            "recall-pull: WARNING: the server's audit log no longer extends a checkpoint this \
             machine saved ({}), found {}. Run recall doctor.",
            found.detail, found.found_at
        );
    }
}
