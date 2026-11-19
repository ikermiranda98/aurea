//! The trading surface: header, KPI strip, positions table, systems, tasks and
//! the engine verdict log, all fed by `WatchTradingSnapshot`.
//!
//! Money-touching actions (close one, close all, panic) never fire straight
//! from a click: they stage a [`Confirm`] bar that names the consequence first.

use gpui::{
    Context, Div, Entity, Hsla, IntoElement, Render, SharedString, Stateful, Task, Window, div,
    prelude::*, px,
};
use serde_json::json;
use zeron_proto::trading::{
    Broker, IbkrPort, Link, LogLevel, Mt5Detected, PanicResult, Position, PositionCloseResult,
    PositionSide, ProposalAction, ProposalStatus, Snapshot, System, Task as TradingTask,
    TaskKind, TaskStatus, TradingProposal,
};
use zeron_rpc::methods;

use super::i18n;
use super::model::{self, Confirm, Phase};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 30.0;
/// Narrowest the main column (equity, positions, log) goes beside the side
/// column before the side column wraps under it.
const MAIN_COLUMN_BASIS: f32 = 600.0;
/// The side column's (systems, connections, tasks) own width beside it.
const SIDE_COLUMN_BASIS: f32 = 360.0;
const PAGE_PAD: f32 = 16.0;

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
            while let Some(value) = stream.recv().await {
                match serde_json::from_value::<Snapshot>(value) {
                    Ok(snapshot) => {
                        let alive = this
                            .update(cx, |this, cx| {
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
            Confirm::CloseAll => {
                let request_id = format!("ui:{}", uuid::Uuid::new_v4());
                self.call_with(
                    methods::TRADING_POSITIONS_CLOSE_ALL,
                    json!({ "requestId": request_id }),
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

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let demo = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| model::kpis(snapshot).demo);
        let version = self
            .snapshot
            .as_ref()
            .map(|snapshot| SharedString::from(format!("motor {}", snapshot.version)))
            .unwrap_or_else(|| SharedString::from("sin snapshot"));
        div()
            .flex_none()
            .w_full()
            .h(px(52.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(18.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Aurea"),
                    )
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_faint)
                            .child("MESA DE TRADING"),
                    ),
            )
            // The phase reads "EN VIVO" once the stream is up; on the demo
            // fixture that would contradict the DEMO chip beside it.
            .when(!(demo && self.phase == Phase::Live), |el| {
                el.child(self.phase_chip(theme))
            })
            .when(demo, |el| {
                el.child(chip("DEMO", theme.accent))
            })
            .child(
                div()
                    .text_size(px(9.5))
                    .font_family(theme.font_mono.clone())
                    .text_color(theme.text_faint)
                    .child(version),
            )
            .child(div().flex_1())
            .child(
                button("trading-open-chat", "Chat", ButtonTone::Ghost, theme).on_click(
                    cx.listener(|_, _, window, cx| {
                        // A new chat with the agent: it reads the desk through
                        // its trading tools and asks here before moving money.
                        window.dispatch_action(Box::new(crate::shell::NewSession), cx);
                    }),
                ),
            )
            .child(
                button("trading-close-all", "Cerrar todo", ButtonTone::Ghost, theme).on_click(
                    cx.listener(|this, _, _, cx| this.ask(Confirm::CloseAll, cx)),
                ),
            )
            .child(
                button("trading-panic", "Pánico", ButtonTone::Danger, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.ask(Confirm::Panic, cx))),
            )
    }

    fn phase_chip(&self, theme: &Theme) -> Div {
        let (label, color) = match &self.phase {
            Phase::Connecting => ("CONECTANDO", theme.warning),
            Phase::Live => ("EN VIVO", theme.success),
            Phase::Failed(_) => ("SIN MOTOR", theme.danger),
        };
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .child(div().w(px(6.0)).h(px(6.0)).rounded(px(3.0)).bg(color))
            .child(
                div()
                    .text_size(px(9.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(color)
                    .child(label),
            )
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
            .gap(px(14.0))
            .children(self.proposals_card(&snapshot, theme, cx))
            .child(self.kpi_strip(&snapshot, theme))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_start()
                    .gap(px(14.0))
                    // Both columns grow from their basis (2:1); when the two
                    // bases no longer fit side by side, the side column wraps
                    // under the main one and each takes the full width, so the
                    // positions table never squeezes its figures to "21.4…".
                    .child(
                        div()
                            .flex_basis(px(MAIN_COLUMN_BASIS))
                            .flex_grow(2.0)
                            .flex_shrink(1.0)
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .child(self.equity_card(&snapshot, theme))
                            .child(self.positions_card(&snapshot, theme, cx))
                            .child(self.logs_card(&snapshot, theme, now)),
                    )
                    .child(
                        div()
                            .flex_basis(px(SIDE_COLUMN_BASIS))
                            .flex_grow(1.0)
                            .flex_shrink(1.0)
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(14.0))
                            .child(self.systems_card(&snapshot, theme, cx))
                            .child(self.links_card(&snapshot, theme, cx))
                            .child(self.tasks_card(&snapshot, theme, cx)),
                    ),
            )
            .into_any_element()
    }

    /// The agent's money requests: pending ones with Approve / Reject, then
    /// the last few answered, so the person sees what ran.
    fn proposals_card(
        &self,
        snapshot: &Snapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let proposals = snapshot.proposals.clone().unwrap_or_default();
        let pending: Vec<&TradingProposal> = proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Pending)
            .collect();
        let answered: Vec<&TradingProposal> = proposals
            .iter()
            .filter(|p| p.status != ProposalStatus::Pending)
            .take(3)
            .collect();
        if pending.is_empty() && answered.is_empty() {
            return None;
        }
        let mut rows: Vec<Div> = Vec::new();
        for (ix, proposal) in pending.iter().enumerate() {
            let approve_id = proposal.id.clone();
            let reject_id = proposal.id.clone();
            rows.push(
                proposal_row(proposal, theme)
                    .child(
                        button(("trading-proposal-reject", ix), "Rechazar", ButtonTone::Ghost, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_proposal(reject_id.clone(), false, cx)
                            })),
                    )
                    .child(
                        button(("trading-proposal-approve", ix), "Aprobar", ButtonTone::Primary, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_proposal(approve_id.clone(), true, cx)
                            })),
                    ),
            );
        }
        for proposal in answered {
            let (label, tone) = match proposal.status {
                ProposalStatus::Done => ("HECHA", theme.success),
                ProposalStatus::Failed => ("FALLÓ", theme.danger),
                ProposalStatus::Rejected => ("RECHAZADA", theme.text_faint),
                ProposalStatus::Pending => ("PENDIENTE", theme.warning),
            };
            rows.push(
                proposal_row(proposal, theme)
                    .opacity(0.7)
                    .child(chip(label, tone)),
            );
        }
        Some(
            card(theme)
                .border_color(if pending.is_empty() {
                    theme.border
                } else {
                    theme.accent.opacity(0.6)
                })
                .child(section_header(
                    theme,
                    "Peticiones del agente",
                    SharedString::from(if pending.is_empty() {
                        "nada pendiente".to_string()
                    } else {
                        format!("{} pendiente(s) de tu aprobación", pending.len())
                    }),
                ))
                .children(rows),
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
            .child(self.kpi_card(theme, "EQUITY", model::money(kpis.equity), theme.text))
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

    fn kpi_card(&self, theme: &Theme, label: &'static str, value: String, tone: Hsla) -> Div {
        div()
            .flex_1()
            .min_w(px(110.0))
            .p(px(12.0))
            .rounded(px(10.0))
            .bg(theme.surface_card)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(9.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_faint)
                    .child(label),
            )
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(17.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(tone)
                    .child(value),
            )
    }

    fn equity_card(&self, snapshot: &Snapshot, theme: &Theme) -> Div {
        let bars = model::spark_bars(snapshot, 64);
        let chart = if bars.is_empty() {
            div()
                .h(px(56.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child("Sin datos de equity todavía.")
                .into_any_element()
        } else {
            div()
                .h(px(56.0))
                .w_full()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(1.0))
                .children(bars.into_iter().map(|value| {
                    div()
                        .flex_1()
                        .h(px(3.0 + 53.0 * value.clamp(0.0, 1.0)))
                        .rounded(px(1.0))
                        .bg(theme.accent.opacity(0.55))
                }))
                .into_any_element()
        };
        card(theme)
            .child(section_header(theme, "Curva de equity", "últimos 15 min".into()))
            .child(div().px(px(12.0)).py(px(10.0)).child(chart))
    }

    fn positions_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let positions = model::positions_sorted(snapshot);
        let closable = positions.len();
        let mut rows: Vec<Div> = Vec::with_capacity(positions.len());
        if positions.is_empty() {
            rows.push(empty_row(theme, "Sin posiciones abiertas."));
        } else {
            for (ix, position) in positions.iter().enumerate() {
                rows.push(self.position_row(ix, position, theme, cx));
            }
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
        let mut rows: Vec<Div> = Vec::with_capacity(systems.len());
        if systems.is_empty() {
            rows.push(empty_row(theme, "El pool no tiene sistemas."));
        } else {
            for (ix, system) in systems.iter().enumerate() {
                rows.push(self.system_row(ix, system, theme, cx));
            }
        }
        card(theme)
            .child(section_header(
                theme,
                "Sistemas",
                SharedString::from(format!("{running} en marcha")),
            ))
            .children(rows)
    }

    fn system_row(
        &self,
        ix: usize,
        system: &System,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = SharedString::from(system.id.clone());
        let (state_label, state_tone) = if system.running {
            ("EN MARCHA", theme.success)
        } else {
            ("PARADO", theme.text_faint)
        };
        let pnl_tone = if system.pnl_day.unwrap_or(0.0) >= 0.0 {
            theme.success
        } else {
            theme.danger
        };
        let action = if system.running {
            button(("trading-stop-system", ix), "Parar", ButtonTone::Ghost, theme).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.ask(Confirm::Stop(id.to_string()), cx);
                }),
            )
        } else {
            let system_id = system.id.clone();
            button(
                ("trading-start-system", ix),
                "Arrancar",
                ButtonTone::Primary,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.call(
                    methods::TRADING_SYSTEM_START,
                    json!({ "systemId": system_id.clone() }),
                    cx,
                );
                cx.notify();
            }))
        };
        div()
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .truncate()
                                    .child(SharedString::from(system.name.clone())),
                            )
                            .child(div().flex_none().child(chip(state_label, state_tone))),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .truncate()
                            .child(SharedString::from(format!(
                                "{} · {} · {}",
                                system.id, system.symbol, system.timeframe
                            ))),
                    ),
            )
            .child(
                div()
                    .w(px(70.0))
                    .flex_none()
                    .text_right()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(pnl_tone)
                    .child(model::signed_money(system.pnl_day)),
            )
            .child(action)
    }

    fn links_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let links = snapshot.links.clone().unwrap_or_default();
        let connected = links.iter().filter(|link| link.connected).count();
        // The demo fixture never enumerates accounts (`accounts: None`); a live
        // TWS paper link always does. That is what tells the button which way
        // the surface is running.
        let live = snapshot.accounts.is_some();
        let mut rows: Vec<Div> = Vec::with_capacity(links.len());
        if links.is_empty() {
            rows.push(empty_row(theme, "Sin conexiones registradas."));
        } else {
            for (ix, link) in links.iter().enumerate() {
                rows.push(self.link_row(ix, link, !live, theme, cx));
            }
        }
        let actions = div()
            .w_full()
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .border_t_1()
            .border_color(theme.border)
            .child(
                button(
                    "trading-link-add-tws",
                    "Añadir TWS paper",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = Some("Registrando TWS paper en 127.0.0.1:7497…".into());
                    this.call(
                        methods::TRADING_LINK_ADD,
                        json!({
                            "host": "127.0.0.1",
                            "port": 7497,
                            "label": "TWS paper",
                            "broker": "ibkr",
                        }),
                        cx,
                    );
                    cx.notify();
                })),
            )
            .child(
                button(
                    "trading-link-add-mt5",
                    "Añadir MT5",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = Some("Buscando terminales MetaTrader 5 en este equipo…".into());
                    this.call_with(
                        methods::TRADING_MT5_DETECT,
                        json!({ "probe": false }),
                        cx,
                        |this, value, cx| {
                            let detected = match serde_json::from_value::<Vec<Mt5Detected>>(value) {
                                Ok(detected) => detected,
                                Err(error) => {
                                    this.error =
                                        Some(format!("Respuesta MT5 inválida: {error}").into());
                                    return;
                                }
                            };
                            let Some(account) = detected.into_iter().next() else {
                                this.notice =
                                    Some("No se detectó ningún MetaTrader 5 instalado.".into());
                                return;
                            };
                            this.notice = Some(
                                format!("Vinculando MT5 {}@{}…", account.login, account.server)
                                    .into(),
                            );
                            this.call(
                                methods::TRADING_LINK_ADD,
                                json!({
                                    "broker": "mt5",
                                    "mt5Login": account.login,
                                    "mt5Server": account.server,
                                    "label": account.name,
                                }),
                                cx,
                            );
                        },
                    );
                    cx.notify();
                })),
            )
            .child(div().flex_1())
            .child(if live {
                button(
                    "trading-disconnect-tws",
                    "Desconectar TWS",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = Some("Cerrando la conexión con TWS…".into());
                    this.call(methods::TRADING_DISCONNECT, json!({}), cx);
                    cx.notify();
                }))
            } else {
                button(
                    "trading-connect-tws",
                    "Conectar TWS paper",
                    ButtonTone::Primary,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = Some("Conectando con TWS paper en 127.0.0.1:7497…".into());
                    this.call(methods::TRADING_CONNECT, json!({}), cx);
                    cx.notify();
                }))
            })
            .child(
                button(
                    "trading-detect-ports",
                    "Detectar puertos",
                    ButtonTone::Ghost,
                    theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = Some("Sondeando los puertos de TWS…".into());
                    this.call_with(
                        methods::TRADING_IBKR_DETECT,
                        json!({}),
                        cx,
                        |this, value, _cx| {
                            let ports = match serde_json::from_value::<Vec<IbkrPort>>(value) {
                                Ok(ports) => ports,
                                Err(error) => {
                                    this.error =
                                        Some(format!("Respuesta de puertos inválida: {error}").into());
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
                    cx.notify();
                })),
            );
        let meta = if live {
            SharedString::from(format!("{connected}/{} activas", links.len()))
        } else {
            SharedString::from("de ejemplo")
        };
        card(theme)
            .child(section_header(theme, "Conexiones", meta))
            .children(rows)
            .when(!live, |el| {
                el.child(
                    div()
                        .px(px(12.0))
                        .py(px(7.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child("Modo demo: estas conexiones son de ejemplo. Conecta TWS paper para ver tu cuenta."),
                )
            })
            .child(actions)
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
        let (state_label, state_tone) = if demo {
            // The demo fixture's links are invented: they are not connected
            // to anything, and "CONECTADA" beside "Conectar TWS paper" read
            // as a contradiction.
            ("EJEMPLO", theme.text_faint)
        } else if link.connected {
            ("CONECTADA", theme.success)
        } else {
            ("CAÍDA", theme.danger)
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

    fn tasks_card(&self, snapshot: &Snapshot, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let tasks = model::tasks_sorted(snapshot);
        let mut rows: Vec<Div> = Vec::with_capacity(tasks.len());
        if tasks.is_empty() {
            rows.push(empty_row(theme, "Sin tareas programadas."));
        } else {
            for (ix, task) in tasks.iter().take(6).enumerate() {
                rows.push(self.task_row(ix, task, snapshot, theme, cx));
            }
        }
        card(theme)
            .child(section_header(theme, "Tareas", "programadas".into()))
            .children(rows)
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
        let logs = model::logs_newest(snapshot, 12);
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
                        .h(px(24.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .when(log.level == LogLevel::Error, |el| {
                            el.bg(theme.danger.opacity(0.05))
                        })
                        .child(
                            div()
                                .w(px(56.0))
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(9.5))
                                .text_color(theme.text_faint)
                                .child(model::clock(log.ts)),
                        )
                        .child(
                            div()
                                .w(px(48.0))
                                .flex_none()
                                .text_size(px(9.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(tone)
                                .child(model::level_label(log.level)),
                        )
                        .child(
                            div()
                                .w(px(110.0))
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
        // The page starts under the titlebar row, like Zeron's Pull requests
        // page: the row belongs to the sidebar toggle, history arrows and the
        // window's own caption buttons, whichever side they sit on.
        let mut root = div()
            .size_full()
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(theme.font_sans.clone())
            .child(self.header(&theme, cx));
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
        ProposalAction::StartSystem { system_id } => format!("Arrancar el sistema {system_id}"),
        ProposalAction::StopSystem { system_id } => format!("Parar el sistema {system_id}"),
    }
}

/// One proposal's text column; the caller appends its buttons or chip.
fn proposal_row(proposal: &TradingProposal, theme: &Theme) -> Div {
    let detail = match &proposal.result {
        Some(result) if proposal.status != ProposalStatus::Pending => result.clone(),
        _ => proposal.reason.clone(),
    };
    div()
        .px(px(12.0))
        .py(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .truncate()
                        .child(SharedString::from(proposal_title(&proposal.action))),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(SharedString::from(detail)),
                ),
        )
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
        .py(px(14.0))
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
