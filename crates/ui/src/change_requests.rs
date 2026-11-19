//! What remains of the pull-request badges in Aurea: the branch a chat's own
//! source context names (projects and pull requests are gone; the branch
//! still labels a chat that carries one).

use zeron_proto::{Chat, Space};

/// Branch metadata safe to render for this conversation. Pre-source-context
/// rows are intentionally rejected: their scalar branch may describe an old
/// state of either a shared checkout or a worktree.
pub(crate) fn conversation_branch<'a>(chat: &'a Chat, _spaces: &'a [Space]) -> Option<&'a str> {
    chat.source_context
        .as_ref()
        .map(|source| source.branch.as_str())
        .filter(|branch| !branch.trim().is_empty())
}
