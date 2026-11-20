//! Settings → Sistemas: the folder whose `NN_name` subfolders are the desk's
//! trading systems. The engine remembers the choice (`TradingPoolSelect`)
//! and reads it again at every start.

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};
use serde_json::json;
use zeron_proto::trading::PoolSelectResult;
use zeron_rpc::methods;

use crate::settings::search::{SearchAnchor, SettingId};
use crate::settings::widgets::{self, ActionTone};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

pub struct SystemsFolderPage {
    engine: Option<EngineHandle>,
    /// The folder in use, as the engine last answered (`None`: none chosen).
    current: Option<PoolSelectResult>,
    /// The engine hasn't answered yet.
    loading: bool,
    /// A folder is being picked or applied.
    busy: bool,
    status: Option<SharedString>,
}

impl SystemsFolderPage {
    pub fn new(state: gpui::Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let engine = state.read(cx).engine().cloned();
        let mut page = Self {
            loading: engine.is_some(),
            engine,
            current: None,
            busy: false,
            status: None,
        };
        page.refresh(cx);
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else { return };
        cx.spawn(async move |this, cx| {
            let answer = engine.client().call(methods::TRADING_POOL_CURRENT, json!({})).await;
            let _ = this.update(cx, |page, cx| {
                page.loading = false;
                match answer {
                    Ok(value) => page.current = serde_json::from_value(value).ok().flatten(),
                    Err(error) => page.status = Some(format!("No se pudo leer: {error}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn pick(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.status = Some("El motor no está disponible.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = None;
        cx.notify();
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Elegir carpeta".into()),
        });
        cx.spawn(async move |this, cx| {
            let dir = match picked.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                // Cancelled.
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |page, cx| {
                        page.busy = false;
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(error)) => {
                    let _ = this.update(cx, |page, cx| {
                        page.busy = false;
                        page.status = Some(format!("No se pudo abrir el selector: {error}").into());
                        cx.notify();
                    });
                    return;
                }
                Err(_) => return,
            };
            let answer = engine
                .client()
                .call(
                    methods::TRADING_POOL_SELECT,
                    json!({ "dir": dir.to_string_lossy() }),
                )
                .await;
            let _ = this.update(cx, |page, cx| {
                page.busy = false;
                page.status = Some(match answer {
                    Ok(value) => match serde_json::from_value::<PoolSelectResult>(value) {
                        Ok(result) => {
                            let message = format!(
                                "Carpeta guardada: {} {}.",
                                result.systems,
                                if result.systems == 1 { "sistema" } else { "sistemas" }
                            );
                            page.current = Some(result);
                            message.into()
                        }
                        Err(_) => "Carpeta guardada.".into(),
                    },
                    // The engine says why (for instance, no `NN_name` folders).
                    Err(error) => format!("No se pudo usar esa carpeta: {error}").into(),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for SystemsFolderPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let (path, detail): (SharedString, SharedString) = match (&self.current, self.loading) {
            (_, true) => ("Leyendo…".into(), "".into()),
            (Some(current), _) => (
                current.dir.clone().into(),
                format!(
                    "{} {} detectados",
                    current.systems,
                    if current.systems == 1 { "sistema" } else { "sistemas" }
                )
                .into(),
            ),
            (None, _) => (
                "Ninguna carpeta elegida".into(),
                "La mesa muestra sistemas de ejemplo (modo demo).".into(),
            ),
        };
        let choose = widgets::text_action(
            &theme,
            ActionTone::Filled,
            if self.busy { "Eligiendo…" } else { "Elegir carpeta…" },
        )
        .id("systems-folder-choose")
        .role(gpui::Role::Button)
        .when(self.busy || self.engine.is_none(), |button| button.opacity(0.5).cursor_default())
        .on_click(cx.listener(|this, _, _, cx| this.pick(cx)));
        let row = widgets::card_row(&theme, true)
            .flex_col()
            .items_stretch()
            .gap(px(10.0))
            .child(widgets::row_title(&theme, "Carpeta de sistemas"))
            .child(widgets::meta_line(
                &theme,
                vec![
                    "La carpeta donde están tus sistemas: cada uno en una subcarpeta \
                     con nombre NN_nombre (por ejemplo 01_eurusd_mt5). Aurea la recuerda \
                     al reiniciar."
                        .into_any_element(),
                ],
            ))
            .child(
                div()
                    .id("systems-folder-current")
                    .w_full()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg)
                    .px(px(12.0))
                    .py(px(10.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text)
                            .child(path),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(detail),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(choose)
                    .child(div().flex_1())
                    .children(self.status.clone().map(|status| {
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(status)
                    })),
            )
            .search_anchor(SettingId::SystemsFolder);
        div().id("systems-folder-page").size_full().overflow_y_scroll().child(
            widgets::page_column()
                .child(widgets::page_header(&theme, "Sistemas", None))
                .child(widgets::section_card(&theme).child(row)),
        )
    }
}
