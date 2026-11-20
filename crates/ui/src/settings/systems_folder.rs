//! Settings → Sistemas: where the desk's trading systems (`NN_name` folders)
//! come from — a folder on this computer, or one folder of a git repository
//! that Aurea keeps a sparse copy of and updates at every start. The engine
//! remembers the choice (`TradingPoolSelect` / `TradingRepoSet`).

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*,
    px,
};
use serde_json::json;
use zeron_proto::trading::{PoolSelectResult, RepoSource, RepoState, RepoSyncReport};
use zeron_rpc::methods;

use crate::composer::ComposerInput;
use crate::settings::search::{SearchAnchor, SettingId};
use crate::settings::widgets::{self, ActionTone};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// What the engine says while bots run (`engine::rpc::SYSTEMS_RUNNING`).
const RUNNING_MARK: &str = "Hay sistemas en marcha";
/// The changed files shown after an update.
const CHANGED_SHOWN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    Repo,
}

pub struct SystemsFolderPage {
    engine: Option<EngineHandle>,
    origin: Origin,
    /// The folder in use, as the engine last answered (`None`: none chosen).
    current: Option<PoolSelectResult>,
    repo: RepoState,
    url: Entity<ComposerInput>,
    branch: Entity<ComposerInput>,
    subdir: Entity<ComposerInput>,
    /// The engine hasn't answered yet.
    loading: bool,
    /// A pick, download or update is under way.
    busy: bool,
    /// The last update was refused because bots run: offer to insist.
    offer_force: bool,
    status: Option<SharedString>,
    _fields: Vec<gpui::Subscription>,
}

fn field(placeholder: &'static str, cx: &mut Context<SystemsFolderPage>) -> Entity<ComposerInput> {
    cx.new(|cx| ComposerInput::with_context(placeholder, "PaletteSearch", cx).with_single_line())
}

impl SystemsFolderPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let engine = state.read(cx).engine().cloned();
        let url = field("https://github.com/usuario/repositorio", cx);
        let branch = field("Rama (opcional; la principal si se deja vacía)", cx);
        let subdir = field("Carpeta dentro del repo, p. ej. bots/sistemas", cx);
        // Typing switches the action between "Actualizar" and "Descargar y usar".
        let fields = [&url, &branch, &subdir]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event, cx| {
                    if matches!(event, crate::composer::ComposerInputEvent::Edited) {
                        this.offer_force = false;
                        cx.notify();
                    }
                })
            })
            .collect();
        let mut page = Self {
            loading: engine.is_some(),
            engine,
            origin: Origin::Local,
            current: None,
            repo: RepoState::default(),
            url,
            branch,
            subdir,
            busy: false,
            offer_force: false,
            status: None,
            _fields: fields,
        };
        page.refresh(cx);
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else { return };
        cx.spawn(async move |this, cx| {
            let current = engine.client().call(methods::TRADING_POOL_CURRENT, json!({})).await;
            let repo = engine.client().call(methods::TRADING_REPO_STATE, json!({})).await;
            let _ = this.update(cx, |page, cx| {
                page.loading = false;
                match current {
                    Ok(value) => page.current = serde_json::from_value(value).ok().flatten(),
                    Err(error) => page.status = Some(format!("No se pudo leer: {error}").into()),
                }
                if let Ok(value) = repo {
                    page.repo = serde_json::from_value(value).unwrap_or_default();
                }
                if let Some(source) = page.repo.source.clone() {
                    page.origin = Origin::Repo;
                    page.fill(&source, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fill(&self, source: &RepoSource, cx: &mut App) {
        let set = |input: &Entity<ComposerInput>, text: String, cx: &mut App| {
            input.update(cx, |input, cx| input.set_text(text, cx));
        };
        set(&self.url, source.url.clone(), cx);
        set(&self.branch, source.branch.clone().unwrap_or_default(), cx);
        set(&self.subdir, source.subdir.clone(), cx);
    }

    /// The repository fields as typed.
    fn typed(&self, cx: &App) -> RepoSource {
        let text = |input: &Entity<ComposerInput>| input.read(cx).text().trim().to_string();
        let branch = text(&self.branch);
        RepoSource {
            url: text(&self.url),
            branch: (!branch.is_empty()).then_some(branch),
            subdir: text(&self.subdir),
        }
    }

    /// The fields differ from the source in use: the action downloads anew.
    fn edited(&self, cx: &App) -> bool {
        let typed = self.typed(cx);
        match &self.repo.source {
            Some(saved) => {
                typed.url.trim_end_matches('/') != saved.url
                    || typed.branch != saved.branch
                    || typed.subdir.trim_matches('/').replace('\\', "/") != saved.subdir
            }
            None => true,
        }
    }

    fn set_origin(&mut self, origin: Origin, cx: &mut Context<Self>) {
        if self.origin != origin {
            self.origin = origin;
            self.status = None;
            self.offer_force = false;
            cx.notify();
        }
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
                Ok(Err(error)) => {
                    let _ = this.update(cx, |page, cx| {
                        page.busy = false;
                        page.status = Some(format!("No se pudo abrir el selector: {error}").into());
                        cx.notify();
                    });
                    return;
                }
                // Cancelled.
                _ => {
                    let _ = this.update(cx, |page, cx| {
                        page.busy = false;
                        cx.notify();
                    });
                    return;
                }
            };
            let answer = engine
                .client()
                .call(methods::TRADING_POOL_SELECT, json!({ "dir": dir.to_string_lossy() }))
                .await;
            let _ = this.update(cx, |page, cx| {
                page.busy = false;
                page.status = Some(match answer {
                    Ok(value) => match serde_json::from_value::<PoolSelectResult>(value) {
                        Ok(result) => {
                            let message = format!("Carpeta guardada: {}.", count(result.systems));
                            page.current = Some(result);
                            // A local folder replaces the repository.
                            page.repo = RepoState::default();
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

    /// Download the typed repository folder (new or changed source) or update
    /// the one in use; `force` updates even with bots running.
    fn sync(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            self.status = Some("El motor no está disponible.".into());
            cx.notify();
            return;
        };
        let fresh = self.edited(cx);
        let typed = self.typed(cx);
        if fresh && typed.url.is_empty() {
            self.status = Some("Escribe la URL del repositorio.".into());
            cx.notify();
            return;
        }
        let (method, params) = if fresh {
            (methods::TRADING_REPO_SET, serde_json::to_value(&typed).unwrap_or_default())
        } else {
            (methods::TRADING_REPO_UPDATE, json!({ "force": force }))
        };
        self.busy = true;
        self.offer_force = false;
        self.status = Some(if fresh { "Descargando…" } else { "Actualizando…" }.into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let answer = engine.client().call(method, params).await;
            let _ = this.update(cx, |page, cx| {
                page.busy = false;
                match answer.map(serde_json::from_value::<RepoSyncReport>) {
                    Ok(Ok(report)) => {
                        page.status = Some(
                            if fresh {
                                format!("Descargado: {}.", count(report.systems))
                            } else if report.changed.is_empty() {
                                "Ya estaba al día.".to_string()
                            } else {
                                format!("Actualizado: {} ficheros cambiados.", report.changed.len())
                            }
                            .into(),
                        );
                        page.current = Some(PoolSelectResult {
                            dir: report.dir.clone(),
                            systems: report.systems,
                        });
                        page.repo = RepoState { source: Some(typed_saved(&typed)), last: Some(report) };
                    }
                    Ok(Err(_)) => page.status = Some("Hecho.".into()),
                    Err(error) => {
                        let error = error.to_string();
                        page.offer_force = error.contains(RUNNING_MARK);
                        page.status = Some(error.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn folder_box(&self, theme: &Theme) -> AnyElement {
        let (path, detail): (SharedString, SharedString) = match (&self.current, self.loading) {
            (_, true) => ("Leyendo…".into(), "".into()),
            (Some(current), _) => (current.dir.clone().into(), format!("{} detectados", count(current.systems)).into()),
            (None, _) => (
                "Ninguna carpeta elegida".into(),
                "La mesa muestra sistemas de ejemplo (modo demo).".into(),
            ),
        };
        info_box(theme, "systems-folder-current", path, detail)
    }

    fn repo_box(&self, theme: &Theme) -> Option<AnyElement> {
        let last = self.repo.last.as_ref()?;
        let when = chrono::DateTime::from_timestamp_millis(last.at)
            .map(|at| at.with_timezone(&chrono::Local).format("%d/%m %H:%M").to_string())
            .unwrap_or_default();
        let mut detail = format!("{} · commit {} · actualizado {when}", count(last.systems), last.commit);
        if !last.changed.is_empty() {
            detail.push_str("\nCambios: ");
            let shown: Vec<&str> = last.changed.iter().take(CHANGED_SHOWN).map(String::as_str).collect();
            detail.push_str(&shown.join(", "));
            if last.changed.len() > CHANGED_SHOWN {
                detail.push_str(&format!(" y {} más", last.changed.len() - CHANGED_SHOWN));
            }
        }
        Some(info_box(theme, "systems-repo-last", last.dir.clone().into(), detail.into()))
    }
}

fn typed_saved(typed: &RepoSource) -> RepoSource {
    RepoSource {
        url: typed.url.trim_end_matches('/').to_string(),
        branch: typed.branch.clone(),
        subdir: typed.subdir.trim_matches('/').replace('\\', "/"),
    }
}

fn count(systems: u32) -> String {
    format!("{systems} {}", if systems == 1 { "sistema" } else { "sistemas" })
}

fn info_box(theme: &Theme, id: &'static str, title: SharedString, detail: SharedString) -> AnyElement {
    div()
        .id(id)
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
        .child(div().text_size(crate::typography::ui_rems(12.5)).text_color(theme.text).child(title))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(detail),
        )
        .into_any_element()
}

fn labeled_input(theme: &Theme, label: &'static str, input: &Entity<ComposerInput>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .w_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg)
                .px(px(10.0))
                .py(px(6.0))
                .text_size(crate::typography::ui_rems(12.5))
                .child(input.clone()),
        )
        .into_any_element()
}

impl Render for SystemsFolderPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let usable = self.engine.is_some() && !self.busy;
        let origin_button = |id: &'static str, label: &'static str, origin: Origin, current: Origin| {
            widgets::text_action(
                &theme,
                if origin == current { ActionTone::Solid } else { ActionTone::Quiet },
                label,
            )
            .id(id)
            .role(gpui::Role::Button)
        };
        let origin = div()
            .flex()
            .flex_row()
            .gap(px(6.0))
            .child(
                origin_button("systems-origin-local", "Carpeta local", Origin::Local, self.origin)
                    .on_click(cx.listener(|this, _, _, cx| this.set_origin(Origin::Local, cx))),
            )
            .child(
                origin_button("systems-origin-repo", "Repositorio de GitHub", Origin::Repo, self.origin)
                    .on_click(cx.listener(|this, _, _, cx| this.set_origin(Origin::Repo, cx))),
            );
        let status = self.status.clone().map(|status| {
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(status)
        });

        let mut row = widgets::card_row(&theme, true)
            .flex_col()
            .items_stretch()
            .gap(px(10.0))
            .child(widgets::row_title(&theme, "Carpeta de sistemas"))
            .child(widgets::meta_line(
                &theme,
                vec![
                    "Dónde están tus sistemas: cada uno en una subcarpeta con nombre \
                     NN_nombre (por ejemplo 01_eurusd_mt5). Aurea lo recuerda al reiniciar."
                        .into_any_element(),
                ],
            ))
            .child(origin);

        match self.origin {
            Origin::Local => {
                let choose = widgets::text_action(
                    &theme,
                    ActionTone::Filled,
                    if self.busy { "Eligiendo…" } else { "Elegir carpeta…" },
                )
                .id("systems-folder-choose")
                .role(gpui::Role::Button)
                .when(!usable, |button| button.opacity(0.5).cursor_default())
                .on_click(cx.listener(|this, _, _, cx| this.pick(cx)));
                row = row.child(self.folder_box(&theme)).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(choose)
                        .child(div().flex_1())
                        .children(status),
                );
            }
            Origin::Repo => {
                let fresh = self.edited(cx);
                let action = widgets::text_action(
                    &theme,
                    ActionTone::Solid,
                    match (self.busy, fresh) {
                        (true, _) => "Trabajando…",
                        (false, true) => "Descargar y usar",
                        (false, false) => "Actualizar",
                    },
                )
                .id("systems-repo-sync")
                .role(gpui::Role::Button)
                .when(!usable, |button| button.opacity(0.5).cursor_default())
                .on_click(cx.listener(|this, _, _, cx| this.sync(false, cx)));
                let force = self.offer_force.then(|| {
                    widgets::text_action(&theme, ActionTone::Danger, "Actualizar igualmente")
                        .id("systems-repo-force")
                        .role(gpui::Role::Button)
                        .on_click(cx.listener(|this, _, _, cx| this.sync(true, cx)))
                });
                row = row
                    .child(widgets::meta_line(
                        &theme,
                        vec![
                            "Aurea descarga solo esa carpeta del repositorio y la actualiza \
                             cada vez que arranca, si no hay sistemas en marcha. Solo lee: \
                             nunca sube cambios. Para repos privados usa tus credenciales de git."
                                .into_any_element(),
                        ],
                    ))
                    .child(labeled_input(&theme, "Repositorio", &self.url))
                    .child(labeled_input(&theme, "Rama", &self.branch))
                    .child(labeled_input(&theme, "Carpeta dentro del repo", &self.subdir))
                    .children(self.repo_box(&theme))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(action)
                            .children(force)
                            .child(div().flex_1())
                            .children(status),
                    );
            }
        }

        div().id("systems-folder-page").size_full().overflow_y_scroll().child(
            widgets::page_column()
                .child(widgets::page_header(&theme, "Sistemas", None))
                .child(widgets::section_card(&theme).child(row.search_anchor(SettingId::SystemsFolder))),
        )
    }
}
