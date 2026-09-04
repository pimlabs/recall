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

use recall_hooks::{exit, is_memory_file, payload};

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
    if !is_memory_file(&project::memory_dir(&project::root()), &triggered) {
        return Ok(exit::OK);
    }

    let ctx = project::hook_context()?;
    match recall_hooks::push(&ctx, &triggered).await {
        Ok(res) => {
            if res.pushed.is_some() || !res.deleted.is_empty() {
                eprintln!(
                    "recall-push: pushed {}, deleted {} for {}",
                    usize::from(res.pushed.is_some()),
                    res.deleted.len(),
                    ctx.project_key
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
    let ctx = match project::hook_context() {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("recall-pull: {err}, leaving local memory untouched");
            return Ok(exit::OK);
        }
    };
    match recall_hooks::pull(&ctx).await {
        Ok(res) => {
            eprintln!("{}", res.describe(&ctx.project_key));
            Ok(exit::OK)
        }
        Err(err) => {
            eprintln!("recall-pull: fetch failed ({err}), leaving local memory untouched");
            Ok(exit::OK)
        }
    }
}
