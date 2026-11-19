//! Repository artwork is fetched once per project/connection, off the render thread.
use super::*;
use crate::image_media::MediaImage;

// Curated badge tones: (dark appearance, light appearance). Keep the ordering
// stable so projects retain their assigned color. These are explicit colors,
// independent of the selected theme accent; only their appearance variant changes.
const MONOGRAM_PALETTE: [(u32, u32); 8] = [
    (0x94a3b8, 0x475569), // slate
    (0x93c5fd, 0x2563eb), // blue
    (0xc4b5fd, 0x7c3aed), // violet
    (0xfda4af, 0xbe123c), // rose
    (0xfcd34d, 0xa16207), // amber
    (0x6ee7b7, 0x047857), // emerald
    (0x5eead4, 0x0f766e), // teal
    (0xfdba74, 0xc2410c), // orange
];

fn monogram(name: &str, seed: &str, selected: bool, theme: &Theme) -> AnyElement {
    // FNV-1a selects a stable entry from the curated palette.
    let hash = seed.bytes().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16777619)
    });
    let letter = name
        .trim()
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let (dark, light) = MONOGRAM_PALETTE[hash as usize % MONOGRAM_PALETTE.len()];
    let tone = gpui::Hsla::from(gpui::rgb(if theme.appearance.is_dark() {
        dark
    } else {
        light
    }));
    let mut hover_text = tone;
    hover_text.l = if theme.appearance.is_dark() {
        (tone.l + 0.12).min(0.95)
    } else {
        (tone.l - 0.10).max(0.15)
    };
    let tile = div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        // The active row wears the hover tint permanently.
        .bg(tone.opacity(if selected { 0.24 } else { 0.08 }))
        .text_color(if selected {
            hover_text
        } else {
            tone.opacity(0.85)
        })
        .group_hover("sidebar-session-row", move |style| {
            style.bg(tone.opacity(0.24)).text_color(hover_text)
        })
        .font_family(theme.font_mono.clone())
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(
            div()
                .w_full()
                .text_center()
                .text_size(px(9.0))
                .line_height(px(13.0))
                .child(letter),
        );
    crate::frost::frosted(3.0, crate::frost::MENU_BLUR, tile).into_any_element()
}

pub(super) struct ProjectIcon {
    name: String,
    seed: String,
    media: Option<MediaImage>,
    refreshed: std::time::Instant,
    _task: Task<()>,
}

impl Render for ProjectIcon {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.media {
            Some(media) => gpui::img(media.image.clone())
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            None => monogram(&self.name, &self.seed, false, Theme::of(cx)),
        }
    }
}

/// Same card as the pull-request badge tooltip: project name, then the
/// owning device below in the muted line.
struct ProjectIconTooltip {
    name: SharedString,
    device: SharedString,
}

impl Render for ProjectIconTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let card = div()
            .max_w(px(320.0))
            .px(px(9.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(crate::popover::surface_bg(theme))
            .when(!theme.is_frost(), |el| el.shadow_md())
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(self.name.clone()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(self.device.clone()),
            );
        crate::frost::frosted(6.0, crate::frost::MENU_BLUR, card)
    }
}

fn project_icon_frame(
    chat_id: &str,
    name: &str,
    device: &str,
    size: f32,
    child: impl IntoElement,
) -> AnyElement {
    let name: SharedString = name.to_owned().into();
    let device: SharedString = device.to_owned().into();
    div()
        .id(SharedString::from(format!("project-icon-{chat_id}")))
        .size(px(size))
        .flex_none()
        .tooltip(move |_, cx| {
            cx.new(|_| ProjectIconTooltip {
                name: name.clone(),
                device: device.clone(),
            })
            .into()
        })
        .tooltip_show_delay(Duration::from_millis(350))
        .child(child)
        .into_any_element()
}

impl Shell {
    pub(super) fn render_project_icon(
        &self,
        chat_id: &str,
        size: f32,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let chat = state.chats.iter().find(|chat| chat.id == chat_id);
        // The tooltip names this row's own project; the artwork is shared.
        let name = chat
            .and_then(|chat| state.space_for_chat(chat))
            .map(|space| space.display_name().to_string())
            .unwrap_or_else(|| "No project".into());
        // Same fallback as the row's "@ device" fragment.
        let device = chat
            .and_then(|chat| state.device_name(&chat.device_id))
            .unwrap_or("Unknown device")
            .to_string();
        let art = self.project_icon_art(chat_id, selected, cx);
        project_icon_frame(chat_id, &name, &device, size, art)
    }

    /// A project group header's icon: the same artwork its rows would wear,
    /// without the tooltip — the label beside it already names the project.
    pub(super) fn render_project_group_icon(
        &self,
        chat_id: &str,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .size(px(size))
            .flex_none()
            .child(self.project_icon_art(chat_id, false, cx))
            .into_any_element()
    }

    /// Artwork for a chat's project, shared by every space of the project:
    /// the representative's name and color, read from this device's checkout
    /// when there is one (no round trip), else from the representative.
    fn project_icon_art(
        &self,
        chat_id: &str,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let space = state
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| state.space_for_chat(chat))
            .cloned();
        self.project_icons.render(
            &self.state,
            space.as_ref(),
            self.active_sidebar_pin_profile_key(cx),
            selected,
            cx,
        )
    }
}

/// Shared artwork pipeline for sidebar and repository picker. Each owner keeps
/// the entities alive; loading stays off the render thread and has a deadline.
#[derive(Default)]
pub(crate) struct ProjectIcons {
    entries: std::cell::RefCell<std::collections::HashMap<String, Entity<ProjectIcon>>>,
}

impl ProjectIcons {
    pub(crate) fn render<V: 'static>(
        &self,
        state: &Entity<AppState>,
        space: Option<&zeron_proto::Space>,
        profile: Option<String>,
        selected: bool,
        cx: &mut Context<V>,
    ) -> AnyElement {
        let state = state.read(cx);
        let representative = space.map(|space| state.representative_space(space));
        let name = representative
            .map(|space| space.display_name().to_string())
            .unwrap_or_else(|| "Home".into());
        let seed = representative
            .map(|space| space.path.clone())
            .unwrap_or_else(|| "home".into());
        // Aurea has no workspace files to read artwork from: the monogram.
        let _ = (profile, &self.entries);
        monogram(&name, &seed, selected, Theme::of(cx))
    }
}
