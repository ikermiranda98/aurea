//! Scored thread search for the command palette, modeled on the mobile
//! workspace view's `score_row` (`zeron_client`): every query word must match
//! some field; a match scores its field's weight, plus half again when it
//! starts a word. Archived threads score half, and ties go to the most
//! recently active thread.

/// Lowercased whitespace-separated query words.
pub(super) fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Best first: higher score, then more recent activity, then the caller's
/// order. Keeps at most `limit`.
pub(super) fn rank<T>(mut scored: Vec<(T, u32, i64)>, limit: usize) -> Vec<T> {
    scored.sort_by(|(_, score_a, active_a), (_, score_b, active_b)| {
        score_b.cmp(score_a).then(active_b.cmp(active_a))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(item, _, _)| item).collect()
}
