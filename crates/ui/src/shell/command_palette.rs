//! Global search over actions and conversations, using the sidebar's
//! conversation rows. Results are grouped in sections; tabs narrow them to
//! one kind. (Aurea has no workspace files to search.)
use super::*;
use crate::appearance::AppearanceMode;

/// Key context of the palette card: its mod-1…4 tab bindings live here.
pub(super) const KEY_CONTEXT: &str = "CommandPalette";
const RESULTS_FADE_BAND: f32 = 18.0;

pub(super) struct CommandPalette {
    search: Entity<ComposerInput>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    tab: Tab,
    /// The highlighted row, kept by identity so results arriving around it
    /// never move it; `active_index` is where it was, for when it vanishes.
    active: Option<RowKey>,
    active_index: usize,
    enter_press: EnterPress,
    // Claim focus during mount so the shell does not restore the composer
    // while this input is still absent from the dispatch tree.
    focus_pending: bool,
    scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

#[derive(Default)]
struct EnterPress {
    down: bool,
}

impl EnterPress {
    fn press(&mut self, is_held: bool) -> bool {
        let was_down = std::mem::replace(&mut self.down, true);
        !was_down && !is_held
    }

    fn release(&mut self) {
        self.down = false;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Tab {
    #[default]
    All,
    Commands,
}

impl Tab {
    // Aurea has one chat and no workspace files: commands only.
    pub(super) const ALL: [Tab; 2] = [Tab::All, Tab::Commands];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Commands => "Commands",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// The neighbouring tab, wrapping (tab / shift-tab).
    fn step(self, forward: bool) -> Self {
        let count = Self::ALL.len();
        let next = if forward {
            self.index() + 1
        } else {
            self.index() + count - 1
        };
        Self::ALL[next % count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    QuickActions,
    Commands,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::QuickActions => "Quick Actions",
            Self::Commands => "Commands",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Command {
    NewChat,
    Trading,
    Settings,
    Theme(AppearanceMode),
    ArchiveThread,
}

impl Command {
    fn label(&self) -> &'static str {
        match self {
            Self::NewChat => "New chat",
            Self::Trading => "Abrir mesa de trading",
            Self::Settings => "Open settings",
            Self::Theme(AppearanceMode::System) => "Switch to system theme",
            Self::Theme(AppearanceMode::Light) => "Switch to light theme",
            Self::Theme(AppearanceMode::Dark) => "Switch to dark theme",
            Self::ArchiveThread => "Archive thread",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Self::NewChat => icons::PEN_NEW_SQUARE,
            Self::Trading => icons::WALLET,
            Self::Settings => icons::SETTINGS,
            Self::Theme(mode) => mode.icon(),
            Self::ArchiveThread => icons::ARCHIVE_MINIMALISTIC,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum RowKind {
    Command(Command),
}

#[derive(Clone, Debug, PartialEq)]
struct PaletteRow {
    section: Section,
    kind: RowKind,
}

/// A row's identity across result refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RowKey(Section, String);

impl PaletteRow {
    fn new(section: Section, kind: RowKind) -> Self {
        Self { section, kind }
    }

    fn key(&self) -> RowKey {
        let id = match &self.kind {
            RowKind::Command(command) => format!("command:{}", command.label()),
        };
        RowKey(self.section, id)
    }
}

/// One titled group of rows, already limited for the tab.
#[derive(Clone, Debug, PartialEq)]
struct PaletteSection {
    section: Section,
    rows: Vec<PaletteRow>,
    /// Shown beside the title: "Searching…", or why the search failed.
    status: Option<SharedString>,
}

impl PaletteSection {
    fn new(section: Section, kinds: impl IntoIterator<Item = RowKind>) -> Self {
        Self {
            section,
            rows: kinds
                .into_iter()
                .map(|kind| PaletteRow::new(section, kind))
                .collect(),
            status: None,
        }
    }

    fn with_status(mut self, status: Option<SharedString>) -> Self {
        self.status = status;
        self
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.status.is_none()
    }
}

fn matches_query(query: &str, text: &str) -> bool {
    let text = text.to_lowercase();
    query.split_whitespace().all(|word| text.contains(word))
}

/// `can_archive`: a focused, unarchived chat exists for Archive thread.
fn commands_for(query: &str, is_dark: bool, can_archive: bool) -> Vec<Command> {
    [
        Command::NewChat,
        Command::Trading,
        Command::Settings,
        Command::Theme(if is_dark {
            AppearanceMode::Light
        } else {
            AppearanceMode::Dark
        }),
    ]
    .into_iter()
    .chain(can_archive.then_some(Command::ArchiveThread))
    .filter(|command| matches_query(query, command.label()))
    .collect()
}

/// The scroll container's child index of flat row `row_ix`: each section
/// mounts a header before its rows.
fn child_index(sections: &[PaletteSection], row_ix: usize) -> usize {
    let mut rows_before = 0;
    for (section_ix, section) in sections.iter().enumerate() {
        if row_ix < rows_before + section.rows.len() {
            return row_ix + section_ix + 1;
        }
        rows_before += section.rows.len();
    }
    row_ix + sections.len()
}

/// The child to scroll into view for row `row_ix`: its section header when
/// it is the section's first row, so the header stays visible with it.
fn scroll_child_index(sections: &[PaletteSection], row_ix: usize) -> usize {
    let mut rows_before = 0;
    for (section_ix, section) in sections.iter().enumerate() {
        if row_ix < rows_before + section.rows.len() {
            let child = row_ix + section_ix + 1;
            return if row_ix == rows_before { child - 1 } else { child };
        }
        rows_before += section.rows.len();
    }
    child_index(sections, row_ix)
}

/// Navigation order: every row of every section, headers skipped.
fn flat_rows(sections: &[PaletteSection]) -> Vec<&PaletteRow> {
    sections.iter().flat_map(|section| &section.rows).collect()
}

/// Where the row `active` sits in `rows`: found by identity when still
/// listed, else `fallback` clamped.
fn row_position(active: Option<&RowKey>, fallback: usize, rows: &[&PaletteRow]) -> usize {
    active
        .and_then(|key| rows.iter().position(|row| &row.key() == key))
        .unwrap_or_else(|| fallback.min(rows.len().saturating_sub(1)))
}

impl CommandPalette {
    fn active_position(&self, rows: &[&PaletteRow]) -> usize {
        row_position(self.active.as_ref(), self.active_index, rows)
    }

    fn set_active(&mut self, rows: &[&PaletteRow], index: usize) {
        self.active_index = index;
        self.active = rows.get(index).map(|row| row.key());
    }

    fn reset_active(&mut self) {
        self.active = None;
        self.active_index = 0;
    }
}

impl Shell {
    pub(super) fn reset_command_palette_key_state(&mut self) {
        if let Some(palette) = self.command_palette.as_mut() {
            palette.enter_press.release();
        }
    }

    pub(super) fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.close_command_palette(window, cx);
            return;
        }
        self.close_add_space(cx);
        let search = cx.new(|cx| {
            ComposerInput::with_context("Search threads, commands and files…", "PaletteSearch", cx)
        });
        let events = cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(palette) = this.command_palette.as_mut() {
                    palette.reset_active();
                    palette.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                }
                cx.notify();
            }
        });
        let previous_focus = window.focused(cx);
        self.command_palette = Some(CommandPalette {
            search,
            focus: cx.focus_handle(),
            previous_focus,
            tab: Tab::All,
            active: None,
            active_index: 0,
            enter_press: EnterPress::default(),
            focus_pending: true,
            scroll: gpui::ScrollHandle::new(),
            _search_events: events,
        });
        cx.notify();
    }

    pub(super) fn select_palette_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        if palette.tab != tab {
            palette.tab = tab;
            palette.reset_active();
            palette.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            cx.notify();
        }
    }

    pub(super) fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.command_palette.take() {
            if let Some(focus) = palette.previous_focus {
                window.focus(&focus, cx);
            }
            cx.notify();
        }
    }

    /// The open palette's sections for its tab and query.
    fn palette_sections(&self, cx: &App) -> Vec<PaletteSection> {
        let Some(palette) = &self.command_palette else {
            return Vec::new();
        };
        let query = palette.search.read(cx).text().trim().to_lowercase();
        let can_archive = self.state.read(cx).archivable_selected_chat().is_some();
        let commands = |section| {
            PaletteSection::new(
                section,
                commands_for(&query, Theme::of(cx).appearance.is_dark(), can_archive)
                    .into_iter()
                    .map(RowKind::Command),
            )
        };
        let sections = match (palette.tab, query.is_empty()) {
            (_, true) => vec![commands(Section::QuickActions)],
            (_, false) => vec![commands(Section::Commands)],
        };
        sections
            .into_iter()
            .filter(|section| !section.is_empty())
            .collect()
    }

    /// Pointer motion moves the highlight, so hover and keyboard never light
    /// two rows. Motion only: rows scrolling under a resting pointer must not
    /// steal the keyboard's place.
    fn hover_command(&mut self, key: RowKey, ix: usize, cx: &mut Context<Self>) {
        if let Some(palette) = self.command_palette.as_mut()
            && palette.active.as_ref() != Some(&key)
        {
            palette.active = Some(key);
            palette.active_index = ix;
            cx.notify();
        }
    }

    fn move_palette_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        let sections = self.palette_sections(cx);
        let rows = flat_rows(&sections);
        let count = rows.len();
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        if count == 0 {
            return;
        }
        let current = palette.active_position(&rows);
        let next = if down {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        palette.set_active(&rows, next);
        // A section's first row brings its header along, and the very first
        // row returns to the top, so a header never hides above the edge.
        if next == 0 {
            palette.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        } else {
            palette
                .scroll
                .scroll_to_item(scroll_child_index(&sections, next));
        }
        cx.notify();
    }

    fn activate_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sections = self.palette_sections(cx);
        let rows = flat_rows(&sections);
        let Some(row) = self
            .command_palette
            .as_ref()
            .and_then(|palette| rows.get(palette.active_position(&rows)))
            .map(|row| (*row).clone())
        else {
            return;
        };
        self.activate_palette_row(row, window, cx);
    }

    fn activate_palette_row(
        &mut self,
        row: PaletteRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let RowKind::Command(Command::Theme(mode)) = row.kind {
            // Keep the palette open so this ordinary action updates to its next state.
            crate::appearance::set_mode(mode, cx);
            cx.notify();
            return;
        }
        self.close_command_palette(window, cx);
        match row.kind {
            RowKind::Command(Command::NewChat) => self.open_new_session(None, cx),
            RowKind::Command(Command::Trading) => self.open_trading(cx),
            RowKind::Command(Command::Settings) => self.open_last_settings(cx),
            RowKind::Command(Command::Theme(_)) => unreachable!(),
            RowKind::Command(Command::ArchiveThread) => self.archive_selected_chat(cx),
        }
    }

    fn command_shortcut(&self, command: &Command) -> Option<String> {
        let id = match command {
            Command::NewChat => ShortcutId::NewSession,
            Command::Settings => return Some(crate::settings::badge_combo("mod-,")),
            Command::ArchiveThread => ShortcutId::ArchiveSession,
            Command::Trading => return None,
            Command::Theme(_) => return None,
        };
        let combo = self.settings.keymap.get(id);
        let valid = Keystroke::parse(&platform_combo(combo)).is_ok();
        Some(crate::settings::badge_combo(if valid {
            combo
        } else {
            id.default_combo()
        }))
    }

    fn render_palette_row(
        &mut self,
        row: &PaletteRow,
        ix: usize,
        active: bool,
        query: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        Some(match &row.kind {
            RowKind::Command(command) => {
                let label = command.label();
                let shortcut = self.command_shortcut(command);
                let target = row.clone();
                popover::menu_row(theme, active, format!("command-action-{ix}"))
                    .id(("command-action", ix))
                    .rounded(px(popover::PALETTE_ITEM_RADIUS))
                    .role(gpui::Role::Button)
                    .aria_label(label)
                    .min_h(px(30.0))
                    .py(px(4.0))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_palette_row(target.clone(), window, cx)
                    }))
                    .child(
                        icon(command.icon())
                            .size(px(16.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().min_w_0().child(popover::search_highlight(
                        label.into(),
                        Some(query),
                        theme,
                    )))
                    .when_some(shortcut, |row, shortcut| {
                        row.child(popover::kbd_hint(theme, &shortcut))
                    })
                    .into_any_element()
            }
        })
    }

    pub(super) fn render_command_palette(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sections = self.palette_sections(cx);
        let palette = self.command_palette.as_mut()?;
        if std::mem::take(&mut palette.focus_pending) {
            window.focus(&palette.search.focus_handle(cx), cx);
        }
        let row_count = sections
            .iter()
            .map(|section| section.rows.len())
            .sum::<usize>();
        let active = palette.active_position(&flat_rows(&sections));
        let search = palette.search.clone();
        let query = search.read(cx).text().to_string();
        let focus = palette.focus.clone();
        let scroll = palette.scroll.clone();
        let current_tab = palette.tab;
        let theme = Theme::of(cx).for_popup();
        let mut children = Vec::new();
        let mut ix = 0;
        for (section_ix, section) in sections.iter().enumerate() {
            // End spacing belongs to the content, so it scrolls out of the
            // fade instead of leaving a permanent gutter beside the chrome.
            let mut header = div()
                .flex_none()
                .when(section_ix == 0, |header| header.pt(px(8.0)));
            if section_ix > 0 {
                header = header.child(spaces::sidebar_separator(&theme).w_full().my(px(8.0)));
            }
            children.push(
                header
                    .child(palette_section_header(
                        &theme,
                        section.section.title(),
                        section.status.clone(),
                    ))
                    .into_any_element(),
            );
            for row in &section.rows {
                let key = row.key();
                let content = self.render_palette_row(row, ix, ix == active, &query, &theme, cx);
                let Some(content) = content else {
                    ix += 1;
                    continue;
                };
                children.push(
                    div()
                        .id(("command-result", ix))
                        .flex_none()
                        .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                            this.hover_command(key.clone(), ix, cx)
                        }))
                        .when(ix + 1 == row_count, |row| row.pb(px(8.0)))
                        .child(div().px(px(8.0)).child(content))
                        .into_any_element(),
                );
                ix += 1;
            }
        }
        let body = div()
            .id("command-results")
            .min_h_0()
            .max_h(px(palette_results_height(viewport)))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_LIST_GAP))
            .children(children)
            .when(sections.is_empty(), |el| {
                let _ = current_tab;
                let (title, hint) = ("No results", "Try a command or a chat title.");
                el.child(palette_empty(&theme, title, hint))
            });
        let body = palette_results_fade(body, &scroll);
        let tabs = div()
            .flex_none()
            .px(px(12.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.06))
            .children(Tab::ALL.into_iter().map(|tab| {
                let combo = crate::settings::badge_combo(&format!("mod-{}", tab.index() + 1));
                // The app's tab chip (right-pane surfaces, PR board), not a
                // menu row: its height, type and washes.
                crate::surface_chrome::tab(("command-tab", tab.index()), tab == current_tab, &theme)
                    .px(px(8.0))
                    .role(gpui::Role::Tab)
                    .aria_label(tab.label())
                    .gap(px(6.0))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_palette_tab(tab, cx)))
                    .child(tab.label())
                    .child(popover::kbd_hint(&theme, &combo))
            }));
        let card = palette_card("command-palette", &focus, viewport, &theme)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, action: &SelectPaletteTab, _, cx| {
                if let Some(tab) = Tab::ALL.get(action.0) {
                    this.select_palette_tab(*tab, cx);
                }
            }))
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    match event.keystroke.key.as_str() {
                        "up" | "down" => {
                            this.move_palette_selection(event.keystroke.key == "down", cx)
                        }
                        "enter" => {
                            let activate = this
                                .command_palette
                                .as_mut()
                                .is_some_and(|palette| palette.enter_press.press(event.is_held));
                            if !activate {
                                cx.stop_propagation();
                                return;
                            }
                            this.activate_palette_selection(window, cx);
                        }
                        "tab" => {
                            let forward = !event.keystroke.modifiers.shift;
                            if let Some(tab) = this
                                .command_palette
                                .as_ref()
                                .map(|palette| palette.tab.step(forward))
                            {
                                this.select_palette_tab(tab, cx);
                            }
                        }
                        "escape" => this.close_command_palette(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }),
            )
            .on_key_up(cx.listener(|this, event: &gpui::KeyUpEvent, _, cx| {
                if event.keystroke.key == "enter" {
                    if let Some(palette) = this.command_palette.as_mut() {
                        palette.enter_press.release();
                    }
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.close_command_palette(window, cx)),
            )
            .child(palette_header(
                &theme,
                search.into_any_element(),
                popover::kbd_hint(&theme, &crate::settings::badge_combo("mod-k")),
            ))
            .child(tabs)
            .child(body)
            .child(
                palette_footer()
                    .child(command_key_hint(&theme, "↑ ↓", "Navigate"))
                    .child(command_key_hint(&theme, "↵", "Open"))
                    .child(command_key_hint(&theme, "Esc", "Close")),
            );
        Some(palette_overlay(viewport, card))
    }
}

/// A section's title, in the sidebar's secondary text style, with its
/// search status trailing.
fn palette_section_header(
    theme: &Theme,
    title: &'static str,
    status: Option<SharedString>,
) -> gpui::Div {
    div()
        .px(px(16.0))
        .pt(px(2.0))
        .pb(px(4.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(theme.text_muted)
        .child(title)
        .when_some(status, |header, status| {
            header.child(div().min_w_0().truncate().opacity(0.7).child(status))
        })
}

/// The results list's max height; shared so every palette sits at one size.
pub(super) fn palette_results_height(viewport: gpui::Size<Pixels>) -> f32 {
    (f32::from(viewport.height) - 180.0).clamp(100.0, 360.0)
}

/// Scroll fades at whichever list edge hides rows.
pub(super) fn palette_results_fade(
    body: impl IntoElement,
    scroll: &gpui::ScrollHandle,
) -> crate::edge_fade::EdgeFaded {
    crate::edge_fade::edge_faded(RESULTS_FADE_BAND, true, true, body).fade_overflow_y(scroll)
}

pub(super) fn palette_empty(
    theme: &Theme,
    title: impl Into<SharedString>,
    hint: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .w_full()
        .py(px(24.0))
        .px(px(16.0))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.0))
        .text_size(crate::typography::ui_rems(13.0))
        .child(title.into())
        .child(div().text_color(theme.text_muted).child(hint.into()))
}

/// The palette's glass card; callers add key handling and sections.
pub(super) fn palette_card(
    id: &'static str,
    focus: &FocusHandle,
    viewport: gpui::Size<Pixels>,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .track_focus(focus)
        .w(px(560.0_f32.min(f32::from(viewport.width) - 32.0)))
        .flex()
        .flex_col()
        .rounded(px(16.0))
        .border_1()
        .border_color(theme.border)
        .when(!theme.is_frost(), |el| el.shadow_lg())
        .bg(popover::surface_bg(theme))
        .text_color(theme.text)
}

pub(super) fn palette_header(theme: &Theme, search: AnyElement, hint: gpui::Div) -> gpui::Div {
    div()
        .min_h(px(44.0))
        .flex_none()
        .px(px(16.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .border_b_1()
        .border_color(crate::theme::hairline(0.06))
        .child(popover::palette_search_icon(theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(crate::typography::ui_rems(14.0))
                .child(search),
        )
        .child(hint)
}

pub(super) fn palette_footer() -> gpui::Div {
    div()
        .flex_none()
        .px(px(16.0))
        .py(px(7.0))
        .border_t_1()
        .border_color(crate::theme::hairline(0.06))
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(12.0))
}

/// The card's chrome beside its results at their tallest: the search header,
/// New project's breadcrumbs and the footer (Cmd+K has no crumbs and simply
/// ends shorter).
const PALETTE_CHROME_HEIGHT: f32 = 120.0;

/// Where a palette card's top sits: where a card at its tallest would sit
/// centered, never under the titlebar. Results growing or shrinking only
/// move the bottom edge, and Cmd+K and New project share the line, so
/// typing or switching between them never jumps the card.
fn palette_top(viewport: gpui::Size<Pixels>) -> f32 {
    let tallest = palette_results_height(viewport) + PALETTE_CHROME_HEIGHT;
    ((f32::from(viewport.height) - tallest) / 2.0).max(Theme::TITLEBAR_HEIGHT + 16.0)
}

/// Mount a palette card over the scrimmed window, frosted like the composer.
pub(super) fn palette_overlay(
    viewport: gpui::Size<Pixels>,
    card: gpui::Stateful<gpui::Div>,
) -> AnyElement {
    // Match the composer's 16px backdrop blur, including its opaque fallback.
    let card = crate::frost::frosted(16.0, crate::frost::MENU_BLUR, card);
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    // Match glass modals: quiet the background while
                    // preserving its color through the frosted palette.
                    .bg(popover::scrim_alpha(0.35))
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(palette_top(viewport)))
                    .child(card),
            ),
    )
    .priority(2)
    .into_any_element()
}

/// A footer legend: the keys on a key cap (footers keep their caps, unlike
/// the plain inline legends) and the verb after it. Arrow and return keys
/// are icons, which sit optically centered on the cap where font glyphs
/// don't; other keys ("Esc") are text.
pub(super) fn command_key_hint(theme: &Theme, keys: &str, label: &'static str) -> gpui::Div {
    let glyph = |path: &'static str| {
        crate::icons::icon(path)
            .size(px(12.5))
            .text_color(theme.text_muted)
    };
    let cap = popover::key_cap(theme);
    let cap = match keys {
        "↵" => cap.child(glyph(crate::icons::RETURN)),
        "←" => cap.child(glyph(crate::icons::ARROW_LEFT)),
        "↑ ↓" => cap
            .child(glyph(crate::icons::ARROW_UP))
            .child(glyph(crate::icons::ARROW_DOWN)),
        _ => cap
            .font_family(popover::KEY_GLYPH_FONT)
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_muted)
            .child(SharedString::from(keys.to_string())),
    };
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(cap)
        .child(
            div()
                .text_size(crate::typography::ui_rems(10.0))
                .text_color(theme.text_muted)
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::AppearanceMode;
    use gpui::{AppContext, TestAppContext};

    fn palette_window(cx: &mut TestAppContext) -> (gpui::WindowHandle<Shell>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        (window, dir)
    }

    fn chat(id: &str, parent: Option<&str>, archived: bool, age: i64) -> zeron_proto::Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "deviceId": "local", "archived": archived,
            "parentChatId": parent,
            "createdAt": "2026-09-20T00:00:00Z".parse::<chrono::DateTime<Utc>>().unwrap()
                - chrono::Duration::minutes(age),
        }))
        .unwrap()
    }

    #[test]
    fn archive_is_offered_only_with_a_focused_chat() {
        assert!(!commands_for("archive", true, false).contains(&Command::ArchiveThread));
        assert_eq!(
            commands_for("archive", true, true),
            vec![Command::ArchiveThread]
        );
    }

    /// A shell with the real app keymap and two sidebar sessions.
    fn keyed_palette_window(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<Shell>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            crate::composer::init(cx, ComposerSendBehavior::default());
            apply_keymap(
                cx,
                &KeymapConfig::default(),
                ComposerSendBehavior::default(),
            );
        });
        let (window, _) = palette_window(cx);
        window
            .update(cx, |shell, _, cx| {
                shell.settings.sidebar_organization = SidebarOrganization::InOneList;
                shell.state.update(cx, |state, _| {
                    state.connection = zeron_proto::view::ConnectionStatus::Ready;
                    state.workspace_scope = Some(WorkspaceScope::Local);
                    state.local_device_id = Some("local".into());
                    state.apply_chats(vec![
                        chat("first", None, false, 0),
                        chat("second", None, false, 1),
                    ]);
                    state.selected_chat = Some("first".into());
                });
            })
            .unwrap();
        (window, dir)
    }

    fn palette_tab(window: gpui::WindowHandle<Shell>, cx: &mut TestAppContext) -> Option<Tab> {
        window
            .read_with(cx, |shell, _| shell.command_palette.as_ref().map(|p| p.tab))
            .unwrap()
    }

    fn selected_chat(window: gpui::WindowHandle<Shell>, cx: &mut TestAppContext) -> Option<String> {
        window
            .read_with(cx, |shell, cx| shell.state.read(cx).selected_chat.clone())
            .unwrap()
    }

    #[gpui::test]
    fn digit_shortcuts_jump_to_sessions_once_the_palette_closes(cx: &mut TestAppContext) {
        let (window, _dir) = keyed_palette_window(cx);
        window
            .update(cx, |shell, window, cx| {
                shell.toggle_command_palette(window, cx);
                shell.close_command_palette(window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();

        cx.simulate_keystrokes(window.into(), &platform_combo("mod-2"));
        assert_eq!(palette_tab(window, cx), None);
        assert_eq!(selected_chat(window, cx).as_deref(), Some("second"));
    }

    #[test]
    fn x11_unflagged_enter_repeats_activate_once_until_release() {
        let mut enter = EnterPress::default();
        // The pinned X11 backend drops synthetic repeat releases and emits
        // every repeated KeyDownEvent with is_held=false.
        assert!(enter.press(false));
        for _ in 0..35 {
            assert!(!enter.press(false));
        }
        enter.release();
        assert!(enter.press(false));
    }

    #[test]
    fn flagged_enter_repeats_do_not_activate() {
        let mut enter = EnterPress::default();
        assert!(!enter.press(true));
        assert!(!enter.press(false));
        enter.release();
        assert!(enter.press(false));
        assert!(!enter.press(true));
    }

    #[test]
    fn action_search_hides_empty_section_and_preserves_order() {
        assert_eq!(
            commands_for("", true, false),
            vec![
                Command::NewChat,
                Command::Trading,
                Command::Settings,
                Command::Theme(AppearanceMode::Light)
            ]
        );
        assert_eq!(
            commands_for("new", true, false),
            vec![Command::NewChat]
        );
        assert_eq!(
            commands_for("settings", true, false),
            vec![Command::Settings]
        );
        assert_eq!(
            commands_for("theme", true, false),
            vec![Command::Theme(AppearanceMode::Light)]
        );
        assert!(commands_for("deployment", true, false).is_empty());
    }

    #[test]
    fn theme_action_targets_the_opposite_resolved_appearance() {
        assert_eq!(
            commands_for("theme", true, false),
            vec![Command::Theme(AppearanceMode::Light)]
        );
        assert_eq!(
            commands_for("theme", false, false),
            vec![Command::Theme(AppearanceMode::Dark)]
        );
        assert_eq!(
            commands_for("light", true, false),
            vec![Command::Theme(AppearanceMode::Light)]
        );
        assert_eq!(
            commands_for("dark", false, false),
            vec![Command::Theme(AppearanceMode::Dark)]
        );
    }

    #[test]
    fn search_matches_words_across_chat_metadata() {
        assert!(matches_query(
            "mac auth",
            "Fix authentication Zeron @ MacBook main"
        ));
        assert!(matches_query("  ", "Any chat"));
        assert!(!matches_query("mac windows", "Zeron @ MacBook"));
    }
}
