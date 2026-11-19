//! Session-owned explorer chrome, independent of the surface tab host.

use super::*;
use crate::settings::{FILES_PANEL_MAX, FILES_PANEL_MIN};

pub(super) struct FilesPanelResize;

/// Files kept per chat for the command palette's Recent Files.
const RECENT_FILES_PER_CHAT: usize = 20;

impl Shell {
}

/// Allocate a real column to Files, reducing its preferred width before the
/// chat or surface minima. The shell fit policy hides a whole column when its
/// minimum cannot fit, so no settled column is rendered as a sliver.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FilesPanelLayout {
    width: f32,
    surface_max: f32,
}

/// `surface_reveal` is how far the surface host is open, from 0 to 1. Files
/// yields the host's minimum in step with its open/close tween rather than
/// all at once when the flag flips.
fn files_panel_layout(
    viewport: f32,
    sidebar: f32,
    preferred: f32,
    visible: f32,
    surface_reveal: f32,
    expanded: bool,
) -> FilesPanelLayout {
    let surfaces_open = surface_reveal > 0.0;
    let available = (viewport - sidebar).max(0.0);
    // Takeover gives the conversation's space to the surface host. It must
    // not also widen Files: that would move the seam before the right tween
    // has painted its first frame, and move it back again on reversal.
    let surface_chat_min = if surfaces_open && expanded {
        0.0
    } else {
        CHAT_PANEL_MIN
    };
    let surface_min = RIGHT_PANE_MIN * surface_reveal.clamp(0.0, 1.0);
    let max_width = if preferred > 0.0 {
        (available - CHAT_PANEL_MIN - surface_min).max(0.0)
    } else {
        0.0
    };
    let width = visible.max(0.0).min(max_width);
    FilesPanelLayout {
        width,
        surface_max: right_pane_max_width(viewport - width, sidebar, surface_chat_min),
    }
}

impl Shell {
    pub(super) fn files_panel_open(&self, cx: &App) -> bool {
        // Aurea drops the workspace file explorer: it never opens.
        let _ = cx;
        false
    }

    fn files_layout(&self, visible: f32, cx: &App) -> FilesPanelLayout {
        let fit = self.horizontal_fit();
        files_panel_layout(
            self.viewport_width,
            self.sidebar_now(),
            if fit.files && (self.files_panel_open(cx) || self.tween_active(self.files_tween)) {
                self.settings.files_panel_width
            } else {
                0.0
            },
            if fit.files { visible } else { 0.0 },
            if fit.right {
                self.right_reveal()
            } else {
                // A host the fit is hiding still occupies its exit width.
                self.fit_exit_width(1)
                    .map_or(0.0, |width| (width / RIGHT_PANE_MIN).clamp(0.0, 1.0))
            },
            self.right_pane_expanded && fit.right,
        )
    }

    /// How far the surface host is open: its open/close tween's progress,
    /// 1 at rest while open. Width-only tweens between open sizes stay at 1.
    fn right_reveal(&self) -> f32 {
        // What the host actually occupies, up to its minimum: continuous
        // through reversals, which restart the tween from a partial width.
        match self
            .right_tween
            .filter(|tween| self.tween_active(Some(*tween)))
        {
            Some(tween) => {
                (self.eval_tween(Some(tween), tween.to) / RIGHT_PANE_MIN).clamp(0.0, 1.0)
            }
            None => 1.0,
        }
    }

    pub(super) fn files_target(&self, cx: &App) -> f32 {
        self.files_layout(
            if self.files_panel_open(cx) {
                self.settings.files_panel_width
            } else {
                0.0
            },
            cx,
        )
        .width
    }

    pub(super) fn files_visible_width(&self, cx: &App) -> f32 {
        if !matches!(self.route, Route::Chat) || self.active_chat.is_empty() {
            return 0.0;
        }
        if !self.horizontal_fit().files
            && let Some(exiting) = self.fit_exit_width(2)
        {
            // Exits clamp in layout order so the columns never overflow.
            return exiting.min((self.viewport_width - self.sidebar_now()).max(0.0));
        }
        self.files_layout(
            self.ease_toward(self.files_tween, self.files_target(cx)),
            cx,
        )
        .width
    }

    pub(super) fn files_reserved_width(&self, cx: &App) -> f32 {
        self.files_visible_width(cx)
    }

    /// Files' width once every running open/close has landed. Initial surface
    /// targets use this allocation; painted widths follow the live layout.
    pub(super) fn files_settled_width(&self, cx: &App) -> f32 {
        let (fit, sidebar) = self.settled_fit();
        self.files_width_for(fit, sidebar, cx)
    }

    /// Files' width for a given fit and sidebar width: its own width, as far
    /// as the conversation and the surface host's minimums leave room.
    pub(super) fn files_width_for(&self, fit: HorizontalPanelFit, sidebar: f32, cx: &App) -> f32 {
        let preferred = if fit.files && self.files_panel_open(cx) {
            self.settings.files_panel_width
        } else {
            0.0
        };
        let right = fit.right && self.right_pane_open(cx);
        files_panel_layout(
            self.viewport_width,
            sidebar,
            preferred,
            preferred,
            if right { 1.0 } else { 0.0 },
            self.right_pane_expanded && right,
        )
        .width
    }

    pub(super) fn surface_max_width(&self, cx: &App) -> f32 {
        self.files_layout(self.files_visible_width(cx), cx)
            .surface_max
    }

    pub(super) fn right_visible_width(&self, cx: &App) -> f32 {
        if !self.horizontal_fit().right {
            let available =
                (self.viewport_width - self.sidebar_now() - self.files_visible_width(cx)).max(0.0);
            return self
                .fit_exit_width(1)
                .map_or(0.0, |width| width.min(available));
        }
        let available =
            (self.viewport_width - self.sidebar_now() - self.files_visible_width(cx)).max(0.0);
        // Leaving takeover returns the conversation's width over the same
        // tween. Applying its settled minimum here jumps the seam by 300px
        // before that first frame (and breaks mid-flight reversals).
        let max = if self.tween_active(self.main_takeover_tween) {
            available
        } else {
            self.surface_max_width(cx)
        };
        self.right_now(max, cx).min(available)
    }

    pub(super) fn files_content_width(&self, cx: &App) -> f32 {
        self.column_content_width(
            (2, self.horizontal_fit().files),
            self.files_tween,
            self.files_content_tween,
            self.files_target(cx),
        )
    }

    /// Ease Files from what it painted (`from`, its content laid out at
    /// `content`) to `to`, both read before the action changed anything.
    pub(super) fn transition_files(&mut self, from: f32, content: f32, to: f32) {
        let (content_from, content_to) = panel_content_tween(content, from, to);
        let mask = self.panel_tween(from, to);
        self.files_tween = Some(mask);
        self.files_content_tween = Some(WidthTween {
            from: content_from,
            to: content_to,
            ..mask
        });
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[test]
    fn files_layout_shrinks_the_tree_before_the_chat_or_editor() {
        let docked = files_panel_layout(1400.0, 256.0, 286.0, 286.0, 1.0, false);
        assert_eq!(
            docked,
            FilesPanelLayout {
                width: 286.0,
                surface_max: 558.0,
            }
        );
        let narrow = files_panel_layout(1200.0, 256.0, 440.0, 440.0, 1.0, false);
        assert_eq!(
            narrow,
            FilesPanelLayout {
                width: 284.0,
                surface_max: 360.0,
            }
        );
        assert_eq!(
            1200.0 - 256.0 - narrow.width - narrow.surface_max,
            CHAT_PANEL_MIN
        );
        // Closing the surface releases space for the tree. Expanding the
        // surface keeps the tree's existing allocation stable.
        assert_eq!(
            files_panel_layout(1200.0, 256.0, 440.0, 440.0, 0.0, false).width,
            440.0
        );
        let expanded = files_panel_layout(1100.0, 256.0, 286.0, 286.0, 1.0, true);
        assert_eq!(expanded.width, 184.0);
        assert_eq!(expanded.surface_max, 660.0);
        // Growing the viewport restores the preferred width.
        assert_eq!(
            files_panel_layout(1600.0, 256.0, 440.0, 440.0, 1.0, false).width,
            440.0
        );
    }

    #[test]
    fn files_layout_returns_space_smoothly_during_close() {
        let mut previous_chat = 0.0;
        for visible in [186.0, 140.0, 84.0, 40.0, 0.0] {
            let layout = files_panel_layout(1200.0, 256.0, 286.0, visible, 1.0, false);
            let chat = 944.0 - layout.width - layout.surface_max;
            assert!(chat >= previous_chat && chat >= CHAT_PANEL_MIN);
            previous_chat = chat;
        }
        assert_eq!(
            files_panel_layout(1200.0, 256.0, 286.0, 0.0, 1.0, false),
            files_panel_layout(1200.0, 256.0, 0.0, 0.0, 1.0, false),
        );
    }

    #[test]
    fn files_layout_never_takes_space_reserved_for_the_chat_or_surface() {
        for viewport in [0.0, 120.0, 280.0, 600.0, 1000.0, 1200.0, 1600.0] {
            for sidebar in [0.0, 256.0, 400.0] {
                for surfaces in [false, true] {
                    for expanded in [false, true] {
                        for visible in [0.0, 1.0, 140.0, 440.0] {
                            let layout = files_panel_layout(
                                viewport,
                                sidebar,
                                440.0,
                                visible,
                                if surfaces { 1.0 } else { 0.0 },
                                expanded,
                            );
                            let available = (viewport - sidebar).max(0.0);
                            assert!(layout.width >= 0.0 && layout.width <= visible);
                            assert!(layout.surface_max >= 0.0);
                            assert!(layout.width + layout.surface_max <= available + 0.001);
                            if available
                                >= CHAT_PANEL_MIN + if surfaces { RIGHT_PANE_MIN } else { 0.0 }
                            {
                                assert!(
                                    available - layout.width - layout.surface_max >= CHAT_PANEL_MIN
                                        || surfaces && expanded
                                );
                            }
                        }
                    }
                }
            }
        }
    }

}

