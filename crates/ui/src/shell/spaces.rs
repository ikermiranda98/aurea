//! Spaces sidebar: the space-filter dropdown (searchable, with "All projects"),
//! the filtered Sessions list, and the add-space palette (device
//! tabs + filtered folder browser).
//!
//! A space = a synced (device, folder) pair. Spaces stopped being a
//! navigation spine when tabs went device-local: the dropdown only FILTERS
//! the sidebar's session list (never the tab strip) and hosts space
//! management (add via the palette; rename/delete via row context menus).
//! Child module of `shell` so it renders straight off `Shell`'s private state.

use super::*;
use gpui::{FocusHandle, Window};
use std::collections::HashSet;
use zeron_proto::{
    Device, DriveEntry, FolderListing, HomeFolderMatch,
};

/// Remove only ids absent from the workspace. Archived sessions remain known
/// so unarchiving restores their local pin and position.
pub(super) fn retain_known_pins(
    pinned_ids: &mut Vec<String>,
    known_chat_ids: &HashSet<String>,
) -> bool {
    let before = pinned_ids.len();
    let mut seen = HashSet::new();
    pinned_ids.retain(|id| known_chat_ids.contains(id) && seen.insert(id.clone()));
    pinned_ids.len() != before
}

pub(super) fn pinned_drag_snapshot_is_valid(
    dragged_id: &str,
    snapshot_ids: &[String],
    current_ids: &[String],
) -> bool {
    snapshot_ids.iter().any(|id| id == dragged_id) && snapshot_ids == current_ids
}

pub(super) fn compare_sidebar_chats(
    sort: SidebarSort,
    left: &zeron_proto::Chat,
    right: &zeron_proto::Chat,
) -> std::cmp::Ordering {
    let primary = match sort {
        SidebarSort::Created => right.created_at.cmp(&left.created_at),
        SidebarSort::LastUpdated => right
            .last_message_at
            .unwrap_or(right.created_at)
            .cmp(&left.last_message_at.unwrap_or(left.created_at)),
    };
    primary.then_with(|| left.id.cmp(&right.id))
}

/// The space-filter dropdown, `Some` while open. The same searchable-menu
/// recipe as the composer's ref picker: filter input on top
/// (`PaletteSearch` context so ↑↓/⏎ bubble to the card), ranked substring
/// rows, keyboard highlight.
pub(super) struct SpacesMenu {
    search: Entity<ComposerInput>,
    /// Keyboard highlight — an index into [`Shell::spaces_menu_rows`], or
    /// that list's length when the pinned "New project" footer holds it.
    active: usize,
    /// Tracked on the card — puts it on the keyboard dispatch path while the
    /// search input holds focus (the structure every working picker uses).
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

pub(super) struct SidebarViewMenu {
    submenu: Option<usize>,
    submenu_active: Option<usize>,
    submenu_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    submenu_on_left: bool,
    hover_intent: popover::HoverIntent<usize>,
    /// Keyboard cursor. Mouse-opened menus start without one so the persisted
    /// radio/check state is the only selection signal until an arrow key is
    /// pressed.
    active: Option<usize>,
    focus: FocusHandle,
}

struct SidebarViewOptionsTooltip;

impl Render for SidebarViewOptionsTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .child("Sidebar view options")
    }
}

/// Shared quiet rule for sidebar groups and palette sections.
pub(super) fn sidebar_separator(theme: &Theme) -> gpui::Div {
    div().h(px(1.0)).bg(theme.border.opacity(0.6))
}

/// New project navigates devices, locations, then folders on a command-palette surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectStep {
    Devices,
    Locations,
    Folders,
    /// Cloud picked: the GitHub repositories Cloud reaches. A Cloud project
    /// is a repository, not a folder on a device.
    Repositories,
}

pub(super) struct AddSpaceFlow {
    step: ProjectStep,
    location: Option<(String, Option<String>)>,
    /// The selected device.
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder. Carries the
    /// tab-completion ghost (the faint suffix ⇥ accepts), and a trailing `/`
    /// on a folder-naming query descends immediately.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// The selected device's mounted drives/volumes.
    /// Best-effort: an error just leaves the section at Home only.
    drives: Loadable<Vec<DriveEntry>>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the Home crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the current step’s filtered rows.
    active: usize,
    submit_busy: bool,
    /// A clone into the open folder is running (`owner/name`).
    cloning: Option<String>,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    /// Horizontal breadcrumb strip; `crumb_key` is the path it last revealed.
    crumb_scroll: gpui::ScrollHandle,
    crumb_key: String,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    drives_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    /// Folders matching the query anywhere under the device's home (Home
    /// location only).
    home_search: HomeFolderSearch,
    /// The crumb row as a typed path; `None` shows the crumbs.
    path_edit: Option<PathEdit>,
    _search_events: Subscription,
}

impl AddSpaceFlow {
}

/// The crumb trail as an editable path, address-bar style: ⌘L, the pen at
/// the row's end, or a click beside the crumbs. The open folder stays
/// listed until the typed path answers, so a typo never loses it.
struct PathEdit {
    input: Entity<ComposerInput>,
    /// ListFolders for the typed path is in flight.
    opening: bool,
    /// ⌘↵ asked for the open in flight to add the project too; a later ↵
    /// doesn't take that back.
    adding: bool,
    /// Why the typed path didn't open; the next edit clears it.
    error: Option<SharedString>,
    /// ⇥'s source: the folders of the typed path's parent, keyed by that
    /// parent's `folder_path_key`.
    siblings: Option<(String, Loadable<Vec<zeron_proto::FolderEntry>>)>,
    focus_pending: bool,
    open_task: Option<Task<()>>,
    siblings_task: Option<Task<()>>,
    /// Resolves the device's home for `~` when nothing has listed it yet.
    _home_task: Option<Task<()>>,
    _events: Subscription,
}

/// The new-project picker's search over the device's home, answered by the
/// host's home search index (the one projectless chats use). Entering Home
/// starts the index; leaving Home or closing the picker releases it unless a
/// projectless chat warmed it.
#[derive(Default)]
struct HomeFolderSearch {
    /// Bumped per request; an answer to an older one is dropped.
    generation: u64,
    loading: bool,
    /// The host's index was still scanning at the last answer.
    indexing: bool,
    results: Vec<HomeFolderMatch>,
    /// A request reached the host, so it may hold an index to release.
    started: bool,
    /// The host predates `SearchHomeFolders`; the section stays hidden.
    unsupported: bool,
    task: Option<Task<()>>,
    /// The request that started the host's index.
    warm_task: Option<Task<()>>,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

// Handle-based rail host for the spaces dropdown: its list is a plain
// tracked scroller, so the trait's default metrics/press/drag (off the live
// ScrollHandle) apply unchanged.
impl popover::ScrollRailHost for Shell {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        &mut self.spaces_menu_bar
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.spaces_menu.get().map(|menu| menu.list_scroll.clone())
    }
}

impl Shell {
    // ---- space filter ----

    /// Close the space-filter dropdown through the exit animation (no-op when
    /// it isn't open). Every close path funnels here so the menu always
    /// animates out instead of vanishing.
    pub(super) fn close_spaces_menu(&mut self, cx: &mut Context<Self>) {
        if self.spaces_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.spaces_menu);
            cx.notify();
        }
    }

    // ---- sidebar sections ----

    pub(super) fn sidebar_session_transfer_is_valid(
        &self,
        payload: &SidebarSessionDrag,
        cx: &App,
    ) -> bool {
        if payload.filter != self.settings.space_filter
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&payload.profile_key)
        {
            return false;
        }
        let state = self.state.read(cx);
        let visible: HashSet<String> = state
            .sidebar_chats(Utc::now(), payload.filter.as_deref())
            .into_iter()
            .map(|(_, chat)| chat.id.clone())
            .collect();
        if !visible.contains(&payload.chat_id) {
            return false;
        }
        let pins = self.sidebar_pins_for_profile(&payload.profile_key, cx);
        let current: Vec<_> = pins.into_iter().filter(|id| visible.contains(id)).collect();
        current == *payload.visible_ids
    }

    pub(super) fn cancel_sidebar_session_transfer(&mut self, cx: &mut Context<Self>) {
        self.chat_status_hover = None;
        self.cancel_pinned_session_drag(cx);
        if let Some(mut transfer) = self.sidebar_session_transfer.take() {
            transfer.preview = None;
            if !self.reduced_motion && self.sidebar_session_transfer_is_valid(&transfer.payload, cx)
            {
                self.sidebar_session_return = Some(SidebarSessionReturn {
                    transfer,
                    epoch: self.pinned_session_drag_generation,
                    started: std::time::Instant::now(),
                });
                let epoch = self.pinned_session_drag_generation;
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(
                                SIDEBAR_DRAG_SCROLL_FRAME_MS,
                            ))
                            .await;
                        let keep_running = this
                            .update(cx, |shell, cx| {
                                let Some(returning) = shell.sidebar_session_return.as_ref() else {
                                    return false;
                                };
                                if returning.epoch != epoch {
                                    return false;
                                }
                                if returning.started.elapsed() >= TAB_SLIDE.total()
                                    && returning.transfer.slide.started.elapsed()
                                        >= TAB_SLIDE.total()
                                    && returning.transfer.source_collapse.started.elapsed()
                                        >= TAB_SLIDE.total()
                                    && returning
                                        .transfer
                                        .section_gaps
                                        .values()
                                        .all(|gap| gap.started.elapsed() >= TAB_SLIDE.total())
                                {
                                    shell.sidebar_session_return = None;
                                    cx.notify();
                                    return false;
                                }
                                cx.notify();
                                true
                            })
                            .unwrap_or(false);
                        if !keep_running {
                            break;
                        }
                    }
                })
                .detach();
                self.pinned_session_drag_generation =
                    self.pinned_session_drag_generation.wrapping_add(1);
            }
            cx.notify();
        }
    }

    pub(super) fn update_pinned_session_drag(
        &mut self,
        payload: &SidebarSessionDrag,
        over: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.pinned_open
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&payload.profile_key)
        {
            self.cancel_pinned_session_drag(cx);
            return;
        }
        let Some(from) = payload
            .visible_ids
            .iter()
            .position(|id| id == &payload.chat_id)
        else {
            self.cancel_pinned_session_drag(cx);
            return;
        };
        if over >= payload.visible_ids.len() {
            self.cancel_pinned_session_drag(cx);
            return;
        }
        match &mut self.pinned_session_drag {
            Some(drag)
                if drag.chat_id == payload.chat_id
                    && drag.filter == payload.filter
                    && drag.profile_key == payload.profile_key
                    && drag.visible_ids.as_ref() == payload.visible_ids.as_ref()
                    && drag.over != over =>
            {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch = drag.epoch.wrapping_add(1);
                cx.notify();
            }
            Some(drag)
                if drag.chat_id == payload.chat_id
                    && drag.filter == payload.filter
                    && drag.profile_key == payload.profile_key
                    && drag.visible_ids.as_ref() == payload.visible_ids.as_ref() => {}
            _ => {
                self.pinned_session_drag_generation =
                    self.pinned_session_drag_generation.wrapping_add(1);
                self.pinned_session_drag = Some(PinnedSessionDragState {
                    chat_id: payload.chat_id.clone(),
                    visible_ids: payload.visible_ids.clone(),
                    from,
                    over,
                    prev_over: from,
                    epoch: 0,
                    filter: payload.filter.clone(),
                    profile_key: payload.profile_key.clone(),
                    pointer_y: None,
                    viewport_top: 0.0,
                    viewport_bottom: 0.0,
                    generation: self.pinned_session_drag_generation,
                    autoscroll_active: false,
                });
                cx.notify();
            }
        }
    }

    pub(super) fn pinned_session_drag_is_valid(&self, cx: &App) -> bool {
        let Some(drag) = self.pinned_session_drag.as_ref() else {
            return true;
        };
        if !self.pinned_open
            || drag.filter != self.settings.space_filter
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&drag.profile_key)
        {
            return false;
        }
        let current_pins = self.sidebar_pins_for_profile(&drag.profile_key, cx);
        let state = self.state.read(cx);
        let in_filter = state.project_filter(drag.filter.as_deref());
        let visible_ids: HashSet<String> = state
            .overview_chats(Utc::now())
            .into_iter()
            .filter(|(_, chat)| in_filter(chat))
            .map(|(_, chat)| chat.id.clone())
            .collect();
        let current_ids = current_pins
            .iter()
            .filter(|id| visible_ids.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        pinned_drag_snapshot_is_valid(&drag.chat_id, drag.visible_ids.as_ref(), &current_ids)
    }

    pub(super) fn cancel_pinned_session_drag(&mut self, cx: &mut Context<Self>) {
        if self.pinned_session_drag.take().is_some() {
            self.pinned_session_drag_generation =
                self.pinned_session_drag_generation.wrapping_add(1);
            cx.notify();
        }
    }

    pub(super) fn close_sidebar_view_menu(&mut self, cx: &mut Context<Self>) {
        self.clean_chat_armed = false;
        if let Some(menu) = self.sidebar_view_menu.open_mut() {
            menu.hover_intent.reset();
        }
        if self.sidebar_view_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.sidebar_view_menu);
            cx.notify();
        }
    }

    pub(super) fn open_sidebar_view_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_spaces_menu(cx);
        let focus = cx.focus_handle();
        self.sidebar_view_menu.open(SidebarViewMenu {
            submenu: None,
            submenu_active: None,
            submenu_bounds: None,
            submenu_on_left: false,
            hover_intent: popover::HoverIntent::default(),
            active: None,
            focus: focus.clone(),
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn sidebar_view_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(menu) = self.sidebar_view_menu.open_mut() else {
            return;
        };
        menu.hover_intent.cancel();
        let key = match popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        ) {
            popover::MenuKey::Up => "up",
            popover::MenuKey::Down => "down",
            _ => event.keystroke.key.as_str(),
        };
        match key {
            "escape" => self.close_sidebar_view_menu(cx),
            "up" | "down" => {
                let delta = if key == "up" { -1 } else { 1 };
                menu.active = popover::menu_step(menu.active, 2, delta);
                cx.notify();
            }
            "enter" | "space" if !event.is_held => match menu.active.unwrap_or(0) {
                0 => self.activate_clean_chat(cx),
                _ => {
                    self.close_sidebar_view_menu(cx);
                    let section = self.settings.settings_section.reopenable();
                    self.open_settings(section, cx);
                }
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The drawer's "..." menu in Aurea: Clean Chat (asks once more before
    /// it wipes the history) and Settings.
    fn render_sidebar_view_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let Some(menu_state) = self.sidebar_view_menu.get() else {
            return div().into_any_element();
        };
        let active = menu_state.active;
        let focus = menu_state.focus.clone();
        let armed = self.clean_chat_armed;
        // Plain menu rows at their natural height (user request: smaller,
        // more natural boxes).
        let row = |ix: usize, id: &'static str| {
            popover::menu_row_nav(theme, false, active == Some(ix), id).id(id)
        };
        let clean_label = if armed { "Click again to confirm" } else { "Clean Chat" };
        let clean = row(0, "sidebar-clean-chat")
            // The row already owns `on_hover` (its hover blend): follow the
            // pointer with mouse moves instead.
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(menu) = this.sidebar_view_menu.open_mut()
                    && menu.active != Some(0)
                {
                    menu.active = Some(0);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.activate_clean_chat(cx);
                cx.stop_propagation();
            }))
            .child(
                icon(icons::TRASH_BIN_MINIMALISTIC)
                    .size(px(14.0))
                    .text_color(if armed { theme.danger } else { theme.text_muted }),
            )
            .child(
                div()
                    .flex_1()
                    .when(armed, |el| el.text_color(theme.danger))
                    .child(clean_label),
            );
        let open_settings = row(1, "sidebar-open-settings")
            // The row already owns `on_hover` (its hover blend): follow the
            // pointer with mouse moves instead.
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(menu) = this.sidebar_view_menu.open_mut()
                    && menu.active != Some(1)
                {
                    menu.active = Some(1);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_sidebar_view_menu(cx);
                let section = this.settings.settings_section.reopenable();
                this.open_settings(section, cx);
                cx.stop_propagation();
            }))
            .child(
                icon(icons::SETTINGS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(div().flex_1().child("Settings"));
        // Sized to its two rows, not to the drawer.
        popover::popover_card(theme)
            .w(px(184.0))
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.sidebar_view_menu_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.close_sidebar_view_menu(cx);
            }))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(clean)
            .child(open_settings)
            .into_any_element()
    }

    /// Clean Chat: the first activation arms it, the second deletes every
    /// chat (archived and side chats too) and leaves an empty one to start
    /// from zero.
    fn activate_clean_chat(&mut self, cx: &mut Context<Self>) {
        if !self.clean_chat_armed {
            self.clean_chat_armed = true;
            cx.notify();
            return;
        }
        self.clean_chat_armed = false;
        self.close_sidebar_view_menu(cx);
        let chats: Vec<String> = self
            .state
            .read(cx)
            .chats
            .iter()
            .map(|chat| chat.id.clone())
            .collect();
        for chat in chats {
            self.delete_chat(chat, cx);
        }
        self.state.update(cx, |s, cx| s.select_chat(None, cx));
        cx.notify();
    }

    /// The sidebar's view options ("…"): a titlebar control at the sidebar's
    /// trailing edge, opening the Organize / Sort / Show menu.
    pub(super) fn render_sidebar_view_trigger(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view_open = self.sidebar_view_menu.is_open();
        let view_focus = self.sidebar_view_trigger_focus.clone();
        let view_trigger = div()
            .id("sidebar-view-options")
            .role(gpui::Role::Button)
            .aria_label("Sidebar view options")
            .aria_expanded(view_open)
            .track_focus(&view_focus)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border.opacity(0.0))
            .focus_visible(|el| el.border_color(theme.border_strong))
            .cursor_pointer()
            .text_color(theme.text_muted)
            .bg(if view_open {
                theme.glass_hover()
            } else {
                theme.glass_hover().opacity(0.0)
            })
            .hover(|el| el.bg(theme.glass_hover()))
            // Over the titlebar's drag strip: block it, as the other titlebar
            // controls do, so a click never starts a window drag.
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.sidebar_view_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                if this.sidebar_view_menu.take_press_was_open() {
                    this.close_sidebar_view_menu(cx);
                } else {
                    this.open_sidebar_view_menu(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(
                    event.keystroke.key.to_ascii_lowercase().as_str(),
                    "enter" | "space" | "arrowdown"
                ) {
                    cx.stop_propagation();
                    if this.sidebar_view_menu.is_open()
                        && !event.keystroke.key.eq_ignore_ascii_case("arrowdown")
                    {
                        this.close_sidebar_view_menu(cx);
                    } else if !this.sidebar_view_menu.is_open() {
                        this.open_sidebar_view_menu(window, cx);
                    }
                }
            }))
            .tooltip(|_, cx| cx.new(|_| SidebarViewOptionsTooltip).into())
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child(
                icon(icons::MORE_HORIZONTAL)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );
        let view_trigger = if self.sidebar_view_menu.get().is_some() {
            let closing = self.sidebar_view_menu.closing_since();
            let menu = self.render_sidebar_view_menu(theme, cx);
            view_trigger.relative().child(popover::anchored_menu_right(
                "sidebar-view-options-menu",
                menu,
                closing,
            ))
        } else {
            view_trigger
        };
        view_trigger.into_any_element()
    }

    /// The chats in navigation order. Aurea has no sidebar list: the visible
    /// (non-archived, top-level) chats, most recently active first.
    pub(super) fn sidebar_visible_order(&self, cx: &Context<Self>) -> Vec<String> {
        let state = self.state.read(cx);
        let mut chats: Vec<zeron_proto::Chat> = state
            .sidebar_chats(Utc::now(), None)
            .into_iter()
            .map(|(_, chat)| chat.clone())
            .collect();
        chats.sort_by(|left, right| compare_sidebar_chats(SidebarSort::LastUpdated, left, right));
        chats.into_iter().map(|chat| chat.id).collect()
    }

    /// Aurea has no sidebar list to scroll a chat into: always shown.
    pub(super) fn reveal_sidebar_chat(&mut self, _chat_id: &str, _cx: &mut Context<Self>) -> bool {
        true
    }

    // ---- add-space flow ----

    // ---- typed path: the crumb row as an address bar ----

    // ---- space context menu / rename / delete overlays ----

}

/// Synthetic responses for the isolated native screenshot fixture only.
#[cfg(feature = "project-palette-fixture")]
impl Shell {
    pub fn fixture_project_responses(&mut self, cx: &mut Context<Self>) {
        if std::env::var_os("ZERON_FIXTURE_BACKGROUND").is_some() {
            self.composer
                .read(cx)
                .pickers()
                .clone()
                .update(cx, |pickers, cx| pickers.fixture_model_catalog(cx));
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.is_some() && !matches!(flow.drives, Loadable::Ready(_)) {
            flow.drives = Loadable::Ready(
                serde_json::from_value(serde_json::json!([
                    {"name":"Projects", "path":"/projects"},
                    {"name":"System", "path":"/"}
                ]))
                .unwrap(),
            );
            cx.notify();
        }
        if flow.step == ProjectStep::Folders && !matches!(flow.browser, Loadable::Ready(_)) {
            let path = flow
                .browser_path
                .clone()
                .unwrap_or_else(|| "/home/alex".into());
            if flow.browser_path.is_none() {
                flow.home = Some(path.clone());
            }
            let names = match path.as_str() {
                "/home/alex" => vec![
                    "Desktop",
                    "Documents",
                    "Downloads",
                    "Movies",
                    "Music",
                    "Pictures",
                    "Projects",
                    "Public",
                    "dotfiles",
                    "notes",
                    "sandbox",
                    "scratch",
                ],
                "/projects" | "/home/alex/Projects" => vec!["fieldnotes", "mobile-app", "website"],
                _ => vec!["assets", "docs", "src", "tests"],
            };
            flow.browser = Loadable::Ready(
                serde_json::from_value(serde_json::json!({
                    "path":path,
                    "entries":names.into_iter().map(|name| serde_json::json!({
                        "name":name,"isDir":true,"isRepo":name=="fieldnotes"
                    })).collect::<Vec<_>>()
                }))
                .unwrap(),
            );
            cx.notify();
        }
    }
}

/// The typed-path capture's typing target and read-outs
/// (`examples/new-project-path-fixture.rs`).
#[cfg(feature = "project-path-fixture")]
impl Shell {
    /// The New project input holding the keyboard — the path field or the
    /// search — to type into through its input handler, as the platform does.
    pub fn fixture_project_input(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<ComposerInput>> {
        let flow = self.add_space.as_ref()?;
        flow.path_edit
            .as_ref()
            .map(|edit| edit.input.clone())
            .into_iter()
            .chain([flow.search.clone()])
            .find(|input| input.focus_handle(cx).is_focused(window))
    }

    /// Where the New project palette stands; `None` while it is closed.
    pub fn fixture_project_summary(&self, cx: &App) -> Option<String> {
        let flow = self.add_space.as_ref()?;
        let edit = flow.path_edit.as_ref();
        Some(format!(
            "step={:?} open={:?} field={:?} error={:?}",
            flow.step,
            flow.browser.ready().map(|listing| listing.path.as_str()),
            edit.map(|edit| edit.input.read(cx).text().to_owned()),
            edit.and_then(|edit| edit.error.as_deref()),
        ))
    }
}

/// The drawer's "..." menu: Clean Chat and Settings.
#[cfg(test)]
mod drawer_menu_tests {
    use super::*;

    fn shell(cx: &mut gpui::TestAppContext, path: &std::path::Path) -> gpui::WindowHandle<Shell> {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: path.into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        })
    }

    /// The menu builds (it panicked when a row took two hover listeners) and
    /// Clean Chat asks before it deletes.
    #[gpui::test]
    fn the_drawer_menu_builds_and_clean_chat_asks_first(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let window = shell(cx, dir.path());
        window
            .update(cx, |shell, window, cx| {
                shell.open_sidebar_view_menu(window, cx);
                let theme = Theme::of(cx).clone();
                let _menu = shell.render_sidebar_view_menu(&theme, cx);
                assert!(!shell.clean_chat_armed);
                shell.activate_clean_chat(cx);
                assert!(shell.clean_chat_armed, "the first click only arms it");
                let _armed = shell.render_sidebar_view_menu(&theme, cx);
                shell.close_sidebar_view_menu(cx);
                assert!(!shell.clean_chat_armed, "closing the menu disarms it");
            })
            .unwrap();
    }
}
