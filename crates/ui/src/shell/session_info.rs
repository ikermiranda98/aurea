//! The session card: where this chat runs (device, project, branch) and the
//! project's run actions, with the ports card (the servers its agents and
//! terminals started) and the lanes card (its side chats and subagents, see
//! `session_lanes`) stacked under it, all behind one toggle.
//! While the column has room, the card shows at its right on its own (until
//! the user hides it), the transcript and composer shifting left as needed
//! to clear it. Once clearing it would narrow the transcript, the card hides,
//! and opening it floats it over the transcript instead.
use super::*;


/// The card's width, its inset from the column's right edge, and the
/// least gap the reading column keeps from it.
pub(super) const SESSION_CARD_WIDTH: f32 = 272.0;
const SESSION_CARD_INSET: f32 = 12.0;
const SESSION_CARD_GAP: f32 = 24.0;
/// The transcript's own side gutters (its rows pad 48px each side).
const TRANSCRIPT_GUTTER: f32 = 48.0;

/// The room the transcript holds free on its right for the card docked in a
/// main column `main` wide, for a configured reading width `transcript`: none
/// while the centered column already clears the card, else just enough to
/// shift it left. `None` once that would narrow the column: the card no
/// longer fits beside it.
pub(super) fn session_card_reserve(main: f32, transcript: f32) -> Option<f32> {
    let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
    // Centered in `main - reserve`, the column's right edge is
    // (main - reserve) / 2 + transcript / 2; keep it at `card_left - gap`.
    let reserve = (main + transcript - 2.0 * (card_left - SESSION_CARD_GAP)).max(0.0);
    (main - reserve - 2.0 * TRANSCRIPT_GUTTER >= transcript - 0.5).then_some(reserve)
}

/// The branch's diff over its base, for the branch row's `+N −M`: the
/// Changes pane's *Branch changes* scope, kept without the pane. The
/// `WatchCheckoutDiffs` stream only says when to re-capture: its checksum
/// moves with the working tree and HEAD.
#[derive(Default)]
pub(super) struct SessionDiffs {
    diffs: Vec<zeron_proto::CheckoutDiff>,
    /// What the running watch targets: `None` the connected engine, else a
    /// remote chat on its host (diffs are produced where the checkout lives).
    target: Option<Option<crate::changes::RemoteDiffWatch>>,
    _watch: Option<Task<()>>,
    /// The comparison base per `device:cwd`, from `ListBranches`.
    base: Option<(String, String)>,
    /// `chat|cwd` the totals belong to, and those totals.
    branch: Option<(String, (u32, u32))>,
    /// `chat|cwd|checksum` last fetched (or in flight).
    fetched: Option<String>,
    _fetch: Option<Task<()>>,
}

impl SessionDiffs {
    /// The working-tree diff's checksum for `chat`: a branch capture is
    /// stale once it moves.
    fn checksum(&self, chat: &zeron_proto::Chat) -> Option<String> {
        crate::changes::resolve_diff(&self.diffs, chat).map(|diff| diff.checksum.clone())
    }

    /// The branch's line totals over its base; `None` while clean or unknown.
    fn totals(&self, chat: &zeron_proto::Chat) -> Option<(u32, u32)> {
        let context = format!("{}|{}", chat.id, chat.cwd.as_deref()?);
        self.branch
            .as_ref()
            .filter(|(key, _)| *key == context)
            .map(|&(_, totals)| totals)
            .filter(|&(adds, dels)| adds + dels > 0)
    }
}

/// The selected chat's running servers, for the ports card: the same
/// `WatchPreviews` stream the browser's start page lists, kept without it.
#[derive(Default)]
pub(super) struct SessionPreviews {
    snapshot: zeron_proto::PreviewSnapshot,
    /// The chat the running watch follows.
    chat: Option<String>,
    _watch: Option<Task<()>>,
    /// Ports paged in past the first [`PORTS_INITIAL`]; per chat.
    shown: usize,
    scroll: gpui::ScrollHandle,
    /// The card's floating scrollbar, the lanes card's treatment.
    bar: popover::MenuScrollbarState,
}

impl Shell {
    /// The card's reserve at the main column's LIVE width (see
    /// [`session_card_reserve`]): while the sidebar or a pane animates, the
    /// settled width would move or hide the card ahead of the column.
    fn session_info_reserve(&self, cx: &App) -> Option<f32> {
        let main = (self.viewport_width
            - self.sidebar_now()
            - self.right_visible_width(cx)
            - self.files_visible_width(cx))
        .max(0.0);
        session_card_reserve(main, crate::settings::transcript_width(cx))
    }

    /// Whether the card fits beside the transcript without narrowing it.
    pub(super) fn session_info_fits(&self, cx: &App) -> bool {
        self.session_info_reserve(cx).is_some()
    }

    /// Show the card: beside the transcript when it fits there, else over it.
    pub(super) fn show_session_info(&mut self, cx: &mut Context<Self>) {
        if !self.session_info_fits(cx) {
            self.session_info_overlay = true;
        } else if self.settings.session_info_hidden {
            self.settings.session_info_hidden = false;
            self.schedule_save(cx);
        }
        cx.notify();
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_column_keeps_the_transcript_centered_beside_the_card() {
        assert_eq!(session_card_reserve(1600.0, 736.0), Some(0.0));
        // The centered column's right edge clears the card by the gap.
        let main = 1600.0;
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        assert!(main / 2.0 + 736.0 / 2.0 <= card_left - SESSION_CARD_GAP);
    }

    #[test]
    fn a_tighter_column_shifts_the_transcript_just_enough() {
        let main = 1200.0;
        let reserve = session_card_reserve(main, 736.0).expect("it still fits");
        assert!(reserve > 0.0);
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        let right = (main - reserve) / 2.0 + 736.0 / 2.0;
        assert!((right - (card_left - SESSION_CARD_GAP)).abs() < 0.01);
        // At full width: the column keeps its configured width.
        assert!(main - reserve - 2.0 * TRANSCRIPT_GUTTER >= 736.0);
    }

    #[test]
    fn the_card_stops_fitting_where_the_transcript_would_narrow() {
        // Gutter, column, gap, card, inset.
        let least =
            TRANSCRIPT_GUTTER + 736.0 + SESSION_CARD_GAP + SESSION_CARD_WIDTH + SESSION_CARD_INSET;
        assert!(session_card_reserve(least, 736.0).is_some());
        assert!(session_card_reserve(least - 1.0, 736.0).is_none());
        assert!(session_card_reserve(700.0, 736.0).is_none());
        // A narrower reading column leaves room sooner.
        assert!(session_card_reserve(least - 1.0, 560.0).is_some());
    }
}
