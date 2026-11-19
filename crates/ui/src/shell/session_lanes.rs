//! The lanes card, stacked under the session card and shown with it: what
//! runs alongside the open chat — its **side chats** (forks, and chats an
//! agent spawned through the Zeron MCP server) and its **subagents** (the
//! spawn chips of its transcript, with their live status). Each lane is a
//! section of session-card rows; clicking a row opens it in the right pane.
//! Settled subagents fold into one "finished" row, closed until opened.

use zeron_proto::{Chat, ChatIndicator};

use super::*;

/// Rows a lane shows before "Show more" pages it, and the page size.
const INITIAL_ROWS: usize = 8;
const PAGE_ROWS: usize = 10;

/// What the lanes card has unfolded or paged, and where it has scrolled; it
/// follows the open chat.
#[derive(Default)]
pub(super) struct LaneState {
    chat_id: String,
    finished_open: bool,
    shown_chats: usize,
    shown_finished: usize,
    scroll: gpui::ScrollHandle,
    /// The card's floating scrollbar, the file tree's treatment.
    bar: popover::MenuScrollbarState,
}

impl LaneState {
    /// Folds and pages are per chat: another chat starts from rest.
    fn follow(&mut self, chat_id: &str) {
        if self.chat_id != chat_id {
            *self = Self {
                chat_id: chat_id.to_owned(),
                ..Self::default()
            };
        }
    }

    fn shown(count: usize) -> usize {
        count.max(INITIAL_ROWS)
    }

    /// Page the side-chat lane until its `index`th branch shows.
    pub(super) fn reveal_chat(&mut self, chat_id: &str, index: usize) {
        self.follow(chat_id);
        if index >= Self::shown(self.shown_chats) {
            self.shown_chats =
                INITIAL_ROWS + (index + 1 - INITIAL_ROWS).div_ceil(PAGE_ROWS) * PAGE_ROWS;
        }
    }
}

/// A side chat of the open chat, as its lane lists it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChildChatRow {
    pub chat_id: String,
    pub device_id: String,
    pub title: SharedString,
    pub status: ChatIndicator,
    pub time_ago: SharedString,
    /// The chat's linked pull request, drawn as the sidebar's badge.
    pub change_request: Option<zeron_proto::ChangeRequestSummary>,
    /// The harness it runs on, drawn as the sidebar's brand mark.
    pub harness: Option<zeron_proto::HarnessId>,
    activity: DateTime<Utc>,
}

/// The live (unarchived) children of `chat_id`, most recent activity first —
/// the same order the sidebar's Sessions list keeps.
pub(super) fn child_chat_rows(
    state: &AppState,
    chat_id: &str,
    now: DateTime<Utc>,
) -> Vec<ChildChatRow> {
    let mut rows: Vec<ChildChatRow> = state
        .chats
        .iter()
        .filter(|chat| !chat.archived && chat.parent_chat_id.as_deref() == Some(chat_id))
        .map(|chat| {
            let activity = chat.last_message_at.unwrap_or(chat.created_at);
            ChildChatRow {
                chat_id: chat.id.clone(),
                device_id: chat.device_id.clone(),
                title: child_chat_title(chat).into(),
                status: state.display_status_for(chat, now),
                time_ago: zeron_proto::view::format_time_ago(activity, now).into(),
                change_request: state.change_request_for_chat(chat).cloned(),
                harness: chat.config.as_ref().map(|config| config.harness),
                activity,
            }
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.activity));
    rows
}

/// A side chat titles itself on its first turn; until then the preview or a
/// placeholder stands in.
pub(super) fn child_chat_title(chat: &Chat) -> String {
    chat.title
        .clone()
        .or_else(|| chat.last_message_preview.clone())
        .unwrap_or_else(|| "New side chat".into())
}

fn glyph(path: &'static str, theme: &Theme) -> AnyElement {
    icon(path)
        .size(px(15.0))
        .text_color(theme.text_muted)
        .into_any_element()
}

/// A row's trailing time, in the muted subline size.
fn time_label(text: SharedString, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(theme.text_muted)
        .child(text)
}

/// A row's title, dissolving at its right edge like the sidebar's.
fn row_title(id: String, title: SharedString) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .child(sidebar_faded_label(
            id.into(),
            true,
            div().line_height(px(17.0)).child(title),
        ))
        .into_any_element()
}

impl Shell {
    /// Show the card with `chat_id`'s side-chat row paged in, so an inline
    /// rename has its row on screen; false when it is not one of the open
    /// chat's live side chats.
    pub(super) fn reveal_session_lane_chat(
        &mut self,
        chat_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let parent = self.active_chat.clone();
        let rows = child_chat_rows(self.state.read(cx), &parent, Utc::now());
        let Some(index) = rows.iter().position(|row| row.chat_id == chat_id) else {
            return false;
        };
        self.session_lanes.reveal_chat(&parent, index);
        // Takeover hides the transcript column the card lives in; a rename
        // field there would take keystrokes unseen.
        if self.right_pane_expanded {
            self.toggle_right_pane_expand(cx);
        }
        self.show_session_info(cx);
        true
    }

}
