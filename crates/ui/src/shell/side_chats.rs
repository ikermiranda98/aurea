use super::*;

pub(super) struct SideChatTab {
    pub state: Entity<AppState>,
    transcript: Entity<Transcript>,
    pub(super) composer: Entity<Composer>,
    /// A fork its host has yet to make (see [`Shell::fork_chat`]). The tab
    /// shows its progress, or why it failed, in place of a composer whose
    /// first send would mint the chat a second way.
    pub(super) fork: Option<PendingFork>,
    _events: Vec<Subscription>,
}

pub(super) struct PendingFork {
    /// The request as sent. Retry replays it unchanged: forking is
    /// idempotent per chat id.
    params: serde_json::Value,
    /// The remote host doing the work, named while it does.
    host: Option<SharedString>,
    error: Option<SharedString>,
    /// Its request is in flight. Per tab: one fork never waits on another.
    running: bool,
}

impl Shell {
    pub(super) fn close_empty_right_pane(&mut self, key: &str, cx: &mut Context<Self>) {
        let empty = if key == self.panel_key(cx) {
            self.right_surface_rows(cx).is_empty()
        } else {
            self.right_tabs.get(key).is_none_or(Vec::is_empty)
        };
        if empty {
            if key == self.panel_key(cx) && self.right_pane_open(cx) {
                self.toggle_right_pane(cx);
            } else {
                self.panels.update(key, |p| p.changes_open = false);
            }
        }
    }

    /// Side-chat creation failed (nothing to fork yet, an engine or remote
    /// device error): say why in the conversation's composer.
    fn show_side_chat_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let message = message.into();
        self.composer
            .update(cx, |composer, cx| composer.show_error(message, cx));
    }

    /// Ask the host for the fork side chat tab `id` is waiting on, unless
    /// that request is already in flight.
    fn run_fork(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(fork) = self
            .side_chats
            .get_mut(&id)
            .and_then(|tab| tab.fork.as_mut())
            .filter(|fork| !fork.running)
        else {
            return;
        };
        fork.error = None;
        fork.running = true;
        let params = fork.params.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<zeron_proto::Chat>(methods::FORK_SIDE_CHAT, params)
                .await;
            let _ = this.update(cx, |this, cx| {
                match (result, this.side_chats.get_mut(&id)) {
                    (Ok(chat), Some(tab)) if tab.fork.is_some() => this.finish_fork(id, chat, cx),
                    (Err(error), Some(tab)) => {
                        if let Some(fork) = &mut tab.fork {
                            fork.running = false;
                            fork.error = Some(error.to_string().into());
                        }
                    }
                    // Closed while forking: the fork still lists under its
                    // parent, and nothing reopens the tab.
                    (Err(error), None) => this.show_side_chat_error(error.to_string(), cx),
                    // Done already: its row opened the chat in this tab.
                    (Ok(_), _) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn retry_fork(&mut self, id: u64, cx: &mut Context<Self>) {
        self.run_fork(id, cx);
    }

    /// The host made the fork: its tab becomes the side chat, in place.
    fn finish_fork(&mut self, id: u64, chat: zeron_proto::Chat, cx: &mut Context<Self>) {
        let parent_listed = self
            .state
            .read(cx)
            .chats
            .iter()
            .any(|c| Some(&c.id) == chat.parent_chat_id.as_ref());
        if parent_listed {
            let tab = self.new_side_chat_tab(chat, false, cx);
            self.side_chats.insert(id, tab);
        } else if let Some(tab) = self.side_chats.get(&id) {
            // The parent went away meanwhile: close as a deleted side chat.
            tab.state
                .clone()
                .update(cx, |state, cx| state.select_chat(None, cx));
        }
    }

    /// Reveal the surface host of the pane keyed `key`: the live one through
    /// the user-visible path, a background conversation's by flag, so a
    /// fork that finishes after the user switched away opens ITS pane, not
    /// whichever is on screen.
    fn open_surfaces_for(&mut self, key: &str, cx: &mut Context<Self>) {
        if key == self.panel_key(cx) {
            // Programmatic, so an already-open pane is left alone.
            self.set_surfaces_open(true, cx);
        } else {
            // Rank the pane most recent, open or fit-hidden, so a narrow
            // window shows it on the next visit.
            self.panels.update(key, |p| p.changes_open = true);
            self.record_panel_open(AuxiliaryPanel::Right, key);
        }
    }

    /// A side chat's own state, transcript and composer over `chat`.
    fn new_side_chat_tab(
        &mut self,
        chat: zeron_proto::Chat,
        unsaved: bool,
        cx: &mut Context<Self>,
    ) -> SideChatTab {
        let chat_id = chat.id.clone();
        let parent = self.state.clone();
        let state = cx.new(|cx| AppState::side_chat_state(&parent, chat, unsaved, cx));
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        // Workspace file links open the editor and web links honor the
        // in-app preference, resolved in this side chat's context.
        let links = Self::session_links(Some(chat_id), cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        let composer = cx.new(|cx| {
            let mut composer = Composer::new(state.clone(), cx);
            composer.set_side_chat(cx);
            composer
        });
        let events = vec![
            cx.subscribe(&transcript, Self::on_transcript_event),
            cx.subscribe(&composer, {
                let transcript = transcript.clone();
                move |this: &mut Self, _, event, cx| {
                    match event {
                        ComposerEvent::WorkspaceCommand(command) => {
                            this.pending_workspace_command = Some(*command);
                            cx.notify();
                        }
                        // A side chat is already selected before its composer
                        // mounts, and it inherits its parent's checkout, so it
                        // never runs worktree setup of its own.
                        ComposerEvent::NewThreadTransitionStarted
                        | ComposerEvent::WorktreeSetup { .. }
                        | ComposerEvent::AnnotationsChanged(_)
                        | ComposerEvent::AnnotationHovered(_) => {}
                        ComposerEvent::Sent {
                            chat_id,
                            message_id,
                        } => transcript.update(cx, |t, cx| {
                            t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                        }),
                        ComposerEvent::Queued {
                            chat_id,
                            message_id,
                        } => transcript.update(cx, |t, cx| {
                            t.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                        }),
                    }
                }
            }),
            cx.observe(&state, |this, state, cx| {
                if state.read(cx).selected_chat.is_none() {
                    this.remove_deleted_side_chats(cx);
                }
                cx.notify();
            }),
        ];
        SideChatTab {
            state,
            transcript,
            composer,
            fork: None,
            _events: events,
        }
    }

    fn remove_deleted_side_chats(&mut self, cx: &mut Context<Self>) {
        let removed: Vec<_> = self
            .side_chats
            .iter()
            .filter(|(_, tab)| tab.state.read(cx).selected_chat.is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in removed {
            self.side_chats.remove(&id);
            let surface = RightSurface::SideChat(id);
            let keys: Vec<_> = self
                .right_tabs
                .iter_mut()
                .filter_map(|(key, tabs)| {
                    let contained = tabs.contains(&surface);
                    tabs.retain(|tab| *tab != surface);
                    contained.then(|| key.clone())
                })
                .collect();
            for key in keys {
                self.panels.forget_right_surface(&key, surface);
                let fallback = self.panels.right_surface_fallback(
                    &key,
                    self.right_tabs.get(&key).into_iter().flatten().copied(),
                );
                self.panels.update(&key, |panel| {
                    if panel.right_active == surface {
                        panel.right_active = fallback;
                    }
                });
                self.close_empty_right_pane(&key, cx);
            }
        }
    }

    pub(super) fn render_side_chat(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self
            .side_chats
            .get(&id)
            .filter(|tab| tab.state.read(cx).selected_chat.is_some())
        else {
            return self.render_surface_picker(cx);
        };
        if let Some(fork) = &tab.fork {
            let (host, error) = (fork.host.clone(), fork.error.clone());
            return self.render_pending_fork(id, host, error, cx);
        }
        let transcript = tab.transcript.clone();
        let composer = tab.composer.clone();
        // Share the main chat's docked width cap and responsive padding. Size
        // from the width the pane lays out at, not its mask, which an open or
        // close animates.
        let width = composer_target_width(
            self.right_content_width(self.right_target(cx)),
            settings::transcript_width(cx),
            true,
        );
        composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width(width, cx);
        });
        let pill = transcript.read(cx).jump_button_shown().then(|| {
            div()
                .absolute()
                .bottom(px(12.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(self.jump_pill(
                    "side-chat-jump",
                    "side-chat-jump-pill",
                    transcript.clone(),
                    cx,
                ))
        });
        self.chat_dropzone(("side-chat-dropzone", id), composer.clone(), cx)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        crate::edge_fade::edge_faded(
                            Theme::TRANSCRIPT_FADE_BAND,
                            true,
                            false,
                            div().size_full().child(transcript),
                        )
                        .inset_top(Theme::TITLEBAR_HEIGHT),
                    )
                    .children(pill),
            )
            .child(div().flex_none().child(composer))
            .child(Self::attachment_drop_overlay(Theme::of(cx)))
            .child(Self::mention_drop_overlay(Theme::of(cx)))
            .into_any_element()
    }

    /// A fork its host has yet to make: what it waits on, or why it failed
    /// and a retry.
    fn render_pending_fork(
        &mut self,
        id: u64,
        host: Option<SharedString>,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let body = match error {
            None => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(loaders::ring_spinner(
                    format!("pending-fork-loader-{id}"),
                    20.0,
                    theme.text_muted,
                ))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_faint)
                        .child(match host {
                            Some(host) => SharedString::from(format!("Forking on {host}…")),
                            None => SharedString::from("Forking…"),
                        }),
                )
                .into_any_element(),
            // An empty state, not an alarm: the engine's reason is the
            // detail under a plain title, and Retry is the pane's one action.
            Some(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .max_w(px(420.0))
                .text_center()
                .child(super::command_palette::palette_empty(
                    &theme,
                    match host {
                        Some(host) => SharedString::from(format!("Couldn't fork on {host}")),
                        None => SharedString::from("Couldn't fork this chat"),
                    },
                    error,
                ))
                .child(
                    popover::btn_ghost(&theme, "Retry", format!("pending-fork-retry-{id}"))
                        .id(("pending-fork-retry", id))
                        .on_click(cx.listener(move |this, _, _, cx| this.retry_fork(id, cx))),
                )
                .into_any_element(),
        };
        crate::motion::fade_in(
            ("pending-fork", id),
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(Theme::SPACE_LG))
                .child(body),
        )
        .into_any_element()
    }
}
