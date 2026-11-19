//! The panel lifecycle through Shell's actions and actual GPUI layout.
use super::*;
use gpui::{AppContext, Bounds, TestAppContext, VisualTestContext};

fn setup(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
        let mut config = UiSettings::default();
        config.sidebar_width = 300.0;
        config.files_panel_width = 377.0;
        // The session card shifts the composer while it fits; these panels
        // measure the composer centered in its column.
        config.session_info_hidden = true;
        settings::init(config, dir.path(), cx);
    });
    let (shell, cx) = cx.add_window_view(|_, cx| {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.no_project = true;
            state.selected_chat = Some("a".into());
            state.chats = ["a", "b"]
                .into_iter()
                .map(|id| {
                    serde_json::from_value(serde_json::json!({
                        "id": id, "title": id, "deviceId": "local", "archived": false,
                        "createdAt": Utc::now(),
                    }))
                    .unwrap()
                })
                .collect();
            state
        });
        let mut shell = Shell::new(
            state,
            EngineBootConfig {
                data_dir: dir.path().into(),
                ipc_port: 0,
                edge_url: String::new(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            },
            cx,
        );
        shell.debug_gate = Some(GatePhase::Ready);
        shell.splash = SplashPhase::Gone;
        shell.active_chat = "a".into();
        shell.sidebar_opened_at = 1;
        shell.panel_open_sequence = 1;
        shell.test_time = Some(std::time::Instant::now());
        shell
    });
    cx.simulate_resize(gpui::size(px(1600.0), px(900.0)));
    (dir, shell, cx)
}

fn change(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
    action: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>),
) {
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            action(shell, window, cx);
            cx.notify();
        })
    });
}

enum Panel {
    Sidebar,
    Files,
    Right,
}

impl Panel {
    fn toggle(self, shell: &mut Shell, window: &mut Window, cx: &mut Context<Shell>) {
        match self {
            Self::Sidebar => shell.toggle_sidebar(cx),
            Self::Files => {
                let _ = window;
            }
            Self::Right => shell.toggle_right_pane(cx),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    // Sidebar, Files, surface host, visible conversation.
    widths: [f32; 4],
    composer: Bounds<Pixels>,
    strip: Option<Bounds<Pixels>>,
}

fn draw(shell: &Entity<Shell>, cx: &mut VisualTestContext, millis: u64) -> Frame {
    shell.update(cx, |shell, cx| {
        *shell.test_time.as_mut().unwrap() +=
            Duration::from_millis(millis).mul_f32(motion::speed_scale());
        cx.notify();
    });
    cx.update(|window, cx| window.draw(cx).clear());
    let mut width = |id| cx.debug_bounds(id).map_or(0.0, |b| f32::from(b.size.width));
    Frame {
        widths: [
            width("sidebar-column"),
            width("files-panel"),
            width("right-panel"),
            width("conversation-column"),
        ],
        composer: shell.read_with(cx, |s, cx| {
            s.composer.read(cx).surface_bounds().get().unwrap()
        }),
        strip: cx.debug_bounds("right-titlebar-controls"),
    }
}

#[track_caller]
fn near(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= 0.5,
        "expected {expected}px, got {actual}px"
    );
}

#[track_caller]
fn expect(frame: Frame, widths: [f32; 4]) {
    for (actual, expected) in frame.widths.into_iter().zip(widths) {
        near(actual, expected);
    }
}

#[track_caller]
fn continuous(before: Frame, after: Frame) {
    expect(after, before.widths);
    near(
        f32::from(after.composer.left()),
        f32::from(before.composer.left()),
    );
    near(
        f32::from(after.composer.size.width),
        f32::from(before.composer.size.width),
    );
    if let (Some(before), Some(after)) = (before.strip, after.strip) {
        near(f32::from(after.left()), f32::from(before.left()));
    }
}

fn settled(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Frame {
    // A closing column may make room for a hidden neighbour to return.
    draw(shell, cx, 600);
    draw(shell, cx, 600)
}

fn select(shell: &mut Shell, chat: Option<&str>, cx: &mut Context<Shell>) {
    let state = shell.state.clone();
    state.update(cx, |s, cx| s.select_chat(chat.map(str::to_owned), cx));
    shell.on_state_changed(&state, cx);
}

#[gpui::test]
fn side_chat_menu_rename_keeps_the_editor_visible_without_opening_files(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = setup(cx);
    change(&shell, cx, |shell, _, cx| {
        shell.state.update(cx, |state, cx| {
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "side", "title": "Side chat", "parentChatId": "a",
                    "deviceId": "local", "archived": false, "createdAt": Utc::now(),
                }))
                .unwrap(),
            );
            cx.notify();
        });
        shell.chat_menu.open(ChatMenuState {
            chat_id: "side".into(),
            tab: None,
            position: gpui::point(px(800.0), px(200.0)),
            page: ChatMenuPage::Root,
        });
    });
    settled(&shell, cx);
    assert!(shell.read_with(cx, |shell, cx| !shell.files_panel_open(cx)));

    let rename = cx.debug_bounds("chat-menu-rename").unwrap();
    cx.simulate_click(rename.center(), Default::default());
    settled(&shell, cx);

    shell.read_with(cx, |shell, cx| {
        assert!(
            !shell.files_panel_open(cx),
            "Rename must not open the explorer"
        );
        assert!(shell.session_info_visible(cx));
        assert_eq!(shell.chat_rename.as_ref().unwrap().chat_id, "side");
    });
    let field = cx.debug_bounds("session-lanes-chat-side-title-editor").unwrap();
    let card = cx.debug_bounds("session-lanes").unwrap();
    assert!(field.top() >= card.top() && field.bottom() <= card.bottom());
}

#[gpui::test]
fn side_chat_rename_leaves_takeover_to_show_its_editor(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = setup(cx);
    change(&shell, cx, |shell, _, cx| {
        shell.state.update(cx, |state, cx| {
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "side", "title": "Side chat", "parentChatId": "a",
                    "deviceId": "local", "archived": false, "createdAt": Utc::now(),
                }))
                .unwrap(),
            );
            cx.notify();
        });
        shell.toggle_right_pane(cx);
    });
    settled(&shell, cx);
    change(&shell, cx, |s, _, cx| s.toggle_right_pane_expand(cx));
    near(settled(&shell, cx).widths[3], 0.0);

    change(&shell, cx, |s, _, cx| s.open_rename_chat("side".into(), cx));
    let frame = settled(&shell, cx);
    assert!(!shell.read_with(cx, |s, _| s.right_pane_expanded));
    assert!(frame.widths[3] > 0.0, "the transcript column must return");
    let field = cx.debug_bounds("session-lanes-chat-side-title-editor").unwrap();
    let card = cx.debug_bounds("session-lanes").unwrap();
    assert!(field.top() >= card.top() && field.bottom() <= card.bottom());
}

fn check_side_chat_rename_scroll(cx: &mut TestAppContext, width: f32, reduced_motion: bool) {
    let (_dir, shell, cx) = setup(cx);
    cx.simulate_resize(gpui::size(px(width), px(900.0)));
    cx.update(|_, cx| motion::set_reduced_motion(cx, reduced_motion));
    change(&shell, cx, |shell, _, cx| {
        shell.state.update(cx, |state, cx| {
            let now = Utc::now();
            for index in 0..40 {
                state.chats.push(
                    serde_json::from_value(serde_json::json!({
                        "id": format!("side-{index}"), "title": format!("Side {index}"),
                        "parentChatId": "a", "deviceId": "local", "archived": false,
                        "createdAt": now - chrono::Duration::minutes(index),
                    }))
                    .unwrap(),
                );
            }
            cx.notify();
        });
    });
    settled(&shell, cx);
    assert!(cx.debug_bounds("session-lanes").is_none());

    // Reveal a paged-out row from a hidden card, then a row above the new
    // scroll position. Both directions must leave the whole editor visible.
    for (chat, selector) in [
        ("side-39", "session-lanes-chat-side-39-title-editor"),
        ("side-0", "session-lanes-chat-side-0-title-editor"),
    ] {
        change(&shell, cx, |shell, _, cx| {
            shell.open_rename_chat(chat.into(), cx);
        });
        settled(&shell, cx);
        for _ in 0..4 {
            cx.run_until_parked();
            draw(&shell, cx, 100);
        }
        let field = cx.debug_bounds(selector).unwrap();
        let card = cx.debug_bounds("session-lanes").unwrap();
        assert!(
            field.top() >= card.top() && field.bottom() <= card.bottom(),
            "{chat}'s editor {field:?} is outside the card {card:?}"
        );
        cx.update(|window, cx| {
            shell.read_with(cx, |shell, cx| {
                let rename = shell.chat_rename.as_ref().unwrap();
                assert_eq!(rename.chat_id, chat);
                assert!(rename.input.focus_handle(cx).is_focused(window));
            });
        });
    }
}

#[gpui::test]
fn side_chat_rename_scrolls_to_its_row_in_a_docked_card(cx: &mut TestAppContext) {
    check_side_chat_rename_scroll(cx, 1600.0, false);
}

#[gpui::test]
fn side_chat_rename_scrolls_to_its_row_in_a_floating_card_without_motion(cx: &mut TestAppContext) {
    check_side_chat_rename_scroll(cx, 1000.0, true);
}

#[gpui::test]
fn the_session_card_lands_with_a_chat_switch_and_eases_on_its_toggle(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = setup(cx);
    change(&shell, cx, |s, _, _| {
        s.settings.session_info_hidden = false;
        // b's surface host leaves its transcript no room for the card.
        s.panels.update("b", |p| p.changes_open = true);
    });
    settled(&shell, cx);
    let card = cx
        .debug_bounds("session-info")
        .expect("a's wide column shows the card");
    // Where the card's reveal is easing to, if it eases at all.
    let easing_to = |cx: &mut VisualTestContext| {
        shell.read_with(cx, |s, _| s.session_info_tween.map(|tween| tween.to))
    };

    // The panels snap to b's; the card must not fade out over b's transcript.
    change(&shell, cx, |s, _, cx| select(s, Some("b"), cx));
    draw(&shell, cx, 0);
    assert!(cx.debug_bounds("session-info").is_none());
    assert_eq!(easing_to(cx), None);

    // Back on a, it is at rest from the first frame.
    change(&shell, cx, |s, _, cx| select(s, Some("a"), cx));
    draw(&shell, cx, 0);
    let landed = cx
        .debug_bounds("session-info")
        .expect("a shows its card from the first frame");
    near(f32::from(landed.left()), f32::from(card.left()));
    assert_eq!(easing_to(cx), None);

    // Its own toggle still eases it out, then back in.
    for (reveal, shown) in [(0.0, false), (1.0, true)] {
        change(&shell, cx, |s, _, cx| s.toggle_session_info(cx));
        draw(&shell, cx, 0);
        assert_eq!(easing_to(cx), Some(reveal));
        settled(&shell, cx);
        assert_eq!(cx.debug_bounds("session-info").is_some(), shown);
    }
}
