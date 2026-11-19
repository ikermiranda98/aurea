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

impl Shell {
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
