//! Settings → Chat: the system prompt of the desk's chat
//! (`zeron_proto::desk_prompt`). The agent reads it before every message, so
//! a saved edit applies from the next one.

use std::path::PathBuf;

use gpui::{
    App, Context, Entity, IntoElement, Render, SharedString, Subscription, Window, div,
    prelude::*, px,
};
use zeron_proto::desk_prompt;

use crate::composer::{ComposerInput, ComposerInputEvent, TEXT_EDITOR_CONTEXT};
use crate::settings::search::{SearchAnchor, SettingId};
use crate::settings::widgets::{self, ActionTone};
use crate::theme::Theme;

/// The editor's visible height before it scrolls.
const EDITOR_HEIGHT: f32 = 460.0;

pub struct ChatPromptPage {
    data_dir: PathBuf,
    input: Entity<ComposerInput>,
    /// The text last saved (or loaded): Save is offered only for changes.
    saved: String,
    status: Option<SharedString>,
    _input_events: Subscription,
}

impl ChatPromptPage {
    pub fn new(data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let saved = desk_prompt::current(&data_dir);
        let input = cx.new(|cx| {
            let mut input = ComposerInput::with_context(
                "Instrucciones del chat de la mesa",
                TEXT_EDITOR_CONTEXT,
                cx,
            )
            .with_viewport_height(EDITOR_HEIGHT);
            input.set_text(saved.clone(), cx);
            input.move_to_start(cx);
            input
        });
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.status = None;
                cx.notify();
            }
        });
        Self {
            data_dir,
            input,
            saved,
            status: None,
            _input_events: events,
        }
    }

    fn text(&self, cx: &App) -> String {
        self.input.read(cx).text().to_string()
    }

    fn dirty(&self, cx: &App) -> bool {
        self.text(cx) != self.saved
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        self.status = Some(match desk_prompt::save(&self.data_dir, &text) {
            Ok(()) => {
                self.saved = desk_prompt::current(&self.data_dir);
                if self.saved != text {
                    let restored = self.saved.clone();
                    self.input.update(cx, |input, cx| {
                        input.set_text(restored, cx);
                        input.move_to_start(cx);
                    });
                }
                "Guardado. Se aplica desde el siguiente mensaje.".into()
            }
            Err(error) => format!("No se pudo guardar: {error}").into(),
        });
        cx.notify();
    }

    fn restore_default(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_text(desk_prompt::DEFAULT, cx);
            input.move_to_start(cx);
        });
        self.status = None;
        cx.notify();
    }
}

impl Render for ChatPromptPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let dirty = self.dirty(cx);
        let is_default = self.text(cx).trim() == desk_prompt::DEFAULT.trim();
        let save = widgets::text_action(
            &theme,
            if dirty { ActionTone::Solid } else { ActionTone::Filled },
            "Guardar",
        )
        .id("chat-prompt-save")
        .role(gpui::Role::Button)
        .when(!dirty, |button| button.opacity(0.5).cursor_default())
        .on_click(cx.listener(|this, _, _, cx| {
            if this.dirty(cx) {
                this.save(cx);
            }
        }));
        let restore = widgets::text_action(&theme, ActionTone::Quiet, "Restaurar por defecto")
            .id("chat-prompt-default")
            .role(gpui::Role::Button)
            .when(is_default, |button| button.opacity(0.5).cursor_default())
            .on_click(cx.listener(|this, _, _, cx| this.restore_default(cx)));
        let prompt_row = widgets::card_row(&theme, true)
            .flex_col()
            .items_stretch()
            .gap(px(10.0))
            .child(widgets::row_title(&theme, "System prompt"))
            .child(widgets::meta_line(
                &theme,
                vec![
                    "Lo que lee el agente del chat antes de cada mensaje. Úsalo para \
                     centrarlo en el panel de trading y en cómo quieres las respuestas."
                        .into_any_element(),
                ],
            ))
            .child(
                div()
                    .id("chat-prompt-editor")
                    .w_full()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg)
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.5))
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(save)
                    .child(restore)
                    .child(div().flex_1())
                    .children(self.status.clone().map(|status| {
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(status)
                    })),
            )
            .search_anchor(SettingId::SystemPrompt);
        div().id("chat-prompt-page").size_full().overflow_y_scroll().child(
            widgets::page_column()
                .child(widgets::page_header(&theme, "Chat", None))
                .child(widgets::section_card(&theme).child(prompt_row)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn edits_save_to_the_data_dir_and_default_restores(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), &data, cx);
        });
        let page = cx.add_window(|_, cx| ChatPromptPage::new(data.clone(), cx));
        page.update(cx, |page, _, cx| {
            assert_eq!(page.text(cx), desk_prompt::DEFAULT);
            assert!(!page.dirty(cx));
            page.input
                .update(cx, |input, cx| input.set_text("Responde solo con cifras.", cx));
            assert!(page.dirty(cx));
            page.save(cx);
            assert!(!page.dirty(cx));
        })
        .unwrap();
        assert_eq!(desk_prompt::current(&data), "Responde solo con cifras.");
        page.update(cx, |page, _, cx| {
            page.restore_default(cx);
            page.save(cx);
        })
        .unwrap();
        assert_eq!(desk_prompt::current(&data), desk_prompt::DEFAULT);
        assert!(!desk_prompt::path(&data).exists());
    }
}
