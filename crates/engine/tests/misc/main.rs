//! Small suites in one test binary: each tests/*.rs file is its own binary,
//! linked against the whole crate, so a handful of tests each cost more to
//! link than to run.

mod osc7501;
// The fake Claude and Codex CLIs are POSIX shell scripts (`fake-*.sh`, as in the
// harness crate's own suites); Windows cannot spawn them.
#[cfg(unix)]
mod codex_subagents;
#[cfg(unix)]
mod claude_subagents;
mod local_profiles;
mod self_presence;
