//! `recall promote` — move one note out of this project, into the global or
//! the machine scope.
//!
//! The only command a user runs *at* their memory rather than about it, and
//! the only way into either scope that doesn't involve `mkdir`. It is loud
//! where the hooks are quiet: nobody typed it by accident, so a refusal is
//! worth an error and a non-zero exit rather than a warning nobody reads.

use std::path::{Path, PathBuf};

use recall_hooks::{exit, PromoteError, PromoteTarget};

use crate::project;

/// The destinations `--to` accepts.
///
/// A separate enum from `recall_hooks::PromoteTarget` only because that crate
/// does not depend on clap and should not start: this is the argument
/// parser's vocabulary, and the `From` below is the one place the two meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    /// Follows you into every project you sync.
    Global,
    /// Stays with this machine.
    Machine,
}

impl From<Target> for PromoteTarget {
    fn from(t: Target) -> Self {
        match t {
            Target::Global => PromoteTarget::Global,
            Target::Machine => PromoteTarget::Machine,
        }
    }
}

/// Promotes `file`, then says where it went and who will see it.
pub async fn run(file: &Path, to: Target) -> anyhow::Result<i32> {
    let ctx = project::resolve().hook_context()?;
    let path = resolve(&ctx.memory_dir, file);

    match recall_hooks::promote(&ctx, &path, to.into()).await {
        Ok(res) => {
            if res.resumed {
                println!(
                    "recall: finished an earlier promotion of {}, it was already in {}",
                    res.from, res.to
                );
            } else {
                println!("recall: promoted {} → {}", res.from, res.to);
            }
            // What happens next differs by scope, and getting it wrong in
            // either direction is the confusion this command exists to
            // prevent: a machine note is not going to appear in every
            // project, and saying so would be a promise Recall does not keep.
            match to {
                Target::Global => {
                    if let Some(global) = ctx.global() {
                        println!(
                            "
  It is stored under {} and linked from MEMORY.md. Every other project
  picks it up at its next session start, when the pull hook runs.",
                            global.key
                        );
                    }
                }
                Target::Machine => {
                    if let Some(machine) = ctx.machine() {
                        println!(
                            "
  It is stored under {} and linked from MEMORY.md. It follows this machine
  into every project here, and reaches no other machine.",
                            machine.key
                        );
                    }
                }
            }
            Ok(exit::OK)
        }
        Err(err) => {
            eprintln!("recall: {err}");
            // The split the error type already draws: a refusal decided
            // before anything moved is something the user changes and
            // retries, while a push or a write that failed mid-flight is the
            // server's or the disk's — and a script wants to tell those
            // apart without parsing prose.
            Ok(match err {
                PromoteError::Sync(_) => exit::SERVER,
                _ => exit::CONFIG,
            })
        }
    }
}

/// Where to look for the note named on the command line.
///
/// A relative path is relative to the **memory directory**, not to the
/// working directory: the notes live somewhere nobody `cd`s into, so
/// `recall promote topics/user.md` is what a user types after reading a
/// listing of that directory, and resolving it against the shell's cwd would
/// find nothing nearly every time. An absolute path — from `recall status`,
/// from tab completion, or from Claude Code naming the file it just wrote —
/// is taken as given.
fn resolve(memory_dir: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        return file.to_path_buf();
    }
    memory_dir.join(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_note_is_looked_for_in_the_memory_directory() {
        let dir = Path::new("/home/me/.claude/projects/-home-me-app/memory");
        assert_eq!(
            resolve(dir, Path::new("topics/user.md")),
            dir.join("topics/user.md")
        );
    }

    /// The path `recall status` prints, pasted back verbatim, has to work.
    #[test]
    fn an_absolute_note_is_taken_as_given() {
        let given = Path::new("/home/me/.claude/projects/-home-me-app/memory/user.md");
        assert_eq!(resolve(Path::new("/somewhere/else"), given), given);
    }

    /// Escaping is rejected by `recall_hooks`, lexically, whatever this
    /// produces — but joining must not quietly *hide* the attempt by
    /// producing something that looks contained.
    #[test]
    fn a_traversing_path_still_reads_as_traversing_after_the_join() {
        let dir = Path::new("/home/me/memory");
        assert_eq!(
            resolve(dir, Path::new("../../.ssh/id_rsa")),
            Path::new("/home/me/memory/../../.ssh/id_rsa")
        );
    }
}
