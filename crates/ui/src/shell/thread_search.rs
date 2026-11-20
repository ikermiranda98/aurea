//! Scored thread search for the command palette, modeled on the mobile
//! workspace view's `score_row` (`zeron_client`): every query word must match
//! some field; a match scores its field's weight, plus half again when it
//! starts a word. Archived threads score half, and ties go to the most
//! recently active thread.

/// Lowercased whitespace-separated query words.
pub(super) fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}
