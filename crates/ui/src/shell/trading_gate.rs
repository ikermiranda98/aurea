//! B7 wiring: the close gate between an exit request and the window going away.
//!
//! [`Shell::prepare_exit`] asks [`Shell::trading_gate_allows`] after the
//! unsaved-file check. The first ask starts one `TradingSnapshot` read (raced
//! against [`close_gate::TIMEOUT`]) and holds the exit; the answer either
//! clears it silently or stages the warning dialog. Approving re-arms
//! `pending_exit`, so the shell's render loop replays the exit exactly the way
//! it replays one held by an unsaved file.

use futures::FutureExt;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use serde_json::json;
use zeron_proto::trading::Snapshot;
use zeron_rpc::methods;

use super::{PendingExit, Shell};
use crate::popover;
use crate::theme::Theme;
use crate::trading::close_gate::{self, Verdict};

/// Where the gate stands for this window.
#[derive(Debug, Clone, Default)]
pub(super) enum TradingGate {
    #[default]
    Idle,
    /// The snapshot read is in flight; further exits wait for it.
    Checking,
    /// The warning is on screen for `action`.
    Asking {
        action: PendingExit,
        title: String,
        detail: String,
    },
}

impl Shell {
    /// `true` when `action` may proceed now. Only exits that take the desk
    /// away from the user are gated: closing the window and quitting.
    pub(super) fn trading_gate_allows(&mut self, action: PendingExit, cx: &mut Context<Self>) -> bool {
        if !matches!(action, PendingExit::CloseWindow | PendingExit::Quit) {
            return true;
        }
        if close_gate::take_approval() {
            self.trading_gate = TradingGate::Idle;
            return true;
        }
        // No engine at all: nothing of ours runs to be orphaned.
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return true;
        };
        if !matches!(self.trading_gate, TradingGate::Idle) {
            return false;
        }
        self.trading_gate = TradingGate::Checking;
        let timer = cx.background_executor().timer(close_gate::TIMEOUT);
        cx.spawn(async move |this, cx| {
            let call = engine
                .client()
                .call(methods::TRADING_SNAPSHOT, json!({}))
                .fuse();
            let timer = timer.fuse();
            futures::pin_mut!(call, timer);
            let answer = futures::select! {
                reply = call => reply
                    .ok()
                    .and_then(|value| serde_json::from_value::<Snapshot>(value).ok()),
                _ = timer => None,
            };
            let verdict = close_gate::verdict(answer.as_ref());
            let _ = this.update(cx, |shell, cx| shell.trading_gate_answered(action, verdict, cx));
        })
        .detach();
        cx.notify();
        false
    }

    fn trading_gate_answered(&mut self, action: PendingExit, verdict: Verdict, cx: &mut Context<Self>) {
        match verdict {
            Verdict::Clear => self.trading_gate_proceed(action, cx),
            Verdict::Warn { title, detail } => {
                self.trading_gate = TradingGate::Asking {
                    action,
                    title,
                    detail,
                };
                cx.notify();
            }
        }
    }

    /// Lets the held exit through: one approval, replayed by the render loop.
    fn trading_gate_proceed(&mut self, action: PendingExit, cx: &mut Context<Self>) {
        close_gate::approve();
        self.trading_gate = TradingGate::Idle;
        self.pending_exit = Some(action);
        cx.notify();
    }

    fn trading_gate_cancel(&mut self, cx: &mut Context<Self>) {
        close_gate::revoke();
        self.trading_gate = TradingGate::Idle;
        cx.notify();
    }

    /// The warning dialog. Its safe default is to stay open: Cancel comes
    /// first and the destructive choice is spelled out.
    pub(super) fn render_trading_gate(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let TradingGate::Asking {
            action,
            title,
            detail,
        } = &self.trading_gate
        else {
            return None;
        };
        let confirm = if matches!(action, PendingExit::Quit) {
            "Salir igualmente"
        } else {
            "Cerrar igualmente"
        };
        let action = action.clone();
        let theme = Theme::of(cx).for_popup();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, detail.clone())))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancelar", "trading-gate-cancel")
                            .id("trading-gate-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.trading_gate_cancel(cx))),
                    )
                    .child(
                        popover::btn_danger(&theme, confirm)
                            .id("trading-gate-confirm")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.trading_gate_proceed(action.clone(), cx)
                            })),
                    ),
            )
            .into_any_element();
        Some(popover::modal("trading-gate-dialog", viewport, card))
    }
}
