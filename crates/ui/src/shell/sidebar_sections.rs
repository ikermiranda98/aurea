//! Account-synced sections; local workspaces keep their settings on this device.
use super::*;
use crate::settings::SidebarSection;
use zeron_proto::{SidebarPinChange, SidebarSectionChange};

pub(super) struct SectionDialog {
    profile: String,
    id: Option<String>,
    input: Entity<ComposerInput>,
    focus_pending: bool,
    _events: Subscription,
}

impl Shell {
    pub(super) fn active_sidebar_sections(&self, cx: &App) -> Vec<SidebarSection> {
        let local = self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local);
        let mut sections = if local {
            self.active_sidebar_pin_profile_key(cx)
                .and_then(|key| self.settings.sidebar_sections_by_profile.get(&key).cloned())
                .unwrap_or_default()
        } else {
            self.state.read(cx).sidebar_preferences.sections.clone()
        };
        if !local && self.optimistic_sidebar_pins(cx).is_some() {
            if let Some(pending) = &self.sidebar_pin_write {
                for change in &pending.queue {
                    change.project_sections(&mut sections);
                }
            }
        }
        sections
    }

    pub(super) fn assign_sidebar_section(
        &mut self,
        chat: &str,
        target: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.change_sidebar_section(
            SidebarSectionChange::Assign {
                session_id: chat.to_owned(),
                section_id: target.map(str::to_owned),
                after: None,
                before: None,
            },
            cx,
        );
    }

    pub(super) fn change_sidebar_section(
        &mut self,
        change: SidebarSectionChange,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(key) = self.active_sidebar_pin_profile_key(cx) else {
            return false;
        };
        if self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local) {
            if let SidebarSectionChange::Assign {
                session_id,
                section_id,
                ..
            } = &change
            {
                if section_id
                    .as_ref()
                    .is_some_and(|id| !self.active_sidebar_sections(cx).iter().any(|s| &s.id == id))
                {
                    return false;
                }
                if let Some(pins) = self
                    .settings
                    .sidebar_pinned_session_ids_by_profile
                    .get_mut(&key)
                {
                    // Clearing membership after a local pin must preserve the pin.
                    if section_id.is_some() {
                        pins.retain(|id| id != session_id);
                    }
                }
            }
            change.project(
                self.settings
                    .sidebar_sections_by_profile
                    .entry(key)
                    .or_default(),
            );
            self.schedule_save(cx);
            cx.notify();
            true
        } else {
            if !self.state.read(cx).sidebar_preferences.can_edit() {
                self.show_notice(crate::toast::ToastKind::Error, "Sidebar is still syncing. Try again shortly.", cx);
                cx.notify();
                return false;
            }
            self.queue_sidebar_pin_write(key, SidebarPinChange::Section { change }, cx)
        }
    }

    pub(super) fn migrate_sidebar_sections(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        if state.workspace_scope == Some(WorkspaceScope::Local) || !state.sidebar_preferences.synced
        {
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let Some(key) = self.active_sidebar_pin_profile_key(cx) else {
            return;
        };
        if self
            .sidebar_section_migration
            .as_ref()
            .is_some_and(|(profile, previous)| profile == &key && previous.same_connection(&engine))
        {
            return;
        }
        let Some(sections) = self
            .settings
            .sidebar_sections_by_profile
            .get(&key)
            .filter(|s| !s.is_empty())
            .cloned()
        else {
            return;
        };
        if self.queue_sidebar_pin_write(
            key.clone(),
            SidebarPinChange::Section {
                change: SidebarSectionChange::Import { sections },
            },
            cx,
        ) {
            self.sidebar_section_migration = Some((key, engine));
        }
    }

    pub(super) fn open_section_dialog(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let Some(profile) = self.active_sidebar_pin_profile_key(cx) else {
            return;
        };
        self.close_sidebar_view_menu(cx);
        self.section_menu = None;
        let name = self
            .active_sidebar_sections(cx)
            .into_iter()
            .find(|section| Some(&section.id) == id.as_ref())
            .map(|s| s.name)
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Section name", cx));
        input.update(cx, |input, cx| input.set_text(name, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_section_dialog(cx);
            } else {
                cx.notify();
            }
        });
        self.section_dialog = Some(SectionDialog {
            profile,
            id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_section_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.section_dialog.as_ref() else {
            return;
        };
        if self.active_sidebar_pin_profile_key(cx).as_ref() != Some(&dialog.profile) {
            self.section_dialog = None;
            cx.notify();
            return;
        }
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() || name.chars().count() > 120 {
            return;
        }
        let dialog = self.section_dialog.take().unwrap();
        let change = match dialog.id.clone() {
            Some(id) => SidebarSectionChange::Rename { id, name },
            None => SidebarSectionChange::Create {
                id: uuid::Uuid::new_v4().to_string(),
                name,
            },
        };
        if !self.change_sidebar_section(change, cx) {
            self.section_dialog = Some(dialog);
        }
        cx.notify();
    }

    fn delete_sidebar_section(&mut self, id: &str, cx: &mut Context<Self>) {
        self.section_menu = None;
        self.cancel_sidebar_session_transfer(cx);
        self.change_sidebar_section(SidebarSectionChange::Delete { id: id.to_owned() }, cx);
        cx.notify();
    }

    fn archive_sidebar_section(&mut self, id: &str, cx: &mut Context<Self>) {
        self.section_menu = None;
        let Some(section) = self
            .active_sidebar_sections(cx)
            .into_iter()
            .find(|s| s.id == id)
        else {
            return;
        };
        let state = self.state.read(cx);
        let ids: Vec<_> = state
            .chats
            .iter()
            .filter(|chat| !chat.archived && section.session_ids.contains(&chat.id))
            .map(|chat| chat.id.clone())
            .collect();
        if ids.is_empty() {
            cx.notify();
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        // A separate task preserves every archive request; the single-row mutation
        // slot intentionally replaces previous work and cannot be used in a loop.
        cx.spawn(async move |this, cx| {
            let mut failed = 0;
            for chat_id in ids {
                if engine.client().call(methods::MUTATE, serde_json::json!({"op":"setChatArchived", "chatId":chat_id, "archived":true})).await.is_err() { failed += 1; }
            }
            if failed > 0 {
                let _ = this.update(cx, |this, cx| {
                    if this.state.read(cx).engine().is_some_and(|current| current.same_connection(&engine)) {
                        this.show_notice(crate::toast::ToastKind::Error, format!("Could not archive {failed} sessions. Try again."), cx);
                        cx.notify();
                    }
                });
            }
        }).detach();
        cx.notify();
    }

    fn activate_section_menu(&mut self, action: usize, id: &str, cx: &mut Context<Self>) {
        match action {
            0 => self.open_section_dialog(Some(id.to_string()), cx),
            1 => self.archive_sidebar_section(id, cx),
            _ => self.delete_sidebar_section(id, cx),
        }
    }

    pub(super) fn render_section_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let mut overlays = Vec::new();
        if let Some((id, position)) = self.section_menu.clone() {
            if !self.active_sidebar_sections(cx).iter().any(|s| s.id == id) {
                self.section_menu = None;
                return overlays;
            }
            let mut card = popover::popover_card(&theme)
                .w(px(180.0))
                .flex()
                .flex_col()
                .track_focus(&self.section_menu_focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => this.section_menu = None,
                        "up" | "down" => {
                            this.section_menu_active = popover::menu_step(
                                this.section_menu_active,
                                3,
                                if event.keystroke.key == "up" { -1 } else { 1 },
                            )
                        }
                        "enter" => {
                            if let (Some(action), Some((id, _))) =
                                (this.section_menu_active, this.section_menu.clone())
                            {
                                this.activate_section_menu(action, &id, cx);
                            }
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                    cx.notify();
                }))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.section_menu = None;
                    cx.notify();
                }));
            for (action, label) in ["Edit section", "Archive all", "Delete"]
                .into_iter()
                .enumerate()
            {
                let target = id.clone();
                card = card.child(
                    popover::menu_row_nav(
                        &theme,
                        false,
                        self.section_menu_active == Some(action),
                        format!("section-action-{action}"),
                    )
                    .id(("section-action", action))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.activate_section_menu(action, &target, cx);
                        cx.stop_propagation();
                    }))
                    .child(label),
                );
            }
            overlays.push(popover::menu_at(
                "section-context-menu",
                position,
                card.into_any_element(),
                None,
            ));
        }
        if let Some(dialog) = &mut self.section_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let edit = dialog.id.is_some();
            let input = dialog.input.clone();
            let name = input.read(cx).text().trim().to_string();
            let valid = !name.is_empty() && name.chars().count() <= 120;
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.section_dialog = None;
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(popover::dialog_title(
                            &theme,
                            if edit { "Edit section" } else { "New section" },
                        ))
                        .child(
                            div()
                                .id("section-dialog-close")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.section_dialog = None;
                                    cx.notify();
                                }))
                                .role(gpui::Role::Button)
                                .aria_label("Close")
                                .tooltip(crate::settings::widgets::text_tooltip("Close"))
                                .child(
                                    icon(icons::CLOSE)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .mt(px(8.0))
                        .text_color(theme.text_muted)
                        .child("Group sessions however you like"),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "section-cancel")
                                .id("section-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.section_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(
                                &theme,
                                if edit { "Save" } else { "Create section" },
                            )
                            .id("section-save")
                            .opacity(if valid { 1.0 } else { 0.5 })
                            .on_click(cx.listener(|this, _, _, cx| this.submit_section_dialog(cx))),
                        ),
                );
            overlays.push(popover::modal(
                "section-dialog",
                viewport,
                card.into_any_element(),
            ));
        }
        overlays
    }
}
