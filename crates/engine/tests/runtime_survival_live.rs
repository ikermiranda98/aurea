//! Opt-in real-model checks. A chat's agent process — and the background
//! work it holds — survives everything but the user's Stop: a follow-up with
//! an image, another client's spelling of the same options, a steer into a
//! long command, a model switch. Stop tears all of it down
//! (`stop_tears_down_all_agent_work`), and a resumed process never reads
//! as done before its reply.
//!
//! ZERON_TEST_HARNESS=claude ZERON_TEST_MODEL=claude-haiku-4-5 \
//!   cargo test -p zeron-engine --test runtime_survival_live -- --ignored --nocapture
//!
//! `ZERON_TEST_HARNESS`: claude | codex | cursor | opencode | devin | hermes |
//! pi | antigravity | grok. Harnesses whose agents can hold background work
//! across turns (claude, codex) must also finish that work after the cancel.
use std::{sync::Arc, time::Duration};

use zeron_doc::SessionCommandPayload;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{
    Harness, OpencodeHarness,
};
use zeron_proto::{AgentEvent, ChatConfig, RunRequest, SandboxLevel, SessionStatus};

const CHAT: &str = "survival";

fn processes(core: &EngineCore) -> usize {
    core.sessions
        .subscribe(CHAT, 0)
        .map(|(events, _)| {
            events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::SessionStarted { .. }))
                .count()
        })
        .unwrap_or(0)
}

fn completed_turns(core: &EngineCore) -> usize {
    core.sessions
        .subscribe(CHAT, 0)
        .map(|(events, _)| {
            events
                .iter()
                .filter(|e| {
                    matches!(
                        e.event,
                        AgentEvent::Done {
                            status: zeron_proto::DoneStatus::Completed,
                            ..
                        }
                    )
                })
                .count()
        })
        .unwrap_or(0)
}

fn status(core: &EngineCore) -> Option<SessionStatus> {
    core.sessions.session_status(CHAT).map(|s| s.status)
}

fn journal(core: &EngineCore) -> Vec<AgentEvent> {
    core.sessions
        .subscribe(CHAT, 0)
        .map(|(events, _)| {
            events
                .into_iter()
                .filter(|e| {
                    !matches!(
                        e.event,
                        AgentEvent::TextDelta { .. } | AgentEvent::ReasoningDelta { .. }
                    )
                })
                .map(|e| e.event)
                .collect()
        })
        .unwrap_or_default()
}

/// Waits for `predicate`, giving up early on an errored session.
async fn wait(core: &EngineCore, secs: u64, mut predicate: impl FnMut() -> bool, what: &str) {
    let errored = || status(core) == Some(SessionStatus::Errored);
    let settled = tokio::time::timeout(Duration::from_secs(secs), async {
        while !predicate() && !errored() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if settled.is_err() || errored() {
        eprintln!("status: {:?}; events: {:#?}", status(core), journal(core));
        core.shutdown().await;
        panic!("gave up waiting for {what}");
    }
}

fn run(
    prompt: &str,
    model: &Option<String>,
    harness: zeron_proto::HarnessId,
    cwd: &str,
) -> RunRequest {
    RunRequest {
        mcp: None,
        env: Default::default(),
        prompt: prompt.into(),
        harness: Some(harness),
        model: model.clone(),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        access: Default::default(),
        plan: false,
        attachments: vec![],
        worktree: None,
        resume: None,
        extra_mcp: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "uses real model quota; select harness and inexpensive model explicitly"]
async fn agent_process_survives_sends_and_cancel() {
    let name = std::env::var("ZERON_TEST_HARNESS").expect("select harness");
    let model = std::env::var("ZERON_TEST_MODEL").ok();
    let harness: Arc<dyn Harness> = match name.as_str() {
        "opencode" => Arc::new(OpencodeHarness::new()),
        _ => panic!("unknown harness"),
    };
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_owned();
    let id = harness.id();
    let registry = HarnessRegistry::new();
    registry.register(harness);
    let core =
        EngineCore::assemble(&dir.path().join("engine"), Arc::new(registry), id, None).unwrap();
    core.workspace
        .create_space("space", &core.device_id, &cwd, None, false)
        .unwrap();
    core.workspace
        .create_chat(CHAT, Some("space"), None, None, None)
        .unwrap();
    core.workspace
        .set_chat_config(
            CHAT,
            &ChatConfig {
                harness: id,
                model: model.clone(),
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::DangerFullAccess,
                access: Default::default(),
                plan: false,
            },
        )
        .unwrap();
    println!("{name}: workspace {cwd}");

    // 1. Background work that outlives the turn.
    let background = match name.as_str() {
        "claude" => Some(
            "This is an automated test in a disposable directory. Do exactly two things, then end your turn at once without waiting for either: \
             (1) use the Agent tool with run_in_background=true (subagent_type general-purpose, description 'sleeper') and the prompt \
             'Run this exact bash command and nothing else: sleep 75 && printf done > sub-done . Then reply DONE.'; \
             (2) use the Bash tool with run_in_background=true to run exactly: sleep 70 && printf done > bg-done . \
             Reply only LAUNCHED.",
        ),
        "codex" => Some(
            "This is an automated test in a disposable directory. Spawn one subagent (spawn_agent) whose task is: \
             'Run this exact shell command and nothing else: sleep 75 && printf done > sub-done . Then reply DONE.' \
             Do not wait for it and do not run the command yourself. Reply only LAUNCHED once it is spawned.",
        ),
        _ => None,
    };
    let opening = background.unwrap_or("This is an automated test. Reply only READY.");
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run(opening, &model, id, &cwd),
                message_id: "m-1".into(),
            },
        )
        .unwrap();
    wait(
        &core,
        240,
        || completed_turns(&core) >= 1 && status(&core) == Some(SessionStatus::Idle),
        "the opening turn",
    )
    .await;
    assert_eq!(processes(&core), 1);
    if background.is_some() {
        wait(
            &core,
            120,
            || {
                core.sessions.subscribe(CHAT, 0).is_ok_and(|(events, _)| {
                    events
                        .iter()
                        .any(|e| matches!(e.event, AgentEvent::Subagent { .. }))
                })
            },
            "the background subagent to start",
        )
        .await;
    }
    println!("{name}: opening turn parked");

    // 2. A follow-up from "another client": an image, and option values
    // spelled as strings (the phone's encoding).
    let image = dir.path().join("pixel.png");
    std::fs::write(
        &image,
        [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
    .unwrap();
    let mut follow_up = run(
        &format!(
            "Attached image: {} (a 1x1 test pixel). Do not use tools. Reply only SEEN.",
            image.display()
        ),
        &model,
        id,
        &cwd,
    );
    follow_up.attachments = vec![image.display().to_string()];
    follow_up.model_options = serde_json::Map::from_iter([(
        "unusedByAnyDriver".to_owned(),
        serde_json::Value::String(String::new()),
    )]);
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: follow_up,
                message_id: "m-2".into(),
            },
        )
        .unwrap();
    wait(
        &core,
        240,
        || completed_turns(&core) >= 2 && status(&core) == Some(SessionStatus::Idle),
        "the image follow-up",
    )
    .await;
    assert_eq!(processes(&core), 1, "the follow-up restarted the agent");
    println!("{name}: image follow-up joined the live process");

    // 2b. Steer a streaming turn into a long foreground command. The steer
    // ends the streaming reply (Claude's `now`), and until the command and
    // the reply after it finish the chat must stay Working with no
    // completion ping (2026-10-01: "marked done immediately", sound and
    // notification while the agent was still running the steered work).
    let pings = |core: &EngineCore| {
        core.sessions
            .session_status(CHAT)
            .and_then(|s| s.last_completed_turn)
    };
    let before_pings = pings(&core);
    let before_done = completed_turns(&core);
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run(
                    "Write a 600-word essay about rivers. Do not use tools.",
                    &model,
                    id,
                    &cwd,
                ),
                message_id: "m-2b".into(),
            },
        )
        .unwrap();
    wait(
        &core,
        240,
        || {
            status(&core) == Some(SessionStatus::Working)
                && core.sessions.subscribe(CHAT, 0).is_ok_and(|(events, _)| {
                    events
                        .iter()
                        .rev()
                        .take_while(|e| !matches!(e.event, AgentEvent::Done { .. }))
                        .filter(|e| matches!(&e.event, AgentEvent::TextDelta { .. }))
                        .count()
                        > 5
                })
        },
        "the essay to stream",
    )
    .await;
    core.sessions
        .dispatch(
            CHAT,
            id,
            run(
                "Stop the essay now. Run this exact shell command in the FOREGROUND and wait for it: sleep 12 && printf ok > steer-done . Then reply only STEERED.",
                &model,
                id,
                &cwd,
            ),
            Some("m-2b-steer".into()),
        )
        .await
        .unwrap();
    let steered_at = std::time::Instant::now();
    let mut premature = Vec::new();
    while !dir.path().join("steer-done").exists() {
        assert!(
            steered_at.elapsed() < Duration::from_secs(240),
            "the steered command never ran: {:#?}",
            journal(&core)
        );
        if status(&core) != Some(SessionStatus::Working) || pings(&core) != before_pings {
            premature.push((steered_at.elapsed(), status(&core), pings(&core)));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait(
        &core,
        240,
        || status(&core) == Some(SessionStatus::Idle) && completed_turns(&core) > before_done,
        "the steered turn to end",
    )
    .await;
    assert!(
        premature.is_empty(),
        "the steered turn read as done while its command ran: {premature:?}\n{:#?}",
        journal(&core)
    );
    assert_ne!(
        pings(&core),
        before_pings,
        "the steered turn's end pings once"
    );
    assert_eq!(processes(&core), 1, "the steer restarted the agent");
    println!("{name}: steer into a long command stayed Working until its real end");

    // 2c. Switch models on a follow-up: the live process adopts it.
    if let Ok(other) = std::env::var("ZERON_TEST_OTHER_MODEL") {
        let before = completed_turns(&core);
        core.doc_host
            .queue_command(
                CHAT,
                SessionCommandPayload::Run {
                    request: run(
                        "Do not use tools. Reply only SWITCHED.",
                        &Some(other.clone()),
                        id,
                        &cwd,
                    ),
                    message_id: "m-2c".into(),
                },
            )
            .unwrap();
        wait(
            &core,
            240,
            || completed_turns(&core) > before && status(&core) == Some(SessionStatus::Idle),
            "the model-switch follow-up",
        )
        .await;
        assert_eq!(processes(&core), 1, "the model switch restarted the agent");
        println!("{name}: switched to {other} in the live process");
    }

    // 3. The background work the opening turn launched ran to completion.
    if background.is_some() {
        let expect: &[&str] = if name == "claude" {
            &["sub-done", "bg-done"]
        } else {
            &["sub-done"]
        };
        wait(
            &core,
            240,
            || expect.iter().all(|f| dir.path().join(f).exists()),
            "the background work to finish",
        )
        .await;
        println!("{name}: background work finished: {expect:?}");
        assert_eq!(processes(&core), 1);
    }
    core.shutdown().await;
}
