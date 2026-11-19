//! What remains of Claude Code's harness in Aurea: the refresh lock Claude's
//! CLI shares with this engine's account switching (Aurea runs opencode, but
//! a Claude login kept on the machine still guards its token refreshes).

pub mod refresh_lock;
