//! The trading surface: header, KPI strip, positions table, systems, tasks and
//! the engine verdict log, all fed by `WatchTradingSnapshot`.
//!
//! Money-touching actions (close one, close all, panic) never fire straight
//! from a click: they stage a [`Confirm`] bar that names the consequence first.

use gpui::{
    AnyElement, App, Context, Div, Entity, Hsla, IntoElement, Render, SharedString, Stateful, Task,
    Window, div, prelude::*, px,
};
use serde_json::json;
use zeron_proto::trading::{
    Broker, IbkrPort, Link, LogLevel, Mt5Detected, PanicResult, Position, PositionCloseResult,
    PositionSide, ProposalAction, ProposalStatus, Rule, RuleStatus, Snapshot, System,
    Task as TradingTask, TaskKind, TaskStatus, TradingProposal,
};
use zeron_rpc::methods;

use super::i18n;
use super::model::{self, Confirm, Phase};
use crate::popover::{self, Popup};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 30.0;
/// The desk is two columns: the systems list and the positions table on the
/// wide side, the readings (equity, log) and the plumbing (links, tasks, rules)
/// on the narrow one. The wide side grows faster, and when the two bases no
/// longer fit the narrow one wraps under and takes the full width.
const SYSTEMS_COLUMN_BASIS: f32 = 600.0;
const READINGS_COLUMN_BASIS: f32 = 360.0;
const COLUMN_GAP: f32 = 10.0;
const PAGE_PAD: f32 = 16.0;
/// One row of the systems list: state, id, name, symbol · timeframe, P&L and
/// the start/stop control, all on a single line.
const SYSTEM_ROW_HEIGHT: f32 = 24.0;

/// Which titlebar popover is open. Both are plumbing the desk only needs on
/// demand, so they ride the bar instead of holding a card on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeskMenu {
    Links,
    Tasks,
    /// Right-click on the titlebar's buttons: left / centre / right.
    Align,
}

/// A titlebar button being dragged to a new place (its settings name).
#[derive(Clone)]
struct TitlebarButtonDrag(&'static str);

/// What follows the pointer while a titlebar button is dragged.
struct TitlebarDragGhost(&'static str);

impl Render for TitlebarDragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let label = match self.0 {
            "links" => "Conexiones",
            "tasks" => "Tareas",
            "panic" => "Pánico",
            _ => "Ajustes",
        };
        div()
            .h(px(24.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.accent)
            .opacity(0.9)
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonTone {
    Ghost,
    Primary,
    Danger,
}

pub struct TradingSurface {
    engine: Option<EngineHandle>,
    snapshot: Option<Snapshot>,
    phase: Phase,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    confirm: Option<Confirm>,
    /// The systems list folds to its header (the count stays visible), so the
    /// rest of the desk fits the screen when the pool is not being watched.
    systems_collapsed: bool,
    /// The stopped systems hide behind one summary row unless asked for: a
    /// normal day runs most of the pool, and 15 rows of which 5 are idle cost
    /// the positions table its room.
    stopped_open: bool,
    /// Titlebar popovers: the links and the scheduled tasks. One `Popup` per
    /// trigger, so the press-note that distinguishes "dismiss" from "reopen"
    /// never mixes them up.
    links_menu: Popup<()>,
    tasks_menu: Popup<()>,
    align_menu: Popup<()>,
    /// Opens Settings (the shell hands it over: an action would only reach
    /// the shell while focus sits inside it).
    open_settings: Option<std::rc::Rc<dyn Fn(&mut Window, &mut App)>>,
    /// Left edge (window coordinates) of the titlebar's actions group, read
    /// from the last prepaint. The shell centres `Aurea` on the desk column
    /// and stops it before this edge, so the title never runs under the chips.
    /// A cell (not entity state) because prepaint cannot update the entity.
    titlebar_actions_left: std::rc::Rc<std::cell::Cell<Option<(f32, f32)>>>,
    _watch: Option<Task<()>>,
}

impl TradingSurface {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let engine = state.read(cx).engine().cloned();
        let phase = if engine.is_some() {
            Phase::Connecting
        } else {
            Phase::Failed("Este motor no expone el servicio de trading.".into())
        };
        let mut surface = Self {
            engine,
            snapshot: None,
            phase,
            error: None,
            notice: None,
            confirm: None,
            systems_collapsed: false,
            stopped_open: false,
            links_menu: Popup::default(),
            tasks_menu: Popup::default(),
            align_menu: Popup::default(),
            open_settings: None,
            titlebar_actions_left: std::rc::Rc::default(),
            _watch: None,
        };
        surface.subscribe(cx);
        surface
    }

    /// Opens the snapshot stream once; every later snapshot replaces the view.
    fn subscribe(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let watch = cx.spawn(async move |this, cx| {
            let mut stream = match engine
                .client()
                .subscribe(methods::WATCH_TRADING_SNAPSHOT, json!({}))
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    let message = format!("No se pudo abrir el stream de trading: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.phase = Phase::Failed(message);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                if this.phase != Phase::Live {
                    this.phase = Phase::Live;
                    cx.notify();
                }
            });
            let lite = crate::lite::enabled();
            while let Some(mut value) = stream.recv().await {
                // Lite redraws the desk at most once per `DESK_REFRESH`: what
                // queued meanwhile collapses into the newest snapshot.
                if lite {
                    while let Ok(newer) = stream.try_recv() {
                        value = newer;
                    }
                }
                match serde_json::from_value::<Snapshot>(value) {
                    Ok(snapshot) => {
                        let alive = this
                            .update(cx, |this, cx| {
                                // An identical desk draws nothing new: no
                                // redraw (without a GPU each one is real CPU).
                                let same = this.snapshot.as_ref() == Some(&snapshot)
                                    && this.phase == Phase::Live
                                    && this.error.is_none();
                                if same {
                                    return;
                                }
                                this.snapshot = Some(snapshot);
                                this.phase = Phase::Live;
                                this.error = None;
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            this.error = Some(format!("Snapshot inválido: {error}").into());
                            cx.notify();
                        });
                    }
                }
                if lite {
                    cx.background_executor().timer(crate::lite::DESK_REFRESH).await;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.phase = Phase::Failed("El motor cerró el stream de trading.".into());
                cx.notify();
            });
        });
        self._watch = Some(watch);
    }

    /// Fires a unary engine call without blocking the UI; failures land in the
    /// error strip instead of a dialog.
    fn call(&mut self, method: &'static str, params: serde_json::Value, cx: &mut Context<Self>) {
        self.call_with(method, params, cx, |_, _, _| {});
    }

    /// Like [`Self::call`], with the reply handed to `on_ok` so a screen can
    /// chain a second request (detect → register, for instance).
    fn call_with(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
        on_ok: impl FnOnce(&mut Self, serde_json::Value, &mut Context<Self>) + 'static,
    ) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        self.error = None;
        cx.spawn(async move |this, cx| {
            match engine.client().call(method, params).await {
                Ok(value) => {
                    let _ = this.update(cx, |this, cx| {
                        on_ok(this, value, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let message = format!("{error}");
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(message.into());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn ask(&mut self, confirm: Confirm, cx: &mut Context<Self>) {
        self.confirm = Some(confirm);
        cx.notify();
    }

    /// A close verdict, in Spanish: a success reads as a notice, a refusal
    /// or a failure lands in the error strip.
    fn show_close_verdict(&mut self, value: serde_json::Value, _cx: &mut Context<Self>) {
        match serde_json::from_value::<PositionCloseResult>(value) {
            Ok(result) => {
                let line = i18n::verdict_text(&result);
                if result.ok {
                    self.notice = Some(line.into());
                } else {
                    self.error = Some(line.into());
                }
            }
            Err(error) => self.error = Some(format!("Respuesta de cierre inválida: {error}").into()),
        }
    }

    fn confirm_current(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match confirm {
            Confirm::Close(symbol) => {
                let request_id = format!("ui:{}", uuid::Uuid::new_v4());
                self.call_with(
                    methods::TRADING_POSITION_CLOSE,
                    json!({ "symbol": symbol, "requestId": request_id }),
                    cx,
                    Self::show_close_verdict,
                );
            }
            Confirm::Panic => self.call_with(
                methods::TRADING_PANIC,
                json!({}),
                cx,
                |this, value, _cx| match serde_json::from_value::<PanicResult>(value) {
                    Ok(result) => {
                        let line = i18n::panic_text(&result);
                        if result.close.ok {
                            this.notice = Some(line.into());
                        } else {
                            this.error = Some(line.into());
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Respuesta de pánico inválida: {error}").into())
                    }
                },
            ),
            Confirm::Stop(system) => self.call(
                methods::TRADING_SYSTEM_STOP,
                json!({ "systemId": system }),
                cx,
            ),
        }
        cx.notify();
    }

    // ----------------------------------------------------------------- header

    /// The desk's heading, drawn inside the unified titlebar (user request:
    /// the desk's own 52px strip moved up there, so the page keeps that height
    /// for content). Single line and no chrome of its own — the titlebar
    /// supplies the height, the left inset (aligned with [`PAGE_PAD`]) and the
    /// drag region; the shell observes this entity so a snapshot redraws it.
    pub(crate) fn titlebar_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let demo = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| model::kpis(snapshot).demo);
        // The shell centres `Aurea` on the desk column and needs to stop it
        // before this group; the measured edge lands in the cell on the next
        // prepaint (the entity itself cannot be touched from here).
        let actions_probe = self.titlebar_actions_left.clone();
        let align = crate::settings::titlebar_align(cx);
        let order: Vec<&'static str> = crate::settings::titlebar_order(cx)
            .iter()
            .filter_map(|name| match name.as_str() {
                "links" => Some("links"),
                "tasks" => Some("tasks"),
                "panic" => Some("panic"),
                "settings" => Some("settings"),
                _ => None,
            })
            .collect();
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .overflow_hidden()
            // The engine's state as one dot, where the phase chip used to sit
            // (user request: no "EN VIVO"/"DEMO" text). `Aurea` is centred by
            // the shell, not by this row.
            .child(self.phase_dot(theme, demo))
            .when(align != crate::settings::TitlebarAlign::Left, |row| row.child(div().flex_1()))
            // The buttons, in the order the person dragged them into
            // (Settings among them); right-click picks where they sit.
            .child({
                let group = div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_menu(DeskMenu::Align, cx);
                            cx.notify();
                        }),
                    )
                    .on_children_prepainted(move |bounds, window, _| {
                        let (Some(first), Some(last)) = (bounds.first(), bounds.last()) else {
                            return;
                        };
                        let edges = (
                            first.origin.x.as_f32(),
                            (last.origin.x + last.size.width).as_f32(),
                        );
                        let changed = actions_probe.get().map_or(true, |old| {
                            (old.0 - edges.0).abs() > 0.5 || (old.1 - edges.1).abs() > 0.5
                        });
                        actions_probe.set(Some(edges));
                        if changed {
                            // The shell reads the edges on its next render; ask
                            // for one so the title moves without waiting on a
                            // snapshot.
                            window.request_animation_frame();
                        }
                    })
                    .children(
                        order
                            .iter()
                            .map(|name| self.movable_button(name, theme, cx)),
                    )
                    .id("titlebar-buttons");
                self.menu_anchor(group, DeskMenu::Align, theme, cx)
            })
            .when(align == crate::settings::TitlebarAlign::Center, |row| row.child(div().flex_1()))
            .when(align == crate::settings::TitlebarAlign::Left, |row| row.child(div().flex_1()))
    }

    /// The measured left edge of the titlebar's actions group, in window
    /// coordinates. `None` until the first prepaint.
    pub(crate) fn titlebar_actions_left(&self) -> Option<(f32, f32)> {
        self.titlebar_actions_left.get()
    }

    /// Icon-only panic trigger: the label lives in the tooltip and in the
    /// confirmation bar, so the titlebar keeps room for the centred brand.
    fn panic_button(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("trading-panic")
            .h(px(24.0))
            .w(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(theme.danger.opacity(0.12))
            .cursor_pointer()
            .hover({
                let hover = theme.danger.opacity(0.22);
                move |style| style.bg(hover)
            })
            // Inside the titlebar's drag strip: carve the button out of it, or
            // the press becomes a window move and the click never reaches the
            // handler (the chips already do this).
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .on_click(cx.listener(|this, _, _, cx| this.ask(Confirm::Panic, cx)))
            .on_click(|_, _, cx| cx.stop_propagation())
            .tooltip(crate::settings::widgets::text_tooltip(
                "Pánico: parar todos los bots y cerrar todo",
            ))
            .child(
                crate::icons::icon(crate::icons::STOP)
                    .size(px(14.0))
                    .text_color(theme.danger),
            )
            .into_any_element()
    }

    /// `● TWS 7497` — the link's health in the titlebar. The card that used to
    /// hold the rows and the four buttons opens below it on click.
    fn links_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let links = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.links.clone())
            .unwrap_or_default();
        let live = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.accounts.is_some());
        let connected = links.iter().filter(|link| link.connected).count();
        let _ = live;
        let (tone, label) = if links.is_empty() {
            (theme.text_faint, "sin conexión".to_string())
        } else if connected == 0 {
            (theme.text_faint, "desconectado".to_string())
        } else {
            // Name the links that work; a registered one that is down (an
            // idle TWS next to a working MT5) is in the menu, not the chip.
            let mut names: Vec<String> = Vec::new();
            for link in links.iter().filter(|link| link.connected) {
                let name = match (link.broker, link.port) {
                    (Some(Broker::Mt5), _) => "MT5".to_string(),
                    (Some(Broker::Nt), _) => "NT".to_string(),
                    (_, Some(port)) => format!("TWS {port}"),
                    _ => "IBKR".to_string(),
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            let label = if names.len() > 2 {
                format!("{connected} enlaces")
            } else {
                names.join(" · ")
            };
            (theme.success, label)
        };
        let chip = self
            .desk_chip("trading-links-chip", DeskMenu::Links, theme, cx)
            .child(div().size(px(5.0)).rounded(px(2.5)).bg(tone))
            .child(SharedString::from(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(10.0))
                    .text_color(theme.text_faint),
            );
        self.menu_anchor(chip, DeskMenu::Links, theme, cx)
    }

    /// `⏱` — the scheduled tasks, icon-only in the titlebar (user request):
    /// the clock carries the tone and the tooltip carries the next time.
    /// Faint when the desk has nothing queued.
    fn tasks_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let pending = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                model::tasks_sorted(snapshot)
                    .into_iter()
                    .filter(|task| task.status == TaskStatus::Pending)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let (tone, label) = match pending.as_slice() {
            [] => (theme.text_faint, "Sin tareas programadas".to_string()),
            [task] => (
                theme.accent,
                format!("Tarea a las {}", model::short_clock(task.run_at)),
            ),
            [task, ..] => (
                theme.accent,
                format!(
                    "{} tareas · la próxima a las {}",
                    pending.len(),
                    model::short_clock(task.run_at)
                ),
            ),
        };
        let chip = self
            .desk_icon_chip("trading-tasks-chip", DeskMenu::Tasks, theme, cx)
            .tooltip(crate::settings::widgets::text_tooltip(label))
            .child(
                crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                    .size(px(14.0))
                    .text_color(tone),
            );
        self.menu_anchor(chip, DeskMenu::Tasks, theme, cx)
    }

    /// The shared trigger chrome for a titlebar chip: small, occluding (the
    /// bar is a drag strip, so a press must not become a window move) and
    /// toggling through the popup's press-note.
    fn desk_chip(
        &self,
        id: &'static str,
        menu: DeskMenu,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(22.0))
            .flex_none()
            .px(px(7.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded(px(6.0))
            .bg(crate::theme::ink(0.06))
            .text_size(px(10.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::ink(0.12)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, _| {
                    window.prevent_default();
                    match menu {
                        DeskMenu::Links => this.links_menu.note_trigger_press(),
                        DeskMenu::Tasks => this.tasks_menu.note_trigger_press(),
                        DeskMenu::Align => this.align_menu.note_trigger_press(),
                    }
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_menu(menu, cx);
                cx.notify();
            }))
    }

    /// [`desk_chip`]'s square sibling for an icon-only trigger: same chrome and
    /// the same press-note plumbing, no label.
    fn desk_icon_chip(
        &self,
        id: &'static str,
        menu: DeskMenu,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(24.0))
            .w(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(crate::theme::ink(0.06))
            .text_size(px(10.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::ink(0.12)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, _| {
                    window.prevent_default();
                    match menu {
                        DeskMenu::Links => this.links_menu.note_trigger_press(),
                        DeskMenu::Tasks => this.tasks_menu.note_trigger_press(),
                        DeskMenu::Align => this.align_menu.note_trigger_press(),
                    }
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_menu(menu, cx);
                cx.notify();
            }))
    }

    /// Mounts the open popover under its chip.
    fn menu_anchor(
        &self,
        chip: Stateful<Div>,
        menu: DeskMenu,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (open, closing) = match menu {
            DeskMenu::Links => (self.links_menu.get().is_some(), self.links_menu.closing_since()),
            DeskMenu::Tasks => (self.tasks_menu.get().is_some(), self.tasks_menu.closing_since()),
            DeskMenu::Align => (self.align_menu.get().is_some(), self.align_menu.closing_since()),
        };
        if !open {
            return chip.into_any_element();
        }
        let content = match menu {
            DeskMenu::Links => self.render_links_menu(theme, cx),
            DeskMenu::Tasks => self.render_tasks_menu(theme, cx),
            DeskMenu::Align => self.render_align_menu(theme, cx),
        };
        chip.relative()
            .child(popover::anchored_menu_below_gap(
                match menu {
                    DeskMenu::Links => "trading-links-menu",
                    DeskMenu::Tasks => "trading-tasks-menu",
                    DeskMenu::Align => "trading-align-menu",
                },
                content,
                closing,
                8.0,
            ))
            .into_any_element()
    }

    fn menu_is_open(&self, menu: DeskMenu) -> bool {
        match menu {
            DeskMenu::Links => self.links_menu.is_open(),
            DeskMenu::Tasks => self.tasks_menu.is_open(),
            DeskMenu::Align => self.align_menu.is_open(),
        }
    }

    fn toggle_menu(&mut self, menu: DeskMenu, cx: &mut Context<Self>) {
        let was_open = match menu {
            DeskMenu::Links => self.links_menu.take_press_was_open(),
            DeskMenu::Tasks => self.tasks_menu.take_press_was_open(),
            DeskMenu::Align => self.align_menu.take_press_was_open(),
        };
        if was_open {
            self.close_menu(menu, cx);
            return;
        }
        // One at a time: opening this one dismisses the others.
        for other in [DeskMenu::Links, DeskMenu::Tasks, DeskMenu::Align] {
            if other != menu && self.menu_is_open(other) {
                self.close_menu(other, cx);
            }
        }
        match menu {
            DeskMenu::Links => self.links_menu.open(()),
            DeskMenu::Tasks => self.tasks_menu.open(()),
            DeskMenu::Align => self.align_menu.open(()),
        }
    }

    fn close_menu(&mut self, menu: DeskMenu, cx: &mut Context<Self>) {
        match menu {
            DeskMenu::Links => {
                if self.links_menu.begin_close() {
                    popover::reap_popup(cx, |this: &mut Self| &mut this.links_menu);
                }
            }
            DeskMenu::Tasks => {
                if self.tasks_menu.begin_close() {
                    popover::reap_popup(cx, |this: &mut Self| &mut this.tasks_menu);
                }
            }
            DeskMenu::Align => {
                if self.align_menu.begin_close() {
                    popover::reap_popup(cx, |this: &mut Self| &mut this.align_menu);
                }
            }
        }
    }

    /// Right-click menu of the titlebar's buttons: where the group sits.
    fn render_align_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::settings::TitlebarAlign;
        let current = crate::settings::titlebar_align(cx);
        let row = |id: &'static str, label: &'static str, align: TitlebarAlign, cx: &mut Context<Self>| {
            popover::menu_row(theme, false, id)
                .id(id)
                .py(px(4.0))
                .text_size(px(11.5))
                .on_click(cx.listener(move |this, _, _, cx| {
                    crate::settings::set_titlebar_align(align, cx);
                    this.close_menu(DeskMenu::Align, cx);
                    cx.notify();
                }))
                .child(div().flex_1().child(label))
                .when(current == align, |el| {
                    el.child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(12.0))
                            .text_color(theme.accent),
                    )
                })
        };
        popover::popover_card(theme)
            .w(px(200.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child("Alinear botones · arrástralos para reordenar"),
            )
            .child(row("trading-align-left", "A la izquierda", TitlebarAlign::Left, cx))
            .child(row("trading-align-center", "Al centro", TitlebarAlign::Center, cx))
            .child(row("trading-align-right", "A la derecha", TitlebarAlign::Right, cx))
            .into_any_element()
    }

    /// The shell's way to open Settings, for the titlebar's Settings button.
    pub fn set_open_settings(&mut self, open: std::rc::Rc<dyn Fn(&mut Window, &mut App)>) {
        self.open_settings = Some(open);
    }

    /// Settings, now one of the titlebar's movable buttons.
    fn settings_button(&self, theme: &Theme) -> AnyElement {
        let open = self.open_settings.clone();
        div()
            .id("trading-open-settings")
            .h(px(24.0))
            .w(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(crate::theme::ink(0.06))
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::ink(0.12)))
            .tooltip(crate::settings::widgets::text_tooltip("Ajustes"))
            .on_click(move |_, window, cx| match &open {
                Some(open) => open(window, cx),
                None => window.dispatch_action(Box::new(crate::shell::OpenSettings), cx),
            })
            .child(
                crate::icons::icon(crate::icons::SETTINGS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .into_any_element()
    }

    /// One titlebar button, draggable onto another to take its place.
    fn movable_button(&self, name: &'static str, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let button = match name {
            "links" => self.links_chip(theme, cx),
            "tasks" => self.tasks_chip(theme, cx),
            "panic" => self.panic_button(theme, cx),
            _ => self.settings_button(theme),
        };
        let accent = theme.accent;
        div()
            .id(SharedString::from(format!("titlebar-slot-{name}")))
            .rounded(px(7.0))
            // Inside the titlebar's drag strip: carve the slot out of it, or a
            // press becomes a window move (the buttons inside stay clickable).
            .occlude()
            // Right-click on any button: where the group sits.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_menu(DeskMenu::Align, cx);
                    cx.notify();
                }),
            )
            .on_drag(TitlebarButtonDrag(name), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| TitlebarDragGhost(drag.0))
            })
            .drag_over::<TitlebarButtonDrag>(move |style, _, _, _| {
                style.bg(accent.opacity(0.25))
            })
            .on_drop::<TitlebarButtonDrag>(cx.listener(move |_, drag: &TitlebarButtonDrag, _, cx| {
                if drag.0 == name {
                    return;
                }
                let mut order = crate::settings::titlebar_order(cx);
                order.retain(|item| item != drag.0);
                let at = order.iter().position(|item| item == name).unwrap_or(order.len());
                order.insert(at, drag.0.to_string());
                crate::settings::set_titlebar_order(order, cx);
                cx.notify();
            }))
            .child(button)
            .into_any_element()
    }

    fn render_links_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let links = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.links.clone())
            .unwrap_or_default();
        let live = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.accounts.is_some());
        // The TWS action follows the TWS link itself, not whether any
        // account (an MT5 one, say) reads.
        let tws_up = links
            .iter()
            .any(|link| link.broker != Some(Broker::Mt5) && link.connected);
        let mut rows: Vec<Div> = Vec::with_capacity(links.len());
        if links.is_empty() {
            rows.push(empty_row(theme, "Sin conexiones registradas."));
        } else {
            for (ix, link) in links.iter().enumerate() {
                rows.push(self.link_row(ix, link, !live, theme, cx));
            }
        }
        let mut actions: Vec<Stateful<Div>> = Vec::with_capacity(4);
        actions.push(
            popover::menu_row(theme, false, "trading-menu-add-tws")
                .id("trading-menu-add-tws")
                .py(px(4.0))
                .text_size(px(11.5))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.add_tws_link(cx);
                    this.close_menu(DeskMenu::Links, cx);
                }))
                .child("Añadir TWS paper"),
        );
        actions.push(
            popover::menu_row(theme, false, "trading-menu-add-mt5")
                .id("trading-menu-add-mt5")
                .py(px(4.0))
                .text_size(px(11.5))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.add_mt5_link(cx);
                    this.close_menu(DeskMenu::Links, cx);
                }))
                .child("Añadir MT5"),
        );
        actions.push(
            popover::menu_row(theme, false, "trading-menu-toggle-link")
                .id("trading-menu-toggle-link")
                .py(px(4.0))
                .text_size(px(11.5))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_link(tws_up, cx);
                    this.close_menu(DeskMenu::Links, cx);
                }))
                .child(if tws_up {
                    "Desconectar TWS"
                } else {
                    "Conectar TWS paper"
                }),
        );
        actions.push(
            popover::menu_row(theme, false, "trading-menu-detect-ports")
                .id("trading-menu-detect-ports")
                .py(px(4.0))
                .text_size(px(11.5))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.detect_ports(cx);
                    this.close_menu(DeskMenu::Links, cx);
                }))
                .child("Buscar puertos"),
        );
        popover::popover_card(theme)
            .w(px(320.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(DeskMenu::Links, cx)))
            .child(
                div()
                    .px(px(12.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Conexiones"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!("{} registradas", links.len()))),
                    ),
            )
            .children(rows)
            .child(div().h(px(1.0)).w_full().bg(theme.border))
            .child(div().flex().flex_col().gap(px(2.0)).children(actions))
            .into_any_element()
    }

    fn render_tasks_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let tasks = self
            .snapshot
            .as_ref()
            .map(model::tasks_sorted)
            .unwrap_or_default();
        let pending = tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Pending)
            .count();
        let mut rows: Vec<Div> = Vec::with_capacity(tasks.len());
        if tasks.is_empty() {
            rows.push(empty_row(theme, "Sin tareas programadas. Pídelas en el chat."));
        } else {
            let snapshot = self.snapshot.clone();
            for (ix, task) in tasks.iter().take(6).enumerate() {
                if let Some(snapshot) = &snapshot {
                    rows.push(self.task_row(ix, task, snapshot, theme, cx));
                }
            }
        }
        popover::popover_card(theme)
            .w(px(320.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(DeskMenu::Tasks, cx)))
            .child(
                div()
                    .px(px(12.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Tareas"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(if pending == 0 {
                                "nada pendiente".to_string()
                            } else {
                                format!("{pending} pendientes")
                            })),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    /// Registers the TWS paper link the desk ships with (port 7497 only).
    fn add_tws_link(&mut self, cx: &mut Context<Self>) {
        self.notice = Some("Registrando TWS paper en 127.0.0.1:7497…".into());
        self.call(
            methods::TRADING_LINK_ADD,
            json!({
                "host": "127.0.0.1",
                "port": 7497,
                "label": "TWS paper",
                "broker": "ibkr",
            }),
            cx,
        );
    }

    /// Detects an installed MetaTrader 5 and links its account.
    fn add_mt5_link(&mut self, cx: &mut Context<Self>) {
        self.notice = Some("Buscando terminales MetaTrader 5 en este equipo…".into());
        self.call_with(
            methods::TRADING_MT5_DETECT,
            json!({ "probe": false }),
            cx,
            |this, value, cx| {
                let detected = match serde_json::from_value::<Vec<Mt5Detected>>(value) {
                    Ok(detected) => detected,
                    Err(error) => {
                        this.error = Some(format!("Respuesta MT5 inválida: {error}").into());
                        return;
                    }
                };
                let Some(account) = detected.into_iter().next() else {
                    this.notice = Some("No se detectó ningún MetaTrader 5 instalado.".into());
                    return;
                };
                this.notice =
                    Some(format!("Vinculando MT5 {}@{}…", account.login, account.server).into());
                let linked = format!("MT5 {}@{} conectado.", account.login, account.server);
                this.call_with(
                    methods::TRADING_LINK_ADD,
                    json!({
                        "broker": "mt5",
                        "mt5Login": account.login,
                        "mt5Server": account.server,
                        "label": account.name,
                    }),
                    cx,
                    move |this, _, _| this.notice = Some(linked.into()),
                );
            },
        );
    }

    fn toggle_link(&mut self, live: bool, cx: &mut Context<Self>) {
        if live {
            self.notice = Some("Cerrando la conexión con TWS…".into());
            self.call(methods::TRADING_DISCONNECT, json!({}), cx);
        } else {
            self.notice = Some("Conectando con TWS paper en 127.0.0.1:7497…".into());
            self.call(methods::TRADING_CONNECT, json!({}), cx);
        }
    }

    fn detect_ports(&mut self, cx: &mut Context<Self>) {
        self.notice = Some("Sondeando los puertos de TWS…".into());
        self.call_with(
            methods::TRADING_IBKR_DETECT,
            json!({}),
            cx,
            |this, value, _cx| {
                let ports = match serde_json::from_value::<Vec<IbkrPort>>(value) {
                    Ok(ports) => ports,
                    Err(error) => {
                        this.error = Some(format!("Respuesta de puertos inválida: {error}").into());
                        return;
                    }
                };
                let summary = ports
                    .iter()
                    .map(|port| {
                        format!(
                            "{}:{} {}",
                            port.host,
                            port.port,
                            if port.open { "abierto" } else { "cerrado" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                this.notice = Some(format!("TWS → {summary}").into());
            },
        );
    }

    /// The engine's phase as one coloured dot (user request: the label was
    /// noise). Red wins over demo so a dead engine is never hidden behind the
    /// accent, and the demo fixture gets the accent instead of "live" green.
    fn phase_dot(&self, theme: &Theme, demo: bool) -> Div {
        let color = match &self.phase {
            Phase::Failed(_) => theme.danger,
            Phase::Connecting => theme.warning,
            Phase::Live if demo => theme.accent,
            Phase::Live => theme.success,
        };
        div().flex_none().size(px(7.0)).rounded(px(3.5)).bg(color)
    }

    fn confirm_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<Div> {
        let confirm = self.confirm.clone()?;
        let tone = if confirm.is_money() {
            theme.danger
        } else {
            theme.warning
        };
        let accept_tone = if confirm.is_money() {
            ButtonTone::Danger
        } else {
            ButtonTone::Primary
        };
        Some(
            div()
                .flex_none()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(18.0))
                .py(px(9.0))
                .bg(tone.opacity(0.08))
                .border_b_1()
                .border_color(tone.opacity(0.25))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(tone)
                                .child(SharedString::from(confirm.title())),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted)
                                .child(confirm.detail()),
                        ),
                )
                .child(
                    button("confirm-accept", confirm.accept_label(), accept_tone, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_current(cx))),
                )
                .child(
                    button("confirm-cancel", "Cancelar", ButtonTone::Ghost, theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm = None;
                            cx.notify();
                        })),
                ),
        )
    }

    fn error_strip(&self, theme: &Theme) -> Option<Div> {
        let error = self.error.clone()?;
        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(18.0))
                .py(px(7.0))
                .bg(theme.danger.opacity(0.08))
                .border_b_1()
                .border_color(theme.danger.opacity(0.2))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.danger)
                        .child("ERROR"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(error),
                ),
        )
    }

    fn notice_strip(&self, theme: &Theme) -> Option<Div> {
        let notice = self.notice.clone()?;
        Some(
            div()
                .flex_none()
                .w_full()
                .px(px(18.0))
                .py(px(6.0))
                .bg(theme.accent.opacity(0.06))
                .border_b_1()
                .border_color(theme.accent.opacity(0.18))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .child("AVISO"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(notice),
                ),
        )
    }

    // ------------------------------------------------------------------- body

    fn body(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            let (title, detail, color) = match &self.phase {
                Phase::Connecting => (
                    "Conectando con el motor…",
                    "Esperando el primer snapshot de trading.".to_string(),
                    theme.warning,
                ),
                Phase::Failed(error) => (
                    "La mesa no está disponible",
                    error.clone(),
                    theme.danger,
                ),
                Phase::Live => (
                    "Sin snapshot todavía",
                    "El stream está abierto pero aún no ha llegado estado.".into(),
                    theme.text_muted,
                ),
            };
            return div()
                .flex_1()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(color)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(detail)),
                        ),
                )
                .into_any_element();
        };

        let now = now_ms();
        div()
            .id("trading-surface-scroll")
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(PAGE_PAD))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(self.kpi_strip(&snapshot, theme))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .gap(px(COLUMN_GAP))
                    // The systems grid and the positions table on the wide
                    // side: all 15 bots and what is open read at a glance.
                    .child(
                        board_column(SYSTEMS_COLUMN_BASIS, 1.7)
                            .child(self.systems_card(&snapshot, theme, cx))
                            .child(self.positions_card(&snapshot, theme, cx)),
                    )
                    // The readings and the plumbing on the narrow side. The
                    // links and the tasks now ride the titlebar, so the column
                    // is just the log and the rules.
                    .child(
                        board_column(READINGS_COLUMN_BASIS, 1.15)
                            .child(self.logs_card(&snapshot, theme, now))
                            .child(self.rules_card(&snapshot, theme, cx)),
                    ),
            )
            .into_any_element()
    }

    /// The ids of the proposals still waiting on the person. The shell opens
    /// the chat drawer for a new one, so the card is never hidden behind it.
    pub fn pending_proposal_ids(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.proposals.as_deref())
            .map(|rows| {
                rows.iter()
                    .filter(|p| p.status == ProposalStatus::Pending)
                    .map(|p| p.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The chat's inbox: the agent's pending requests with Approve / Reject,
    /// plus the freshest verdict. It rides the drawer above the composer, so
    /// the request and the conversation share one column; the desk keeps no
    /// copy of it.
    pub fn proposal_inbox(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let snapshot = self.snapshot.as_ref()?;
        let rows = model::inbox_proposals(snapshot, now_ms());
        if rows.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let pending = rows
            .iter()
            .filter(|p| p.status == ProposalStatus::Pending)
            .count();
        let mut children: Vec<Div> = Vec::with_capacity(rows.len());
        for (ix, proposal) in rows.iter().enumerate() {
            let mut row = div()
                .px(px(12.0))
                .py(px(8.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .border_t_1()
                .border_color(theme.border)
                .child(chat_proposal_row(proposal, snapshot, &theme));
            match proposal.status {
                ProposalStatus::Pending => {
                    let approve = proposal.id.clone();
                    let reject = proposal.id.clone();
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(6.0))
                            .child(
                                button(
                                    ("chat-proposal-reject", ix),
                                    "Rechazar",
                                    ButtonTone::Ghost,
                                    &theme,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.resolve_proposal(reject.clone(), false, cx)
                                })),
                            )
                            .child(
                                button(
                                    ("chat-proposal-approve", ix),
                                    "Aprobar",
                                    ButtonTone::Primary,
                                    &theme,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.resolve_proposal(approve.clone(), true, cx)
                                })),
                            ),
                    );
                }
                status => {
                    let (label, tone) = match status {
                        ProposalStatus::Done => ("HECHA", theme.success),
                        ProposalStatus::Failed => ("FALLÓ", theme.danger),
                        _ => ("RECHAZADA", theme.text_faint),
                    };
                    row = row.child(div().flex().flex_row().justify_end().child(chip(label, tone)));
                }
            }
            children.push(row);
        }
        let meta = if pending == 0 {
            "respuesta".to_string()
        } else {
            format!("{pending} por aprobar")
        };
        Some(
            card(&theme)
                .border_color(if pending == 0 {
                    theme.border
                } else {
                    theme.accent.opacity(0.6)
                })
                .child(section_header(&theme, "Peticiones del agente", meta.into()))
                .children(children)
                .into_any_element(),
        )
    }

    /// Approves (runs) or rejects one proposal; the verdict lands as a notice.
    fn resolve_proposal(&mut self, id: String, approve: bool, cx: &mut Context<Self>) {
        self.notice = Some(if approve {
            "Ejecutando la petición aprobada…".into()
        } else {
            "Petición rechazada.".into()
        });
        self.call_with(
            methods::TRADING_PROPOSAL_RESOLVE,
            json!({ "id": id, "approve": approve }),
            cx,
            |this, value, _cx| {
                if let Ok(Some(proposal)) = serde_json::from_value::<Option<TradingProposal>>(value) {
                    let line = proposal.result.unwrap_or_default();
                    match proposal.status {
                        ProposalStatus::Failed => this.error = Some(line.into()),
                        _ if !line.is_empty() => this.notice = Some(line.into()),
                        _ => {}
                    }
                }
            },
        );
        cx.notify();
    }

    fn kpi_strip(&self, snapshot: &Snapshot, theme: &Theme) -> Div {
        let kpis = model::kpis(snapshot);
        let pnl_color = if kpis.pnl_day.unwrap_or(0.0) >= 0.0 {
            theme.success
        } else {
            theme.danger
        };
        let bots_label = format!("{} / {}", kpis.running, kpis.systems);
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(10.0))
            // The equity KPI carries the sparkline the separate card used to
            // hold: one number and its shape, one card.
            .child(self.equity_kpi_card(snapshot, theme))
            .child(self.kpi_card(
                theme,
                "P&L DEL DÍA",
                model::signed_money(kpis.pnl_day),
                pnl_color,
            ))
            .child(self.kpi_card(
                theme,
                "P&L %",
                model::signed_pct(kpis.pnl_pct),
                pnl_color,
            ))
            .child(self.kpi_card(
                theme,
                "EXPOSICIÓN",
                model::money(Some(kpis.exposure)),
                theme.text,
            ))
            .child(self.kpi_card(theme, "BOTS ACTIVOS", bots_label, theme.accent))
    }

    /// The equity card: the figure plus the last 15 minutes as a 16px strip.
    fn equity_kpi_card(&self, snapshot: &Snapshot, theme: &Theme) -> Div {
        let kpis = model::kpis(snapshot);
        let bars = model::spark_bars(snapshot, 64);
        // A flat series has no shape to draw: a rule reads as "sin cambios",
        // a row of minimum bars reads as a chart that failed to load.
        let flat = !bars.is_empty() && bars.iter().all(|value| *value == 0.0);
        let chart = if flat {
            div()
                .h(px(16.0))
                .w_full()
                .flex()
                .items_center()
                .child(div().h(px(1.0)).w_full().bg(theme.accent.opacity(0.35)))
        } else {
            div()
                .h(px(16.0))
                .w_full()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(1.0))
                .children(bars.into_iter().map(|value| {
                    div()
                        .flex_1()
                        .h(px(2.0 + 14.0 * value.clamp(0.0, 1.0)))
                        .rounded(px(1.0))
                        .bg(theme.accent.opacity(0.55))
                }))
        };
        div()
            .flex_1()
            .min_w(px(104.0))
            .px(px(11.0))
            .py(px(8.0))
            .rounded(px(10.0))
            .bg(theme.surface_card)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(9.0))
                    .line_height(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_faint)
                    .child("EQUITY"),
            )
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(16.0))
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .truncate()
                    .child(model::money(kpis.equity)),
            )
            .child(chart)
    }

    fn kpi_card(&self, theme: &Theme, label: &'static str, value: String, tone: Hsla) -> Div {
        div()
            .flex_1()
            .min_w(px(104.0))
            .px(px(11.0))
            .py(px(8.0))
            .rounded(px(10.0))
            .bg(theme.surface_card)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(9.0))
                    .line_height(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_faint)
                    .child(label),
            )
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(16.0))
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(tone)
                    .truncate()
                    .child(value),
            )
    }

    fn positions_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let positions = model::positions_sorted(snapshot);
        let closable = positions.len();
        // Nothing open: one line, no table head. The desk keeps the space for
        // the systems list instead of framing an empty table.
        if positions.is_empty() {
            return card(theme)
                .child(section_header(
                    theme,
                    "Posiciones",
                    SharedString::from("0 abiertas"),
                ))
                .child(empty_row(theme, "Sin posiciones abiertas."));
        }
        let mut rows: Vec<Div> = Vec::with_capacity(positions.len());
        for (ix, position) in positions.iter().enumerate() {
            rows.push(self.position_row(ix, position, theme, cx));
        }
        card(theme)
            .child(section_header(
                theme,
                "Posiciones",
                SharedString::from(format!("{closable} abiertas")),
            ))
            .child(positions_head(theme))
            .children(rows)
    }

    fn position_row(
        &self,
        ix: usize,
        position: &Position,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let symbol = SharedString::from(position.symbol.clone());
        let pnl = position.pnl.unwrap_or(0.0);
        let pnl_color = if pnl >= 0.0 {
            theme.success
        } else {
            theme.danger
        };
        let (side, side_tone) = match position.side {
            PositionSide::Long => ("LONG", theme.success),
            PositionSide::Short => ("SHORT", theme.danger),
        };
        div()
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .border_t_1()
            .border_color(theme.border)
            .when(ix % 2 == 1, |el| el.bg(crate::theme::ink(0.015)))
            .child(
                div()
                    .flex_basis(px(80.0))
                    .flex_grow(1.0)
                    .flex_shrink(1.0)
                    .min_w(px(56.0))
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(symbol.clone()),
            )
            .child(div().w(px(58.0)).flex_none().child(chip(side, side_tone)))
            .child(num_cell(theme, 70.0, model::number_es(position.size, 2)))
            .child(num_cell(theme, 92.0, model::plain(position.entry)))
            .child(num_cell(theme, 92.0, model::plain(position.current)))
            .child(money_cell(theme, 96.0, model::signed_money(position.pnl), pnl_color))
            .child(money_cell(
                theme,
                74.0,
                model::signed_pct(position.pnl_pct),
                pnl_color,
            ))
            .child(
                div()
                    .w(px(CLOSE_COLUMN))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .child(
                        button(
                            ("trading-close-position", ix),
                            "Cerrar",
                            ButtonTone::Ghost,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ask(Confirm::Close(symbol.to_string()), cx);
                        })),
                    ),
            )
    }

    fn systems_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let systems = model::systems_sorted(snapshot);
        let running = systems.iter().filter(|system| system.running).count();
        let stopped = systems.len() - running;
        let collapsed = self.systems_collapsed;
        let stopped_open = self.stopped_open;
        let mut rows: Vec<AnyElement> = Vec::with_capacity(systems.len() + 1);
        if !collapsed {
            if systems.is_empty() {
                rows.push(
                    empty_row(theme, "Sin sistemas: elige su carpeta en Ajustes → Sistemas.")
                        .into_any_element(),
                );
            } else {
                // Running bots first (the sort already does that), then the
                // idle ones behind a summary row: on a normal day that is 10
                // rows instead of 15, and the positions table keeps its room.
                let mut ix = 0;
                for system in systems.iter().filter(|system| system.running) {
                    rows.push(self.system_row(ix, system, theme, cx).into_any_element());
                    ix += 1;
                }
                if stopped > 0 {
                    rows.push(self.stopped_toggle(stopped, stopped_open, theme, cx));
                    if stopped_open {
                        for system in systems.iter().filter(|system| !system.running) {
                            rows.push(self.system_row(ix, system, theme, cx).into_any_element());
                            ix += 1;
                        }
                    }
                }
            }
        }
        card(theme)
            .child(self.systems_header(theme, running, systems.len(), collapsed, cx))
            .children(rows)
    }

    /// `▸ 5 parados` / `⌃ 5 parados · ocultar` — the disclosure that keeps the
    /// idle bots out of the list until they are wanted.
    fn stopped_toggle(
        &self,
        count: usize,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chevron = if open {
            crate::icons::ALT_ARROW_UP
        } else {
            crate::icons::ALT_ARROW_RIGHT
        };
        div()
            .id("trading-stopped-toggle")
            .h(px(SYSTEM_ROW_HEIGHT))
            .flex_none()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .border_t_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::ink(0.04)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.stopped_open = !this.stopped_open;
                cx.notify();
            }))
            .child(
                crate::icons::icon(chevron)
                    .size(px(10.0))
                    .text_color(theme.text_faint),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("{count} parados"))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(theme.text_faint.opacity(0.8))
                    .child(if open { "ocultar" } else { "ver" }),
            )
            .into_any_element()
    }

    /// The systems card's header is the fold control: one click hides the list
    /// and leaves the count ("10 de 15 en marcha") in sight.
    fn systems_header(
        &self,
        theme: &Theme,
        running: usize,
        total: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chevron = if collapsed {
            crate::icons::ALT_ARROW_RIGHT
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        div()
            .id("trading-systems-fold")
            .w_full()
            .h(px(30.0))
            .flex_none()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .bg(theme.surface_raised.opacity(0.5))
            .cursor_pointer()
            .hover(|el| el.bg(theme.surface_raised.opacity(0.9)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.systems_collapsed = !this.systems_collapsed;
                cx.notify();
            }))
            .child(
                div().flex_none().size(px(12.0)).child(
                    crate::icons::icon(chevron)
                        .size(px(11.0))
                        .text_color(theme.text_muted),
                ),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("Sistemas"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "{running} de {total} en marcha"
                    ))),
            )
    }

    /// One line of the systems list: a state dot, id, name, symbol ·
    /// timeframe, P&L and the start/stop control. The state is the dot plus
    /// the row's brightness (running names are full-strength, idle ones dim):
    /// the word would cost 58px of a 24px row.
    fn system_row(
        &self,
        ix: usize,
        system: &System,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let stop_id = SharedString::from(system.id.clone());
        let state_tone = if system.running {
            theme.success
        } else {
            theme.text_faint
        };
        let action = if system.running {
            button(("trading-stop-system", ix), "Parar", ButtonTone::Ghost, theme)
                .h(px(18.0))
                .px(px(6.0))
                .text_size(px(10.0))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(Confirm::Stop(stop_id.to_string()), cx);
                }))
        } else {
            let system_id = system.id.clone();
            button(
                ("trading-start-system", ix),
                "Arrancar",
                ButtonTone::Primary,
                theme,
            )
            .h(px(18.0))
            .px(px(6.0))
            .text_size(px(10.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.call(
                    methods::TRADING_SYSTEM_START,
                    json!({ "systemId": system_id.clone() }),
                    cx,
                );
                cx.notify();
            }))
        };
        let meta = match (system.symbol.is_empty(), system.timeframe.is_empty()) {
            (true, true) => String::new(),
            (true, false) => system.timeframe.clone(),
            (false, true) => system.symbol.clone(),
            (false, false) => format!("{} · {}", system.symbol, system.timeframe),
        };
        let pnl_tone = match system.pnl_day {
            Some(value) if value < 0.0 => theme.danger,
            Some(_) => theme.success,
            None => theme.text_faint,
        };
        div()
            .h(px(SYSTEM_ROW_HEIGHT))
            .flex_none()
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .w(px(5.0))
                    .h(px(5.0))
                    .flex_none()
                    .rounded(px(2.5))
                    .bg(state_tone),
            )
            .child(
                // Pool ids are two digits; longer ones (the demo fixture's
                // folder names) stay on one line and end in "…" instead of
                // wrapping into a column over the card's header.
                div()
                    .min_w(px(18.0))
                    .max_w(px(96.0))
                    .flex_none()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(system.id.clone())),
            )
            .child(
                // The name, then why it stopped or what's wrong (faint, cut
                // with "…" when the row runs out of room).
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(220.0))
                            .truncate()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if system.running {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(model::system_label(&system.name))),
                    )
                    .when_some(system.note.clone(), |row, note| {
                        let tone = if note.starts_with("falló") {
                            theme.danger
                        } else if note.starts_with("aviso") {
                            theme.warning
                        } else {
                            theme.text_faint
                        };
                        row.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(10.0))
                                .text_color(tone)
                                .child(SharedString::from(note)),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(80.0))
                    .flex_none()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(meta)),
            )
            .child(
                div()
                    .w(px(78.0))
                    .flex_none()
                    .text_right()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(9.5))
                    .text_color(pnl_tone)
                    .child(model::signed_money(system.pnl_day)),
            )
            .child(action)
    }

    fn link_row(
        &self,
        ix: usize,
        link: &Link,
        demo: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let (broker_label, broker_tone) = match link.broker {
            Some(Broker::Mt5) => ("MT5", theme.accent),
            Some(Broker::Nt) => ("NT", theme.warning),
            Some(Broker::Ibkr) | None => ("IBKR", theme.success),
        };
        let _ = demo;
        let (state_label, state_tone) = if link.connected {
            ("CONECTADA", theme.success)
        } else {
            ("DESCONECTADA", theme.warning)
        };
        let link_id = link.id.clone();
        let label = link.label.clone().unwrap_or_else(|| link.id.clone());
        div()
            .px(px(12.0))
            .py(px(7.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .border_t_1()
            .border_color(theme.border)
            .child(chip(broker_label, broker_tone))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .truncate()
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(9.0))
                            .text_color(theme.text_faint)
                            .truncate()
                            .child(SharedString::from(link.id.clone())),
                    ),
            )
            .child(chip(state_label, state_tone))
            .child(
                button(
                    ("trading-link-remove", ix),
                    "Quitar",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.call(
                        methods::TRADING_LINK_REMOVE,
                        json!({ "linkId": link_id.clone() }),
                        cx,
                    );
                    cx.notify();
                })),
            )
    }

    fn rules_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        // Only the rules that are watching: a fired or cancelled one has
        // already said what it had to say, and the card exists to show what
        // the desk is still waiting for.
        let rules: Vec<&Rule> = model::rules_sorted(snapshot)
            .into_iter()
            .filter(|rule| rule.status == RuleStatus::Watching)
            .collect();
        let meta = match rules.len() {
            0 => "ninguna vigilando".to_string(),
            1 => "1 vigilando".to_string(),
            n => format!("{n} vigilando"),
        };
        let mut rows: Vec<Div> = Vec::with_capacity(rules.len());
        if rules.is_empty() {
            rows.push(empty_row(theme, "Sin reglas vigilando. Pídelas en el chat."));
        } else {
            for (ix, rule) in rules.iter().take(3).enumerate() {
                rows.push(self.rule_row(ix, rule, snapshot, theme, cx));
            }
        }
        card(theme)
            .child(section_header(theme, "Reglas", meta.into()))
            .children(rows)
    }

    fn rule_row(
        &self,
        ix: usize,
        rule: &Rule,
        snapshot: &Snapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let (status_label, status_tone) = match rule.status {
            RuleStatus::Watching => ("VIGILANDO", theme.accent),
            RuleStatus::Fired => ("AVISÓ", theme.warning),
            RuleStatus::Cancelled => ("CANCELADA", theme.text_faint),
            RuleStatus::Expired => ("CADUCADA", theme.text_faint),
        };
        let expires = rule
            .expires_at
            .map(|at| format!("caduca {}", model::short_clock(at)))
            .unwrap_or_else(|| "sin caducidad".to_string());
        let mut header = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .truncate()
                    .child(SharedString::from(rule.name.clone())),
            )
            .child(chip(status_label, status_tone));
        if rule.status == RuleStatus::Watching {
            let rule_id = rule.id.clone();
            header = header.child(
                button(
                    ("trading-rule-cancel", ix),
                    "Quitar",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.call(
                        methods::TRADING_RULE_CANCEL,
                        json!({ "ruleId": rule_id.clone() }),
                        cx,
                    );
                    cx.notify();
                })),
            );
        }
        let mut row = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .py(px(5.0))
            .child(header)
            .child(mono_line(
                &format!(
                    "{} · {} · {expires}",
                    rule.condition.describe(&snapshot.systems),
                    if rule.proposal.is_some() {
                        "propone al cumplirse"
                    } else {
                        "solo avisa"
                    }
                ),
                theme,
            ));
        if let Some(note) = &rule.note {
            row = row.child(
                div()
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .truncate()
                    .child(SharedString::from(note.clone())),
            );
        }
        row
    }

    fn task_row(
        &self,
        ix: usize,
        task: &TradingTask,
        snapshot: &Snapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let kind_label = match task.kind {
            TaskKind::StartSystems => "ARRANCAR",
            TaskKind::StopSystems => "PARAR",
        };
        let kind_tone = match task.kind {
            TaskKind::StartSystems => theme.success,
            TaskKind::StopSystems => theme.warning,
        };
        let (status_label, status_tone) = match task.status {
            TaskStatus::Pending => ("PENDIENTE", theme.accent),
            TaskStatus::Ran => ("HECHA", theme.success),
            TaskStatus::Missed => ("PERDIDA", theme.danger),
            TaskStatus::Cancelled => ("CANCELADA", theme.text_faint),
            TaskStatus::Failed => ("FALLIDA", theme.danger),
        };
        let systems = if task.system_ids.is_empty() {
            "Todos".to_string()
        } else {
            task.system_ids
                .iter()
                .map(|id| {
                    snapshot
                        .systems
                        .iter()
                        .find(|system| &system.id == id)
                        .map(|system| system.name.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let cancel = (task.status == TaskStatus::Pending).then(|| {
            let task_id = task.id.clone();
            button(
                ("trading-cancel-task", ix),
                "Cancelar",
                ButtonTone::Ghost,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.call(
                    methods::TRADING_TASK_CANCEL,
                    json!({ "taskId": task_id.clone() }),
                    cx,
                );
                cx.notify();
            }))
        });
        div()
            .px(px(12.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(chip(kind_label, kind_tone))
                    .child(chip(status_label, status_tone))
                    .child(div().flex_1())
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(10.0))
                            .text_color(theme.text_muted)
                            .child(model::short_clock(task.run_at)),
                    )
                    .children(cancel),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .truncate()
                    .child(SharedString::from(systems)),
            )
    }

    fn logs_card(&self, snapshot: &Snapshot, theme: &Theme, now: i64) -> Div {
        let logs = model::logs_newest(snapshot, 8);
        let mut rows: Vec<Div> = Vec::with_capacity(logs.len());
        if logs.is_empty() {
            rows.push(empty_row(theme, "El motor no ha registrado eventos."));
        } else {
            for log in logs {
                let tone = match log.level {
                    LogLevel::Info => theme.text_muted,
                    LogLevel::Warning => theme.warning,
                    LogLevel::Error => theme.danger,
                };
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .h(px(20.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .when(log.level == LogLevel::Error, |el| {
                            el.bg(theme.danger.opacity(0.05))
                        })
                        .child(
                            div()
                                .w(px(48.0))
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(9.5))
                                .text_color(theme.text_faint)
                                .child(model::clock(log.ts)),
                        )
                        .child(
                            div()
                                .w(px(36.0))
                                .flex_none()
                                .text_size(px(9.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tone)
                                .child(model::level_label(log.level)),
                        )
                        .child(
                            div()
                                .w(px(48.0))
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(9.5))
                                .text_color(theme.text_faint)
                                .truncate()
                                .child(SharedString::from(log.system.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(if log.level == LogLevel::Error {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .child(SharedString::from(i18n::log_text(log))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(9.0))
                                .text_color(theme.text_faint)
                                .child(model::relative(log.ts, now)),
                        ),
                );
            }
        }
        card(theme)
            .child(section_header(theme, "Registro del motor", "últimos eventos".into()))
            .children(rows)
    }
}

impl Render for TradingSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // The page starts under the titlebar row, which now carries the desk's
        // heading too (`titlebar_row`) — this body keeps only the strips and
        // the cards, so it starts right below the bar.
        let mut root = div()
            .size_full()
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.font_sans.clone());
        if let Some(bar) = self.confirm_bar(&theme, cx) {
            root = root.child(bar);
        }
        if let Some(strip) = self.error_strip(&theme) {
            root = root.child(strip);
        }
        if let Some(strip) = self.notice_strip(&theme) {
            root = root.child(strip);
        }
        root.child(self.body(&theme, cx))
    }
}

// ---------------------------------------------------------------- helpers

/// A column of the desk: grows from its basis and stacks its cards with the
/// page's gap.
fn board_column(basis: f32, grow: f32) -> Div {
    div()
        .flex_basis(px(basis))
        .flex_grow(grow)
        .flex_shrink(1.0)
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap(px(COLUMN_GAP))
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    tone: ButtonTone,
    theme: &Theme,
) -> Stateful<Div> {
    let (bg, fg, hover) = match tone {
        ButtonTone::Ghost => (
            crate::theme::ink(0.06),
            theme.text_muted,
            crate::theme::ink(0.12),
        ),
        ButtonTone::Primary => (
            theme.accent.opacity(0.14),
            theme.accent,
            theme.accent.opacity(0.24),
        ),
        ButtonTone::Danger => (
            theme.danger.opacity(0.12),
            theme.danger,
            theme.danger.opacity(0.22),
        ),
    };
    div()
        .id(id)
        .h(px(24.0))
        .flex_none()
        .px(px(9.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .bg(bg)
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(label.into())
}

fn chip(label: impl Into<SharedString>, tone: Hsla) -> Div {
    div()
        .flex_none()
        .h(px(17.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .bg(tone.opacity(0.10))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(tone)
        .border_1()
        .border_color(tone.opacity(0.18))
        .child(label.into())
}

/// What a proposal asks for, in the desk's words.
fn proposal_title(action: &ProposalAction) -> String {
    match action {
        ProposalAction::ClosePosition { symbol } => format!("Cerrar la posición de {symbol}"),
        ProposalAction::CloseAll => "Cerrar todas las posiciones".into(),
        ProposalAction::Panic => "Pánico: parar bots, cancelar tareas y cerrar todo".into(),
        ProposalAction::StartSystems { system_ids } => systems_title("Arrancar", system_ids),
        ProposalAction::StopSystems { system_ids } => systems_title("Parar", system_ids),
        ProposalAction::ScheduleTask {
            task_kind,
            system_ids,
            delay_minutes,
            at_time,
        } => schedule_title(*task_kind, system_ids, *delay_minutes, at_time.as_deref()),
    }
}

/// `Programar: parar 2 sistemas a las 22:05` / `en 15 min`.
fn schedule_title(
    task_kind: TaskKind,
    system_ids: &[String],
    delay_minutes: Option<f64>,
    at_time: Option<&str>,
) -> String {
    let verb = match task_kind {
        TaskKind::StartSystems => "arrancar",
        TaskKind::StopSystems => "parar",
    };
    let systems = match system_ids {
        [] => "todos los sistemas".to_string(),
        [one] => format!("el sistema {one}"),
        many => format!("{} sistemas", many.len()),
    };
    let when = match (delay_minutes, at_time) {
        (Some(minutes), _) => {
            let text = format!("{minutes}");
            let text = text.strip_suffix(".0").unwrap_or(&text);
            format!(" en {text} min")
        }
        (_, Some(at)) => format!(" a las {at}"),
        _ => String::new(),
    };
    format!("Programar: {verb} {systems}{when}")
}

/// `Arrancar el sistema 01_a` for one, `Arrancar 3 sistemas` for a group.
fn systems_title(verb: &str, system_ids: &[String]) -> String {
    match system_ids {
        [one] => format!("{verb} el sistema {one}"),
        _ => format!("{verb} {} sistemas", system_ids.len()),
    }
}

/// The affected systems as the desk names them, each with the number that
/// explains why it is in the group (its P&L %, or its state without capital).
fn system_lines(system_ids: &[String], snapshot: &Snapshot) -> Vec<String> {
    system_ids
        .iter()
        .map(|id| match snapshot.systems.iter().find(|system| &system.id == id) {
            Some(system) => match system.pnl_pct {
                Some(pct) => format!("{} · {}", system.name, model::signed_pct(Some(pct))),
                None => format!(
                    "{} · {}",
                    system.name,
                    if system.running { "en marcha" } else { "parado" }
                ),
            },
            None => id.clone(),
        })
        .collect()
}

/// One proposal's text column in the chat's narrow inbox; the caller appends
/// its buttons or status chip on their own line, so the reason is not squeezed.
fn chat_proposal_row(proposal: &TradingProposal, snapshot: &Snapshot, theme: &Theme) -> Div {
    let detail = match &proposal.result {
        Some(result) if proposal.status != ProposalStatus::Pending => result.clone(),
        _ => proposal.reason.clone(),
    };
    let affected = match &proposal.action {
        ProposalAction::StartSystems { system_ids } | ProposalAction::StopSystems { system_ids }
            if system_ids.len() > 1 =>
        {
            Some(system_lines(system_ids, snapshot))
        }
        ProposalAction::ScheduleTask { system_ids, .. } if system_ids.len() > 1 => {
            Some(system_lines(system_ids, snapshot))
        }
        _ => None,
    };
    let mut row = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .truncate()
                .child(SharedString::from(proposal_title(&proposal.action))),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_muted)
                .truncate()
                .child(SharedString::from(detail)),
        );
    if let Some(lines) = affected {
        // Enough to see why each one is in; the rest is a count, not a wall.
        const SHOWN: usize = 6;
        for line in lines.iter().take(SHOWN) {
            row = row.child(mono_line(line, theme));
        }
        if lines.len() > SHOWN {
            row = row.child(mono_line(&format!("y {} más", lines.len() - SHOWN), theme));
        }
    }
    row
}

/// A small mono line inside a proposal card (system names and numbers).
fn mono_line(text: &str, theme: &Theme) -> Div {
    div()
        .font_family(theme.font_mono.clone())
        .text_size(px(9.5))
        .text_color(theme.text_faint)
        .truncate()
        .child(SharedString::from(text.to_owned()))
}

fn card(theme: &Theme) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(10.0))
        .bg(theme.surface_card)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
}

fn section_header(
    theme: &Theme,
    title: &'static str,
    meta: SharedString,
) -> Div {
    div()
        .w_full()
        .h(px(30.0))
        .flex_none()
        .px(px(12.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .bg(theme.surface_raised.opacity(0.5))
        .child(
            div()
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(9.0))
                .text_color(theme.text_faint)
                .child(meta),
        )
}

fn empty_row(theme: &Theme, message: &'static str) -> Div {
    div()
        .px(px(12.0))
        .py(px(8.0))
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .border_t_1()
        .border_color(theme.border)
        .child(message)
}

/// The close button's column, the one fixed-width cell of a position row.
const CLOSE_COLUMN: f32 = 64.0;

/// A numeric column: `width` is its share of the row, never a floor, so a
/// narrow card shrinks the figures instead of pushing the close button out.
fn flex_cell(width: f32) -> Div {
    div()
        .flex_basis(px(width))
        .flex_grow(1.0)
        .flex_shrink(1.0)
        .min_w(px(0.0))
        .truncate()
}

fn head_cell(theme: &Theme, width: f32, label: &'static str) -> Div {
    flex_cell(width)
        .text_right()
        .text_size(px(8.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_faint)
        .child(label)
}

fn num_cell(theme: &Theme, width: f32, value: String) -> Div {
    flex_cell(width)
        .text_right()
        .font_family(theme.font_mono.clone())
        .text_size(px(10.5))
        .text_color(theme.text_muted)
        .child(value)
}

fn money_cell(theme: &Theme, width: f32, value: String, tone: Hsla) -> Div {
    flex_cell(width)
        .text_right()
        .font_family(theme.font_mono.clone())
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(tone)
        .child(value)
}

fn positions_head(theme: &Theme) -> Div {
    div()
        .h(px(24.0))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_basis(px(80.0))
                .flex_grow(1.0)
                .flex_shrink(1.0)
                .min_w(px(56.0))
                .text_size(px(8.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_faint)
                .child("SÍMBOLO"),
        )
        .child(
            div()
                .w(px(58.0))
                .flex_none()
                .text_size(px(8.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_faint)
                .child("LADO"),
        )
        .child(head_cell(theme, 70.0, "TAMAÑO"))
        .child(head_cell(theme, 92.0, "ENTRADA"))
        .child(head_cell(theme, 92.0, "ACTUAL"))
        .child(head_cell(theme, 96.0, "P&L"))
        .child(head_cell(theme, 74.0, "P&L %"))
        .child(div().w(px(CLOSE_COLUMN)).flex_none())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
