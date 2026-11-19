//! Codex's realtime voice calls are gone from Aurea (dictation stays, on
//! the desktop). What remains is the hook the account and chat paths call
//! to end any call before they change what a chat runs with: a no-op now.

#[derive(Clone, Default)]
pub struct VoiceManager;

impl VoiceManager {
    /// End every voice call: there are none.
    pub fn retire(&self) {}

    /// End `chat`'s voice call: there is none.
    pub(crate) fn retire_chat(&self, _chat: &str) {}
}
