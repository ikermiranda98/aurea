//! The tool catalog and its dispatch.
//!
//! Every tool is a thin composition of engine reads/writes from
//! [`Zeron`]; the only logic that lives here is argument resolution (chat
//! by prefix, project by path), sender attribution, and the "how do I
//! deliver a message to a chat in this state" choice the composer makes
//! for humans.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::{SessionCommandPayload, TranscriptWindow};
use zeron_proto::{
    AccessMode, Chat, ChatConfig, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, Session, SessionStatus,
    Space, UserInputAnswer,
};

use crate::transcript::{RenderOptions, RenderedMessage, render_entries};
use crate::zeron::{HarnessInfo, TranscriptFollow, TurnOutcome, Zeron, session_for, short};

mod plugins;
mod themes;
pub use plugins::ListedTool;
pub(crate) use plugins::is_plugin_tool_name;

/// Default and ceiling for the blocking waits.
const MAX_BATCH: usize = 32;
/// Cloud runs these agents (Codex and Claude Code): a Cloud chat's harness.
const CLOUD_HARNESSES: &[HarnessId] = &[HarnessId::ClaudeCode, HarnessId::Codex];
const DEFAULT_WAIT: Duration = Duration::from_secs(600);
const MAX_WAIT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct Tools {
    zeron: Arc<Zeron>,
    // Remember successful sends on this MCP connection so wait_for_turn after
    // wait:false also waits for a newly created chat with no session row yet.
    pending_turns: tokio::sync::Mutex<HashMap<String, Arc<PendingTurn>>>,
}

fn chat_key_schema(extra: Value) -> Value {
    let mut properties = json!({
        "chat": {
            "type": "string",
            "description": "Chat id, unique id prefix, or exact title."
        }
    });
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": ["chat"] })
}

const DEVICE_STATS: &str = "device_stats";

fn device_schema() -> Value {
    json!({ "type": "string", "description": "Host device id or exact name. Omit for the local engine." })
}

fn catalog() -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "whoami",
            description: "Which chat and device this server speaks for, plus the engine's workspace mode. Call this first when you need to know your own chat id.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_devices",
            description: "Devices in this workspace (the local engine's device is flagged). Chats and projects are hosted on a device.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: DEVICE_STATS,
            description: "One device's machine stats, asked live of that device (local when omitted): OS (name, version, kernel, architecture), CPU (model, physical and logical cores), memory (total and available bytes), storage (the volumes holding Zeron's data and the home folder, plus every local volume: mount point, filesystem, total/available/used bytes), GPU names where the platform says them cheaply, uptime, load average (Unix), battery, and the Zeron engine version. Any field is null when that machine can't say. Another device answers within about 5 s, or the error says it's offline (with when it was last seen) or its Zeron is too old. Reports are cached for 5 s.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_projects",
            description: "Projects: folders on specific devices. Filter by device (id or name); omit for all projects. Use ids when names or paths repeat.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_harnesses",
            description: "Agent harnesses (claude-code, codex, cursor, …) on the chosen device (local when omitted). `connected` = installed, enabled and not known to be signed out: those are the providers you can delegate to, whichever provider you are. `signedIn` is null when the login state is unknown.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_models",
            description: "Models a harness offers on the chosen device (local engine when omitted). Model ids are harness-specific strings; pass one to create_chat.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "harness": { "type": "string", "description": "Harness id, e.g. claude-code or codex." },
                    "device": device_schema()
                },
                "required": ["harness"]
            }),
        },
        ToolDef {
            name: "list_chats",
            description: "Chats with project, device, harness/model, sidebar status, and last activity. Status is working, awaitingInput (needs input), completed (unseen result), errored, or idle. statusStale marks expired live reports; terminalActivity explains OSC 7501 reports. sessionStatus is native execution state. Newest activity first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Only chats in this project (id, path, or name)." },
                    "device": { "type": "string", "description": "Only chats hosted on this device (id or name)." },
                    "include_archived": { "type": "boolean", "default": false },
                    "parent": { "type": "string", "description": "Only chats created by this chat (id, prefix, or title) — e.g. your own id to list the chats you spawned." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                }
            }),
        },
        ToolDef {
            name: "get_chat",
            description: "One chat's metadata, live status, and any question its agent is currently blocked on.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "create_chat",
            description: "Create a child of your chat to delegate work to (kind subagent, the default when you speak from a chat or pass parent: listed under that chat's Subagents and in its transcript's subagent block), a side chat (kind side: only when the person should treat it as a side conversation of their own), or a standalone session (kind chat, no parent). Any connected provider works (see list_harnesses), whichever provider you run on. Delegation is one level deep: a chat that has a parent cannot create chats. With cloud, the chat runs on a Cloud machine of its own (Codex or Claude Code), from a Cloud chat or a project with a GitHub repository. Select a project and/or device; catalogs are validated on that host. Optionally send a first prompt and wait for the reply. For parallel delegation use create_chats, or launch with wait=false and wait only after all chats have started.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["subagent", "side", "chat"], "description": "subagent (default with a parent or origin chat): delegated work, listed under the parent's Subagents. side: a side conversation for the person, needs a parent or origin. chat: standalone, no parent allowed. Omitted: subagent with a parent/origin, chat otherwise." },
                    "project": { "type": "string", "description": "Project id, path, or name. Belongs to a specific device; when device is supplied, search only there. Optional." },
                    "device": { "type": "string", "description": "Host device (id or name). With project, it must own that project. Without either, use the local engine." },
                    "parent": { "type": "string", "description": "Parent chat to record (id, prefix, or title). Defaults to the chat you are speaking from." },
                    "harness": { "type": "string", "description": "Harness id (see list_harnesses with the chosen device). Prefers a connected provider, claude-code first." },
                    "model": { "type": "string", "description": "Model id from list_models on the chosen device. Omit for the harness default." },
                    "reasoning": { "type": "string", "description": "Reasoning level the model supports (e.g. low, medium, high, max)." },
                    "sandbox": { "type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"], "default": "workspace-write", "description": "Never wider than the sandbox of the chat you are speaking from: more is lowered to it (accessCapped)." },
                    "access": { "type": "string", "enum": ["supervised", "auto-accept-edits", "auto", "full-access"], "description": "What the agent may do without asking: supervised asks before commands and file changes, auto-accept-edits applies edits and asks before the rest, auto lets the agent's own reviewer (Claude Code's auto mode, Codex's auto-review) approve safe actions and ask about the rest, full-access asks nothing. Questions go to the person in the chat. Must be one of the harness's accessModes (list_harnesses). Defaults to the access of the chat you are speaking from, and never exceeds it: more is lowered to it (the reply's accessCapped says so)." },
                    "plan": { "type": "boolean", "default": false, "description": "Plan mode: explore and propose a plan before changing anything. Only for harnesses with supportsPlan." },
                    "title": { "type": "string", "description": "Sidebar title. Otherwise the engine titles it from the first exchange." },
                    "branch": { "type": "string", "description": "Branch label to record on the chat." },
                    "cwd": { "type": "string", "description": "Working directory override (an existing worktree path). Defaults to the project folder." },
                    "cloud": { "type": "boolean", "default": false, "description": "Run on a Cloud machine of its own (cloning the project's GitHub repository) instead of the project's device. From a Cloud chat, the child clones the same repository; without cloud it shares this machine." },
                    "prompt": { "type": "string", "description": "First message to send right away." },
                    "wait": { "type": "boolean", "default": false, "description": "With prompt: block until the first turn finishes and return the reply." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                }
            }),
        },
        ToolDef {
            name: "read_chat",
            description: "Read a chat's transcript as plain messages (newest window by default). Tool calls are summarized one per line.",
            input_schema: chat_key_schema(json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 40, "description": "How many messages, counted from the newest." },
                "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many newest messages (paging backwards)." },
                "include_reasoning": { "type": "boolean", "default": false },
                "include_tools": { "type": "boolean", "default": true }
            })),
        },
        ToolDef {
            name: "send_message",
            description: "Send a message to a chat. Messages are attributed to your chat. mode 'auto' starts a turn when the chat is idle, steers a running turn through its live mailbox (at the next supported input boundary without interrupting the agent). Use mode 'queue' only to explicitly hold a message for later. With wait=true, blocks until the turn finishes and returns the assistant's reply. For parallel work use send_messages, or send to all chats with wait=false before waiting.",
            input_schema: chat_key_schema(json!({
                "text": { "type": "string" },
                "mode": { "type": "string", "enum": ["auto", "run", "steer", "queue"], "default": "auto" },
                "wait": { "type": "boolean", "default": false },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "wait_for_turn",
            description: "Wait for completed, awaitingInput, interrupted, errored, or timedOut. Native pendingInput can be answered with respond_to_input; terminalActivity input must be handled in its terminal. Stale/offline activity is not completion. Returns assistant replies when available.",
            input_schema: chat_key_schema(json!({
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "interrupt_chat",
            description: "Stop the chat's running turn.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "respond_to_input",
            description: "Answer a question the chat's agent is blocked on (see get_chat / wait_for_turn pendingInput). Labels are the option strings, or free text for open questions.",
            input_schema: chat_key_schema(json!({
                "request_id": { "type": "string", "description": "The pending request id. Defaults to the chat's current pending question." },
                "answers": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question_id": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["question_id", "labels"]
                    }
                }
            })),
        },
        ToolDef {
            name: "archive_chat",
            description: "Archive a chat (hide it from the active sidebar list); pass archived=false to restore. Use it to tidy up chats you created.",
            input_schema: chat_key_schema(json!({
                "archived": { "type": "boolean", "default": true }
            })),
        },
    ];
    for (name, single, description) in [
        (
            "create_chats",
            "create_chat",
            "Create multiple chats concurrently: subagents of your chat by default, or side/standalone chats (kind per request, as create_chat). Put each chat's prompt in its request to start all work together. Prefer this for parallel delegation, including harnesses that execute tool calls sequentially. Each request has create_chat arguments; wait defaults to false. Results preserve request order and include per-request errors; successful requests are not rolled back.",
        ),
        (
            "send_messages",
            "send_message",
            "Send messages to multiple independent chats concurrently. Prefer this to delegate parallel work to existing chats. Each request has send_message arguments; wait defaults to false. If wait=true, requests still run concurrently. Results preserve request order and include per-request errors; successful sends are not rolled back. Do not include dependent messages to the same chat.",
        ),
    ] {
        let item_schema = tools
            .iter()
            .find(|tool| tool.name == single)
            .unwrap()
            .input_schema
            .clone();
        tools.push(ToolDef {
            name, description,
            input_schema: json!({
                "type": "object",
                "properties": {"requests": {"type": "array", "minItems": 1, "maxItems": MAX_BATCH, "items": item_schema}},
                "required": ["requests"],
            }),
        });
    }
    tools.push(ToolDef {
        name: SHOW_VISUAL,
        description: SHOW_VISUAL_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The Luau visual script: setup at the top level, ending with `return function() return ui.column { ... } end`."
                },
                "title": { "type": "string", "description": "Short name for the visual." },
                "assets": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Files the script uses, by the name it uses them: { \"prism.glb\": \"models/prism.glb\", \"sky.hdr\": \"/tmp/sky.hdr\" }. Paths are on this machine, relative to your working directory. glTF models (.glb, or .gltf with everything embedded), PNG, JPEG, WebP, and Radiance .hdr images; at most 16 files, 8 MB each, 10 MB together."
                }
            },
            "required": ["script", "title"]
        }),
    });
    tools.push(ToolDef {
        name: SHOW_IMAGE,
        description: SHOW_IMAGE_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The image file on this machine, absolute or relative to your working directory."
                }
            },
            "required": ["path"]
        }),
    });
    tools.push(ToolDef {
        name: WAIT_ON_BACKGROUND,
        description: WAIT_ON_BACKGROUND_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "The background tasks' ids, as your tools reported them."
                },
                "waiting": {
                    "type": "boolean",
                    "description": "false takes the marks back. Default true."
                }
            },
            "required": ["task_ids"]
        }),
    });
    tools.extend(trading_catalog());
    tools.extend(themes::catalog());
    tools
}

const TRADING_SNAPSHOT_TOOL: &str = "trading_snapshot";
const TRADING_BRIEF_TOOL: &str = "trading_brief";
const TRADING_PROPOSE_TOOL: &str = "trading_propose";
const TRADING_SELECT_TOOL: &str = "trading_select";
const TRADING_SCHEDULE_TOOL: &str = "trading_schedule";
const TRADING_WATCH_TOOL: &str = "trading_watch";
const TRADING_UNWATCH_TOOL: &str = "trading_unwatch";

/// Aurea's trading desk: read freely, ask for money actions.
fn trading_catalog() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TRADING_SNAPSHOT_TOOL,
            description: "Aurea's trading desk right now: which accounts are connected (MetaTrader 5 and/or a TWS paper link; with none connected the desk is empty, never made-up data), the shown account (equity, cash, P&L of the day), open positions (symbol, side, size, entry, current price, P&L) plus how many are open across every account, the bot systems (running or not, symbol, P&L, the capital the person assigned to each one and that P&L as a percent of it, when each one last wrote to its log, and their own idea and rules when the folder has them), scheduled tasks, watch rules (`rules`: condition, whether it proposes something, status and when it fired or expires), connections, the newest engine log rows, the proposals still waiting (`pendingProposals`) and the last answered ones (`recentProposals`). Read-only; call it before answering about positions, before proposing anything, and when the person asks how a proposal ended or what a rule saw.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "logs": { "type": "integer", "minimum": 0, "maximum": 100, "description": "How many recent log rows to include (default 20)." }
                }
            }),
        },
        ToolDef {
            name: TRADING_BRIEF_TOOL,
            description: "A short spoken summary of the desk's day, ready to read to the person: the shown account and its P&L, how many bots are running and the best and worst, the open positions and their P&L, the tasks still pending (and today's run, missed and failed ones), the watch rules (how many are watching and how many fired or expired today), the requests still waiting plus today's answered ones with their outcome, and today's warnings and errors from the engine log. Read-only, no arguments. Use it for \"¿cómo va el día?\", \"resumen\", \"¿qué ha pasado hoy?\", \"¿hay algo pendiente?\" instead of dumping trading_snapshot. For anything that needs exact numbers or ids, use trading_snapshot or trading_select.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: TRADING_SELECT_TOOL,
            description: "Pick, from the live desk, the bot systems that match what the person said: by symbol, running or stopped, P&L of the day in percent of their capital or in dollars, and minutes without activity. Read-only and deterministic: use it instead of eyeballing numbers, and never invent system ids. It answers `matched` (each system's `id`, `name`, symbol, running, P&L and a `why` line) and `count`; pass those `id`s together as `system_ids` to one `trading_propose` (start_systems / stop_systems) so a single card covers them all. Call it for groups like \"los que pierden más de 1 %\", \"los de EURUSD\", \"los parados\", \"los que llevan una hora sin hacer nada\". With no filters it lists every system. Systems without capital are left out of the percent filters; a never-seen activity counts as idle.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Only systems whose symbol contains this, case-insensitive (EURUSD)." },
                    "running": { "type": "boolean", "description": "true: only bots running; false: only stopped." },
                    "pnl_pct_min": { "type": "number", "description": "Only systems whose P&L of the day is at least this percent of their capital (1 means +1 %)." },
                    "pnl_pct_max": { "type": "number", "description": "Only systems whose P&L of the day is at most this percent of their capital (-1 means -1 %)." },
                    "pnl_usd_min": { "type": "number", "description": "Only systems whose P&L of the day in dollars is at least this (negative values are losses)." },
                    "pnl_usd_max": { "type": "number", "description": "Only systems whose P&L of the day in dollars is at most this." },
                    "idle_minutes": { "type": "integer", "minimum": 0, "description": "Only systems with no activity for at least this many minutes." }
                }
            }),
        },
        ToolDef {
            name: TRADING_PROPOSE_TOOL,
            description: "Ask the person to run a money or bot action on the trading desk. It NEVER runs by itself: it appears at once as a card in this chat, above the composer, with Approve / Reject. It returns immediately (default), so do not wait: tell the person the card is waiting and do not propose the same action again. When they write again, `trading_snapshot` shows in `pendingProposals` / `recentProposals` what they decided. Actions: close_position (needs symbol, exactly as trading_snapshot names it), close_all (every closable position of the engine account), panic (stop every bot, cancel pending tasks, close everything), start_systems / stop_systems (need system_ids: one card covers the whole list, naming each system; system_id is the single-system shorthand). Always give a short reason in the person's language; propose one action at a time and only when the person asked for it or clearly agreed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["close_position", "close_all", "panic", "start_systems", "stop_systems", "start_system", "stop_system"] },
                    "symbol": { "type": "string", "description": "close_position: the position's symbol." },
                    "system_ids": { "type": "array", "items": { "type": "string" }, "description": "start_systems / stop_systems: the system ids, exactly as trading_snapshot lists them. One approval runs all of them." },
                    "system_id": { "type": "string", "description": "Shorthand for a single start_systems / stop_systems id." },
                    "reason": { "type": "string", "description": "One line the person reads beside the buttons." },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 600, "description": "0 (default) returns at once with the card waiting. Use a positive value only when you must know the answer in this same turn (the person may not answer within it)." }
                },
                "required": ["action", "reason"]
            }),
        },
        ToolDef {
            name: TRADING_SCHEDULE_TOOL,
            description: "Program a start or stop for later, with the person's approval. It NEVER schedules by itself: it appears at once as a card in this chat, above the composer; approving it creates the desk task, and rejecting it does nothing. The task then runs on its own at that time even if this chat is closed, as long as Aurea is running; the desk's Tasks list shows it and the person can cancel it there. If Aurea was off at its time (more than five minutes late), the task is marked missed and does NOT run late. Give either delay_minutes or at_time (the next occurrence of \"HH:MM\" on the PC clock). system_ids is required: use trading_select first for groups like \"los que pierden más de 1 %\", and pass exactly those ids. Returns immediately with the card waiting; do not schedule it again. `trading_snapshot` shows the task in `tasks` and the card's outcome in `pendingProposals` / `recentProposals`.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start_systems", "stop_systems", "start_system", "stop_system"] },
                    "system_ids": { "type": "array", "items": { "type": "string" }, "description": "The system ids, exactly as trading_snapshot lists them. One approval schedules all of them as one task." },
                    "system_id": { "type": "string", "description": "Shorthand for a single system id." },
                    "delay_minutes": { "type": "number", "exclusiveMinimum": 0, "description": "Minutes from now on the PC clock; use either this or at_time." },
                    "at_time": { "type": "string", "description": "The next local wall-clock time \"HH:MM\" on the PC clock (tomorrow when already past); use either this or delay_minutes." },
                    "reason": { "type": "string", "description": "One line the person reads beside the buttons." }
                },
                "required": ["action", "reason"]
            }),
        },
        ToolDef {
            name: TRADING_WATCH_TOOL,
            description: "Leave a watch rule on the desk: Aurea checks it on every update (once a second while it runs) and, when it holds, notifies the person and — if the rule carries an action — leaves a proposal card in this chat. A rule NEVER acts by itself: at most it proposes, and the person approves or rejects. Use it when the person says \"avísame si…\", \"vigila…\", \"si pierde más de X %…\". Conditions: system_pnl_pct / system_pnl_usd (need system_id and at_most and/or at_least; percent is of the capital the person assigned), system_stopped (system_id), system_idle (system_id + minutes without activity), account_pnl_pct (at_most/at_least over opening equity), position_pnl_usd (symbol + at_most/at_least). `action` is optional: start_systems / stop_systems (system_ids), close_position (symbol), close_all, panic; omit it to only notify. The rule expires after expires_minutes (default 1440 = 24 h, max 30 days). It shows in the desk's Reglas section and in trading_snapshot's `rules`. Returns the rule at once.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "condition": { "type": "string", "enum": ["system_pnl_pct", "system_pnl_usd", "system_stopped", "system_idle", "account_pnl_pct", "position_pnl_usd"] },
                    "system_id": { "type": "string", "description": "The system the condition watches, exactly as trading_snapshot lists it." },
                    "symbol": { "type": "string", "description": "position_pnl_usd: the position's symbol; also close_position when the rule fires." },
                    "at_most": { "type": "number", "description": "Fire when the value is at most this (a loss threshold is negative)." },
                    "at_least": { "type": "number", "description": "Fire when the value is at least this (a profit threshold is positive)." },
                    "minutes": { "type": "integer", "minimum": 1, "description": "system_idle: minutes without activity." },
                    "action": { "type": "string", "enum": ["start_systems", "stop_systems", "start_system", "stop_system", "close_position", "close_all", "panic"], "description": "What to PROPOSE when the condition holds; omit to only notify." },
                    "system_ids": { "type": "array", "items": { "type": "string" }, "description": "start/stop_systems: the system ids the proposal covers." },
                    "name": { "type": "string", "description": "Short name for the rule's row (defaults to the reason)." },
                    "reason": { "type": "string", "description": "One line the person reads when the rule fires and beside the buttons." },
                    "expires_minutes": { "type": "number", "exclusiveMinimum": 0, "description": "How long it watches (default 1440 = 24 h, max 43200 = 30 days)." }
                },
                "required": ["condition", "reason"]
            }),
        },
        ToolDef {
            name: TRADING_UNWATCH_TOOL,
            description: "Stop watching a rule that has not fired yet. Use the `id` trading_snapshot's `rules` lists. A fired, cancelled or expired rule cannot be cancelled again.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "rule_id": { "type": "string", "description": "The rule's id, exactly as trading_snapshot lists it." }
                },
                "required": ["rule_id"]
            }),
        },
    ]
}

#[derive(Debug, Default, Deserialize)]
struct TradingSnapshotArgs {
    logs: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TradingProposeArgs {
    action: String,
    symbol: Option<String>,
    #[serde(default)]
    system_ids: Option<Vec<String>>,
    system_id: Option<String>,
    reason: String,
    wait_seconds: Option<u64>,
}

/// The systems a start/stop proposal covers: `system_ids` in order, with the
/// single `system_id` shorthand appended and blanks/duplicates dropped.
fn proposal_system_ids(args: &TradingProposeArgs) -> anyhow::Result<Vec<String>> {
    let listed = args.system_ids.iter().flatten().chain(args.system_id.iter());
    collect_system_ids(&args.action, listed)
}

/// The systems a schedule covers: same rules as a proposal, but a schedule
/// always names at least one (an empty list would silently mean the whole pool).
fn schedule_system_ids(args: &TradingScheduleArgs) -> anyhow::Result<Vec<String>> {
    let listed = args.system_ids.iter().flatten().chain(args.system_id.iter());
    collect_system_ids(&args.action, listed)
}

fn collect_system_ids<'a>(
    action: &str,
    listed: impl Iterator<Item = &'a String>,
) -> anyhow::Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    for id in listed {
        let id = id.trim();
        if !id.is_empty() && !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_owned());
        }
    }
    anyhow::ensure!(!ids.is_empty(), "{action} needs system_ids");
    Ok(ids)
}

#[derive(Debug, Deserialize)]
struct TradingScheduleArgs {
    action: String,
    #[serde(default)]
    system_ids: Option<Vec<String>>,
    system_id: Option<String>,
    delay_minutes: Option<f64>,
    at_time: Option<String>,
    reason: String,
}

/// `trading_watch`: one condition over the desk plus, optionally, what to
/// propose when it holds.
#[derive(Debug, Deserialize)]
struct TradingWatchArgs {
    condition: String,
    system_id: Option<String>,
    symbol: Option<String>,
    at_most: Option<f64>,
    at_least: Option<f64>,
    minutes: Option<u64>,
    action: Option<String>,
    #[serde(default)]
    system_ids: Option<Vec<String>>,
    name: Option<String>,
    reason: String,
    expires_minutes: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct TradingUnwatchArgs {
    rule_id: String,
}

/// The condition the tool's words become, refusing a missing id or threshold
/// here rather than in the engine.
fn watch_condition(args: &TradingWatchArgs) -> anyhow::Result<Value> {
    let system = || -> anyhow::Result<String> {
        args.system_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("{} needs system_id", args.condition))
    };
    let symbol = || -> anyhow::Result<String> {
        args.symbol
            .as_deref()
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("{} needs symbol", args.condition))
    };
    let bounds = || -> anyhow::Result<(Value, Value)> {
        anyhow::ensure!(
            args.at_most.is_some() || args.at_least.is_some(),
            "{} needs at_most and/or at_least",
            args.condition
        );
        Ok((json!(args.at_most), json!(args.at_least)))
    };
    Ok(match args.condition.as_str() {
        "system_pnl_pct" | "system_pnl_usd" => {
            let (at_most, at_least) = bounds()?;
            let mut value = json!({ "system_id": system()?, "at_most": at_most, "at_least": at_least });
            value["kind"] = json!(args.condition);
            value
        }
        "system_stopped" => json!({ "kind": "system_stopped", "system_id": system()? }),
        "system_idle" => json!({
            "kind": "system_idle",
            "system_id": system()?,
            "minutes": args.minutes.ok_or_else(|| anyhow::anyhow!("system_idle needs minutes"))?,
        }),
        "account_pnl_pct" => {
            let (at_most, at_least) = bounds()?;
            json!({ "kind": "account_pnl_pct", "at_most": at_most, "at_least": at_least })
        }
        "position_pnl_usd" => {
            let (at_most, at_least) = bounds()?;
            json!({ "kind": "position_pnl_usd", "symbol": symbol()?, "at_most": at_most, "at_least": at_least })
        }
        other => anyhow::bail!("unknown condition: {other}"),
    })
}

/// The proposal a rule files when it fires; `None` means notify only.
fn watch_proposal(args: &TradingWatchArgs) -> anyhow::Result<Option<Value>> {
    let Some(action) = args
        .action
        .as_deref()
        .map(str::trim)
        .filter(|action| !action.is_empty())
    else {
        return Ok(None);
    };
    let action = match action {
        "start_system" | "start_systems" => {
            let listed = args.system_ids.iter().flatten();
            json!({ "kind": "start_systems", "system_ids": collect_system_ids(action, listed)? })
        }
        "stop_system" | "stop_systems" => {
            let listed = args.system_ids.iter().flatten();
            json!({ "kind": "stop_systems", "system_ids": collect_system_ids(action, listed)? })
        }
        "close_position" => {
            let symbol = args
                .symbol
                .as_deref()
                .map(str::trim)
                .filter(|symbol| !symbol.is_empty())
                .ok_or_else(|| anyhow::anyhow!("close_position needs symbol"))?;
            json!({ "kind": "close_position", "symbol": symbol })
        }
        "close_all" => json!({ "kind": "close_all" }),
        "panic" => json!({ "kind": "panic" }),
        other => anyhow::bail!("unknown action: {other}"),
    };
    Ok(Some(action))
}

/// `trading_select`: the filters the person's words become. Every one is
/// optional; with none, every system matches.
#[derive(Debug, Default, Deserialize, Serialize)]
struct TradingSelectArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pnl_pct_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pnl_pct_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pnl_usd_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pnl_usd_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_minutes: Option<u64>,
}

/// `10,25` (Spanish decimal comma), the way the desk writes numbers.
fn es_number(value: f64) -> String {
    format!("{value:.2}").replace('.', ",")
}

/// `2 h 5 min` / `18 min`, for how long a system has been quiet.
fn es_idle(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    if minutes < 60 {
        format!("{minutes} min")
    } else {
        let hours = minutes / 60;
        let rest = minutes % 60;
        if rest == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {rest} min")
        }
    }
}

/// Why a system is in the answer: its state, its day and how long it has been
/// quiet, in the person's language.
fn system_why(system: &Value, now: i64) -> String {
    let running = system.get("running").and_then(Value::as_bool).unwrap_or(false);
    let mut parts = vec![if running { "en marcha" } else { "parado" }.to_string()];
    match system.get("pnlPct").and_then(Value::as_f64) {
        Some(pct) if pct < 0.0 => parts.push(format!("pierde {} % hoy", es_number(-pct))),
        Some(pct) if pct > 0.0 => parts.push(format!("gana {} % hoy", es_number(pct))),
        Some(_) => parts.push("0,00 % hoy".into()),
        None => {
            if let Some(day) = system.get("pnlDay").and_then(Value::as_f64) {
                if day < 0.0 {
                    parts.push(format!("pierde {} $ hoy", es_number(-day)));
                } else if day > 0.0 {
                    parts.push(format!("gana {} $ hoy", es_number(day)));
                }
            }
        }
    }
    match system.get("lastActivityAt").and_then(Value::as_i64) {
        Some(at) => parts.push(format!("activo hace {}", es_idle(now.saturating_sub(at)))),
        None => parts.push("sin actividad registrada".into()),
    }
    parts.join(" · ")
}

/// The systems of `systems` that pass every filter in `args`, each with the
/// line that explains why it is there. Pure, so the rules are testable without
/// a desk: `now` decides what "idle" means.
fn select_systems(systems: &[Value], args: &TradingSelectArgs, now: i64) -> Vec<Value> {
    let symbol = args
        .symbol
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let mut matched = Vec::new();
    for system in systems {
        let Some(id) = system.get("id").and_then(Value::as_str) else {
            continue;
        };
        let system_symbol = system.get("symbol").and_then(Value::as_str).unwrap_or("");
        if let Some(needle) = &symbol {
            if !system_symbol.to_lowercase().contains(needle.as_str()) {
                continue;
            }
        }
        let running = system.get("running").and_then(Value::as_bool).unwrap_or(false);
        if args.running.is_some_and(|want| want != running) {
            continue;
        }
        // A percent bound needs a percent: a system without capital has no
        // place in "los que pierden más de 1 %".
        let pnl_pct = system.get("pnlPct").and_then(Value::as_f64);
        if args.pnl_pct_min.is_some() || args.pnl_pct_max.is_some() {
            let Some(pct) = pnl_pct else { continue };
            if args.pnl_pct_min.is_some_and(|min| pct < min) {
                continue;
            }
            if args.pnl_pct_max.is_some_and(|max| pct > max) {
                continue;
            }
        }
        let pnl_day = system.get("pnlDay").and_then(Value::as_f64);
        if args.pnl_usd_min.is_some() || args.pnl_usd_max.is_some() {
            let Some(day) = pnl_day else { continue };
            if args.pnl_usd_min.is_some_and(|min| day < min) {
                continue;
            }
            if args.pnl_usd_max.is_some_and(|max| day > max) {
                continue;
            }
        }
        let last_activity = system.get("lastActivityAt").and_then(Value::as_i64);
        if let Some(minutes) = args.idle_minutes {
            let idle_ms = (minutes.min(i64::MAX as u64 / 60_000) * 60_000) as i64;
            // Never having logged counts as idle: the bot has shown no life.
            if last_activity.is_some_and(|at| now.saturating_sub(at) < idle_ms) {
                continue;
            }
        }
        matched.push(json!({
            "id": id,
            "name": system.get("name").cloned().unwrap_or(Value::Null),
            "symbol": system_symbol,
            "running": running,
            "pnlDay": pnl_day,
            "pnlPct": pnl_pct,
            "lastActivityAt": last_activity,
            "why": system_why(system, now),
        }));
    }
    matched
}

/// The desk's state, trimmed to what an agent reasons about.
fn summarize_trading(snapshot: &Value, logs: usize) -> Value {
    let pick = |value: &Value, keys: &[&str]| -> Value {
        let mut out = serde_json::Map::new();
        for key in keys {
            if let Some(v) = value.get(*key).filter(|v| !v.is_null()) {
                out.insert((*key).to_string(), v.clone());
            }
        }
        Value::Object(out)
    };
    let list = |key: &str, keys: &[&str]| -> Value {
        Value::Array(
            snapshot
                .get(key)
                .and_then(Value::as_array)
                .map(|rows| rows.iter().map(|row| pick(row, keys)).collect())
                .unwrap_or_default(),
        )
    };
    let demo = snapshot.get("version").and_then(Value::as_str) == Some("0.1.0-mock");
    let recent_logs: Vec<Value> = snapshot
        .get("logs")
        .and_then(Value::as_array)
        .map(|rows| {
            let mut rows: Vec<Value> = rows
                .iter()
                .map(|row| pick(row, &["ts", "level", "system", "message"]))
                .collect();
            rows.sort_by_key(|row| std::cmp::Reverse(row.get("ts").and_then(Value::as_i64).unwrap_or(0)));
            rows.truncate(logs);
            rows
        })
        .unwrap_or_default();
    let pending: Vec<Value> = snapshot
        .get("proposals")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|p| p.get("status").and_then(Value::as_str) == Some("pending"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut answered: Vec<Value> = snapshot
        .get("proposals")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|p| p.get("status").and_then(Value::as_str) != Some("pending"))
                .map(|p| pick(p, &["id", "action", "reason", "status", "result", "resolvedAt"]))
                .collect()
        })
        .unwrap_or_default();
    answered.sort_by_key(|row| {
        std::cmp::Reverse(row.get("resolvedAt").and_then(Value::as_i64).unwrap_or(0))
    });
    answered.truncate(5);
    // Which connections feed the desk right now (MT5 accounts, TWS paper).
    let vivos: Vec<String> = snapshot
        .get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .filter(|l| l.get("connected").and_then(Value::as_bool) == Some(true))
                .filter_map(|l| l.get("label").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mode = if demo {
        "demo (invented data, nothing real)".to_string()
    } else if vivos.is_empty() {
        "disconnected (no broker connected: no data)".to_string()
    } else {
        format!("live ({})", vivos.join(", "))
    };
    json!({
        "mode": mode,
        "connected": snapshot.get("connected").cloned().unwrap_or(Value::Null),
        "account": snapshot
            .get("account")
            .map(|a| pick(a, &["id", "type", "equity", "cash", "pnlDay", "pnlRealized", "buyingPower"]))
            .unwrap_or(Value::Null),
        "positions": list("positions", &["symbol", "side", "size", "entry", "current", "pnl", "pnlPct"]),
        "openPositionsAllAccounts": snapshot.get("openPositions").and_then(Value::as_array).map(Vec::len),
        "systems": list(
            "systems",
            &[
                "id", "name", "running", "symbol", "timeframe", "pnlDay", "capital", "pnlPct",
                "lastActivityAt", "description", "rules",
            ]
        ),
        "tasks": list("tasks", &["id", "kind", "status", "runAt", "systemIds"]),
        "rules": list(
            "rules",
            &["id", "name", "condition", "proposal", "reason", "status", "createdAt", "expiresAt", "firedAt", "note"]
        ),
        "links": list("links", &["id", "label", "broker", "connected"]),
        "pendingProposals": pending,
        "recentProposals": answered,
        "recentLogs": recent_logs,
    })
}

/// `trading_brief`: the desk's day as a few spoken lines, plus the counts an
/// agent may quote. Read-only and deterministic; timestamps are local (PC
/// clock), the same clock the desk shows.
fn trading_brief_value(snapshot: &Value, now: chrono::DateTime<chrono::Local>) -> Value {
    let list = |key: &str| -> Vec<Value> {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let number = |row: &Value, key: &str| row.get(key).and_then(Value::as_f64);
    let millis = |row: &Value, key: &str| row.get(key).and_then(Value::as_i64);
    let demo = snapshot.get("version").and_then(Value::as_str) == Some("0.1.0-mock");
    let day_start = start_of_day(now);
    let is_today = |ms: Option<i64>| ms.is_some_and(|ms| ms >= day_start);
    let status = row_status;
    let text = row_text;

    let mut lines = vec![format!(
        "Resumen de la mesa ({}) — {}",
        if demo { "demo, datos inventados" } else { "TWS paper" },
        now.format("%d/%m %H:%M")
    )];

    // Account.
    match snapshot.get("account").filter(|account| !account.is_null()) {
        Some(account) => {
            let id = text(account, "id").unwrap_or("sin id");
            let mut parts = Vec::new();
            if let Some(equity) = number(account, "equity") {
                parts.push(format!("equity {}", es_money(equity)));
            }
            if let Some(pnl) = number(account, "pnlDay") {
                parts.push(format!("P&L del día {}", es_signed_money(pnl)));
            }
            if parts.is_empty() {
                parts.push("sin datos".into());
            }
            lines.push(format!("Cuenta {id}: {}.", parts.join(", ")));
        }
        None => lines.push("Cuenta: ninguna conectada.".into()),
    }

    // Systems, best and worst by P&L percent of the capital the person gave.
    let systems = list("systems");
    let running = systems
        .iter()
        .filter(|system| system.get("running").and_then(Value::as_bool) == Some(true))
        .count();
    let mut line = if systems.is_empty() {
        "Sistemas: ninguno.".to_string()
    } else {
        format!("Sistemas: {running} de {} en marcha.", systems.len())
    };
    let mut ranked: Vec<(&Value, f64)> = systems
        .iter()
        .filter_map(|system| number(system, "pnlPct").map(|pct| (system, pct)))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let describe = |system: &Value, pct: f64| {
        let name = text(system, "name").unwrap_or("sin nombre");
        let money = number(system, "pnlDay").map(es_signed_money);
        match money {
            Some(money) => format!("{name} {money} ({})", es_signed_pct(pct)),
            None => format!("{name} {}", es_signed_pct(pct)),
        }
    };
    match ranked.as_slice() {
        [(best, pct)] => line.push_str(&format!(" Mejor: {}.", describe(best, *pct))),
        [(best, best_pct), (worst, worst_pct), ..] => line.push_str(&format!(
            " Mejor: {}; peor: {}.",
            describe(best, *best_pct),
            describe(worst, *worst_pct)
        )),
        [] => {}
    }
    lines.push(line);

    // Open positions.
    let positions = list("positions");
    if positions.is_empty() {
        lines.push("Posiciones: ninguna abierta.".into());
    } else {
        let pnls: Vec<f64> = positions.iter().filter_map(|p| number(p, "pnl")).collect();
        let mut line = format!("Posiciones: {} abiertas", positions.len());
        if !pnls.is_empty() {
            line.push_str(&format!(", P&L {}", es_signed_money(pnls.iter().sum())));
        }
        line.push('.');
        lines.push(line);
    }

    // Tasks: what still waits (and when) and what today came to.
    let tasks = list("tasks");
    let pending: Vec<&Value> = tasks.iter().filter(|task| status(task) == "pending").collect();
    let next = pending.iter().filter_map(|task| millis(task, "runAt")).min();
    let mut line = format!("Tareas: {}", es_count(pending.len(), "pendiente", "pendientes"));
    if let Some(next) = next {
        line.push_str(&format!(" (a las {})", es_clock(next)));
    }
    let today_with = |wanted: &str| {
        tasks
            .iter()
            .filter(|task| status(task) == wanted && is_today(millis(task, "resolvedAt")))
            .count()
    };
    let mut today = Vec::new();
    for (wanted, one, many) in [
        ("ran", "hecha", "hechas"),
        ("missed", "perdida", "perdidas"),
        ("failed", "fallida", "fallidas"),
        ("cancelled", "cancelada", "canceladas"),
    ] {
        let count = today_with(wanted);
        if count > 0 {
            today.push(es_count(count, one, many));
        }
    }
    if !today.is_empty() {
        line.push_str(&format!("; hoy {}", today.join(", ")));
    }
    line.push('.');
    lines.push(line);

    // Rules.
    let rules = list("rules");
    let watching = rules.iter().filter(|rule| status(rule) == "watching").count();
    let fired = rules
        .iter()
        .filter(|rule| status(rule) == "fired" && is_today(millis(rule, "firedAt")))
        .count();
    let expired = rules
        .iter()
        .filter(|rule| status(rule) == "expired" && is_today(millis(rule, "expiresAt")))
        .count();
    let mut line = format!("Reglas: {watching} vigilando");
    if fired + expired > 0 {
        line.push_str(&format!(
            "; hoy {} y {}",
            es_count(fired, "aviso", "avisos"),
            es_count(expired, "caducada", "caducadas")
        ));
    }
    line.push('.');
    lines.push(line);

    // Requests: what waits and today's audit trail with the outcome.
    let proposals = list("proposals");
    let waiting = proposals.iter().filter(|p| status(p) == "pending").count();
    let mut answered: Vec<&Value> = proposals
        .iter()
        .filter(|p| status(p) != "pending" && is_today(millis(p, "resolvedAt")))
        .collect();
    answered.sort_by_key(|p| std::cmp::Reverse(millis(p, "resolvedAt").unwrap_or(0)));
    let count = |wanted: &str| answered.iter().filter(|p| status(p) == wanted).count();
    let mut line = format!("Peticiones: {waiting} esperando respuesta");
    if !answered.is_empty() {
        line.push_str(&format!(
            "; hoy {}, {} y {}",
            es_count(count("done"), "aprobada", "aprobadas"),
            es_count(count("rejected"), "rechazada", "rechazadas"),
            es_count(count("failed"), "fallida", "fallidas")
        ));
    }
    line.push('.');
    lines.push(line);
    if !answered.is_empty() {
        let mut parts = Vec::new();
        for proposal in answered.iter().take(3) {
            let reason = text(proposal, "reason").unwrap_or("sin motivo");
            let reason: String = reason.chars().take(80).collect();
            let word = match status(proposal) {
                "done" => "aprobada",
                "rejected" => "rechazada",
                "failed" => "falló",
                _ => "resuelta",
            };
            let result = text(proposal, "result")
                .filter(|result| !result.is_empty())
                .map(|result| result.trim_end_matches('.').to_owned());
            parts.push(match result {
                Some(result) => format!("{word} «{reason}» → {result}"),
                None => format!("{word} «{reason}»"),
            });
        }
        lines.push(format!("Hoy: {}.", parts.join("; ")));
    }

    // The day's log, with the last error when there is one.
    let logs = list("logs");
    let today_logs: Vec<&Value> = logs
        .iter()
        .filter(|row| is_today(millis(row, "ts")))
        .collect();
    let level = row_level;
    let warnings = today_logs.iter().filter(|row| level(row) == "warn").count();
    let errors = today_logs.iter().filter(|row| level(row) == "error").count();
    let mut line = if warnings + errors == 0 {
        "Registro hoy: sin avisos ni errores.".to_string()
    } else {
        format!(
            "Registro hoy: {} y {}.",
            es_count(warnings, "aviso", "avisos"),
            es_count(errors, "error", "errores")
        )
    };
    if let Some(last) = today_logs.iter().filter(|row| level(row) == "error").max_by_key(|row| millis(row, "ts")) {
        if let (Some(ts), Some(message)) = (millis(last, "ts"), text(last, "message")) {
            line.push_str(&format!(" Último error: {message} ({}).", es_clock(ts)));
        }
    }
    lines.push(line);

    json!({
        "mode": if demo { "demo" } else { "live" },
        "generatedAt": now.timestamp_millis(),
        "text": lines.join("\n"),
        "highlights": {
            "systemsRunning": running,
            "systems": systems.len(),
            "openPositions": positions.len(),
            "pendingTasks": pending.len(),
            "rulesWatching": watching,
            "pendingProposals": waiting,
        },
    })
}

fn row_text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn row_status(row: &Value) -> &str {
    row_text(row, "status").unwrap_or("")
}

fn row_level(row: &Value) -> &str {
    row_text(row, "level").unwrap_or("")
}

fn start_of_day(now: chrono::DateTime<chrono::Local>) -> i64 {    use chrono::TimeZone;
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| chrono::Local.from_local_datetime(&naive).earliest())
        .map(|at| at.timestamp_millis())
        .unwrap_or(i64::MIN)
}

fn es_clock(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

fn es_count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A number the way the desk writes it: thousands with dots, decimals with a
/// comma.
fn es_decimal(value: f64, decimals: usize) -> String {
    let formatted = format!("{value:.decimals$}");
    let (sign, digits) = match formatted.strip_prefix('-') {
        Some(rest) => ("-", rest.to_owned()),
        None => ("", formatted),
    };
    let (integer, fraction) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer.to_owned(), Some(fraction.to_owned())),
        None => (digits, None),
    };
    let mut grouped = String::new();
    for (ix, ch) in integer.chars().enumerate() {
        if ix > 0 && (integer.len() - ix) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    // A loss that rounds to zero reads "0", never "-0".
    let sign = if grouped.chars().all(|ch| ch == '0')
        && fraction.as_deref().is_none_or(|f| f.chars().all(|ch| ch == '0'))
    {
        ""
    } else {
        sign
    };
    match fraction {
        Some(fraction) => format!("{sign}{grouped},{fraction}"),
        None => format!("{sign}{grouped}"),
    }
}

fn es_money(value: f64) -> String {
    format!("{} $", es_decimal(value, 2))
}

fn es_signed_money(value: f64) -> String {
    if value > 0.0 {
        format!("+{}", es_money(value))
    } else {
        es_money(value)
    }
}

fn es_signed_pct(value: f64) -> String {
    if value > 0.0 {
        format!("+{} %", es_decimal(value, 2))
    } else {
        format!("{} %", es_decimal(value, 2))
    }
}

/// Shows a page in the CALLING chat, so it is offered only to an agent that
/// Zeron launched (`ZERON_CHAT_ID` set).
const SHOW_VISUAL: &str = "show_visual";
/// Shows an image file in the CALLING chat; offered like `show_visual`.
const SHOW_IMAGE: &str = "show_image";

/// Says what the CALLING chat's agent waits on after its turn; offered like
/// `show_visual`.
const WAIT_ON_BACKGROUND: &str = "wait_on_background";

const WAIT_ON_BACKGROUND_DESCRIPTION: &str = "\
Say that you'll wait on background work you started, so this chat reads as waiting (not finished) \
after your turn ends, until that work ends and you're woken to carry on. Pass the ids your tools \
gave the background tasks (a background command says the id it runs under). Use it for work you'll \
come back to: a test run, a build, a long check. Never for something you leave running on purpose \
(a dev server, a file watcher): nothing is coming, and the chat would look busy. Background agents \
you started are waited on already. `waiting: false` takes a mark back. It doesn't stop or change \
the work. Refused where your runtime doesn't wake you when background work ends: wait for the work \
inside your turn instead.";

const SHOW_IMAGE_DESCRIPTION: &str = "\
Show an image file (a screenshot, a rendered chart or diagram, a photo) inline in this chat, above \
your final text reply; call it before writing that reply, once per image, in the order they should \
appear. PNG, JPEG, WebP, or GIF, at most 10 MB and 4096 pixels on a side. Zeron copies the file \
into the chat, so it stays visible after the file changes or is deleted. The reader \
already sees the image, so the reply should not announce or describe it: add only what the image \
doesn't show.";

const SHOW_VISUAL_DESCRIPTION: &str = "\
Show an interactive visual (chart, table, dashboard, diagram, simulation, 3D view) inline in this \
chat, above your final text reply; call it before writing that reply. Use it when a visual says more \
than prose. The reader already sees it, so the reply should not announce, locate, or restate it: add \
only what the visual doesn't say. To show an image file as it is (a screenshot, a photo), use \
show_image instead.\n\
\n\
A visual is a Luau script (Lua 5.1 plus `+=`, `continue`, and `if` expressions). Zeron runs it in a \
sandbox (no I/O or network; math.random is seeded, so data is the same on every render) and renders \
what it returns with Zeron's own native interface. Build data and state at the top level, then return \
a view function. Zeron calls the view again after every handler, so handlers just assign to state:\n\
\n\
local s = ui.state { metric = \"p95\" }\n\
local data = { { region = \"us-east\", p95 = 182, p99 = 240 }, { region = \"eu-west\", p95 = 140, p99 = 210 } }\n\
return function()\n\
  return ui.column { gap = 16,\n\
    ui.text { \"Latency by region\", size = \"lg\", weight = 600 },\n\
    ui.tabs { options = { \"p95\", \"p99\" }, value = s.metric, on_change = function(v) s.metric = v end },\n\
    ui.chart { type = \"bar\", height = 220, unit = \"ms\",\n\
      x = ui.map(data, function(d) return d.region end),\n\
      series = { { name = s.metric, values = ui.map(data, function(d) return d[s.metric] end) } } },\n\
  }\n\
end\n\
\n\
Zeron runs the script before showing it (setup, render, every draw, and each tab, toggle, slider, and \
button once) and returns the first error with its line; fix it and call again.\n\
\n\
Nodes are props tables: children are the array part, strings are plain text, nil and false children \
are skipped, and lists of nodes flatten (so ui.map results can be children). Unknown props are errors.\n\
- ui.column { gap = 8, align = \"start|center|end\", ... }, ui.row { gap = 12, align, justify = \
\"start|center|end|between\", wrap = true, ... }, ui.grid { columns = 3, gap = 16, ... }. Any node \
takes width = px or grow = true inside a row (sliders, charts, tables, canvases, and scenes fill a \
row's spare width by default), and key = \"name\" to keep its hover or camera when the tree changes \
shape. A row or column with on_click = function() end is clickable; hover = true gives \
it the hover wash.\n\
- ui.text { \"plain \", ui.span { \"bold\", weight = 600, color = \"accent\", mono = true }, size = \
\"xs|sm|body|lg|xl|2xl\", weight = 400-700, color, mono, italic, align, truncate }, or ui.text \"string\".\n\
- ui.stat { value = \"182 ms\", label = \"p95 latency\", delta = \"-12%\", tone = \"success\" }: a headline \
figure over a muted label; tone colors the delta.\n\
- ui.table { columns = { \"Region\", { label = \"p95\", align = \"end\" } }, rows = { { \"us-east\", 182 }, \
... } }: laid out like a markdown table (bold header, hairline row rules), numbers right-aligned; cells \
are strings, numbers, or nodes.\n\
- ui.chart { type = \"bar|line|area|scatter\", height = 220, x = { labels }, series = { { name, values = \
{ ... }, color } }, stacked, horizontal (bar), min, max, unit = \"ms\", prefix = \"$\", decimals, legend }. \
Scatter series take points = { { x, y }, ... } (x_unit, x_decimals). Axes, gridlines, the legend, and a \
hover readout are built in.\n\
- ui.rule {} is a hairline divider; ui.space { size = 16 }; ui.link { label, url = \"https://...\" } \
opens like a chat link.\n\
- Controls: ui.tabs { options = { ... }, value, on_change = function(option) end }, ui.toggle { label, \
value = bool, on_change = function(on) end }, ui.slider { label, value, min, max, step, unit, on_change \
= function(n) end }, ui.button { label, on_click = function() end, primary = bool }.\n\
- ui.canvas { height = 240, draw = function(g, w, h, t) ... end, animate = bool, on_click = \
function(x, y) end }: 2D drawing in pixels, origin top-left, w = the reply column's width. \
g:line(x1, y1, x2, y2, { color, width, dash }), g:polyline(pts, { color, width }), g:polygon(pts, \
{ fill, stroke, width }), g:rect(x, y, w, h, { fill, stroke, radius }), g:circle(x, y, r, { fill, \
stroke }), g:arc(x, y, r, a0, a1, { color } or { fill, inner = radius } for pie and donut sectors; \
radians clockwise from 3 o'clock), g:text(x, y, str, { size, color, weight, align = \
\"start|center|end\", baseline = \"top|middle|bottom\", mono }). pts are { x1, y1, x2, y2, ... } or \
{ { x, y }, ... }. g.pointer is { x, y, down } while the pointer is over the canvas (draw hover \
effects with it); t is seconds, and animate = true redraws every frame while visible.\n\
- ui.scene { height = 320, draw = function(s, t) ... end, orbit = true, spin = 0.3 (turntable \
radians per second), camera = { yaw = 0.6, pitch = 0.45, zoom = 1 }, animate = bool }: real 3D, y up, \
rendered on the GPU with depth, soft shadows from a key light, and bloom. Zeron frames the camera on \
your geometry and the reader drags to orbit. Solids (box, sphere, cylinder, surface, mesh) take \
material props: roughness (0 mirror to 1 matte), metal (0-1), glow (emits its color and blooms; \
use for lamps, beams, highlights), glass = true (refracts what's behind it; ior = 1.5, dispersion \
= 0-1 splits it into a rainbow, as a prism does). Build real forms from several primitives (legs, \
tops, rims), not single boxes, and give materials so surfaces read as wood, metal, or glass. s:box { at = { x, y, z }, size = { w, h, d } or n, color }, s:sphere { at, radius, color }, \
s:cylinder { at (base center), radius, height, color }, s:surface { x = { min, max }, z = { min, max }, \
f = function(x, z) return y end or values = { { ... } }, detail = 32, colors = { \"chart1\", \"chart3\" } \
(a ramp by height), wire = bool }, s:mesh { vertices = { { x, y, z }, ... }, faces = { { 1, 2, 3 }, ... }, \
color }, s:line(a, b, { color, width }), s:path(points, { color, width }), s:point(at, { radius, \
color, hint = \"shown on hover\" }), s:label(at, text, { color, size }), s:grid { size = 10, step = 1 }, \
s:axes { size = 1, labels = { \"x\", \"y\", \"z\" } }. Physical colors may be \"#rrggbb\" in scenes \
(a material's own color: gold, glass, wood), and only there.\n\
- Assets: pass real files in show_visual's assets (name -> path) and scripts use them by name. \
s:model { asset = \"prism.glb\", at, rotate = { x, y, z } degrees, scale, fit = 2 (center it and size \
its longest side), color and material props override the model's own } draws a glTF model with its \
materials and textures. Solids take texture = \"wood.jpg\" (multiplied by color; mesh takes uvs = { { u, v \
}, ... } per vertex). s:environment { asset = \"studio.hdr\", intensity = 1, background = false } lights \
the scene from an equirectangular image (an HDR is best) that metal and glass reflect and glass bends; \
background = true also draws it behind the scene. s:sprite { at, size = 48 (pixels), color, glow = 1, \
asset = \"flare.png\" } adds a camera-facing light (a flare or glint; a soft round glow without an \
asset) that blooms. ui.shader takes textures = { \"rock.jpg\", ... } as iTexture0-iTexture3 \
(repeating, mipmapped). Find or make assets with your own tools (download, convert KTX2 or EXR to PNG \
or HDR) before calling.\n\
- ui.shader { height = 360, code = [[ GLSL ]], buffers = { [[ GLSL ]], ... }, inputs = { speed = \
s.speed }, blend = \"alpha|light|opaque\", bloom = 0-3, orbit = bool, spin, camera = { yaw, pitch, \
zoom }, animate = true }: GPU shaders for what geometry can't express: raymarched SDFs and \
fractals, light and optics (beams, refraction, dispersion), volumes, fluids, particles, generative \
art. Write each pass as on Shadertoy: void mainImage(out vec4 fragColor, in vec2 fragCoord), with \
iResolution (vec3: width and height in pixels, then the display's pixel density), iTime, \
iTimeDelta, iFrame, iMouse (pixels from the bottom-left; xy where the pointer is, zw where it went \
down while it's held, negative otherwise), iCamera (vec4 yaw, pitch, zoom, 0; orbit = true lets the reader drag \
it), every inputs name as a float (bind them to state so sliders and toggles drive the shader), \
iNoise (a tiling 256x256 random RGBA texture), and iChannel0-iChannel3 with iChannelResolution: \
buffers run first, in order, each into a float texture that every pass reads as iChannel<i> (an \
earlier buffer's current frame; a buffer's own previous frame), for multi-pass work like blur, \
trails, accumulation, and simulation. The shader sits on the chat, never in a box: don't paint a \
background, vignette, or floor color. By default (blend = \"light\") the output is emitted light: \
brightness is opacity and black is clear, right for beams, glows, sparks, and light through glass. \
For solid shaded objects (fractals, SDF bodies) set blend = \"alpha\" and write alpha 0 where the ray \
misses. bloom = 0.6-1.5 adds a soft glow around bright light. \
The width varies with the reader (a phone is about 350px wide, often narrower than the shader is \
tall): scale the scene by min(iResolution.x, iResolution.y), not one axis, and keep the subject \
centered with room on both sides, so it fits and stays centered at any aspect. \
GLSL errors come back with the pass and line number.\n\
- Helpers: ui.scale(d0, d1, r0, r1) returns a mapping function; ui.ticks(lo, hi, n); ui.fmt(n, { unit, \
prefix, decimals }) groups digits; ui.map(list, function(v, i) end); ui.range(a, b, step); ui.clamp; \
ui.lerp; ui.series(i) is \"chart<i>\"; ui.color(token, alpha); ui.mix(a, b, t). print(...) output comes \
back to you.\n\
\n\
Colors are theme tokens, never hex or color names (except scene materials): text; muted (labels, axes, captions); faint \
(tertiary notes); accent (the one thing to notice); on-accent (text on an accent mark); success, \
warning, danger (status only); border (hairlines, gridlines); chart1, chart2, chart3, chart4, \
chart5, chart6 (series, in order; \
chart1 is the accent). They follow the reader's theme, light or dark, on frosted glass or an opaque \
surface, so never assume a background color.\n\
\n\
Hover must not move the page. A visual sits in a transcript, so anything you show only while the \
pointer is on something (a custom readout, a detail line, a hint, a row's or canvas's hover state) must \
not change the visual's height, or everything under it jumps as the pointer crosses it. Reserve its \
space at rest (a placeholder or an empty line of the same height, e.g. ui.text { s.picked or \" \" }), \
or draw it where it takes no layout space (inside a ui.canvas with g.pointer, or a scene point's hint). \
Don't add or remove nodes, or lengthen text onto another line, on hover. ui.chart's hover readout \
never changes the chart's height.\n\
\n\
Style, enforced: layout nodes have no fill, border, shadow, padding, or font props. A visual is part \
of the reply, not a widget, so lay content directly on the chat with type, spacing, and alignment: no \
cards, tiles, boxed panels, banners, or canvas backgrounds. Stats are a row of plain ui.stat figures; \
a chart is a plain heading, then the chart; separate sections with gap or one ui.rule. Text renders \
in Zeron's interface font at the reader's text size: body for most text, sm and xs for labels and \
notes, lg and xl for headings, 2xl for a headline figure. The width is the reply column (about 720px, \
as narrow as 360px), so lay out fluidly with grow, wrap, and grid, and give charts, canvases, and \
scenes heights of 160-420px.";

/// Read `show_visual`'s asset files (paths relative to the agent's working
/// directory), base64-encoded by name for the engine.
async fn read_visual_assets(
    assets: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    use base64::Engine as _;
    use zeron_proto::{
        VISUAL_MAX_ASSET_BYTES as MAX_ASSET_BYTES, VISUAL_MAX_ASSETS as MAX_ASSETS,
        VISUAL_MAX_TOTAL_ASSET_BYTES as MAX_TOTAL_ASSET_BYTES,
        valid_visual_asset_name as valid_name,
    };
    anyhow::ensure!(
        assets.len() <= MAX_ASSETS,
        "a visual takes at most {MAX_ASSETS} assets"
    );
    let mut out = serde_json::Map::new();
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for (name, path) in assets {
        anyhow::ensure!(
            valid_name(name),
            "asset name '{name}' must be a plain file name (letters, digits, '.', '-', '_')"
        );
        // Stored as files, and file systems often ignore case.
        anyhow::ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "asset names must differ by more than case ('{name}')"
        );
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("asset '{name}': can't read {path}"))?;
        anyhow::ensure!(metadata.is_file(), "asset '{name}': {path} is not a file");
        anyhow::ensure!(
            metadata.len() <= MAX_ASSET_BYTES as u64,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        // Bounded however the file changes (or lies about its size).
        let mut bytes = Vec::new();
        {
            use tokio::io::AsyncReadExt as _;
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?
                .take(MAX_ASSET_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?;
        }
        anyhow::ensure!(
            bytes.len() <= MAX_ASSET_BYTES,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        total += bytes.len();
        anyhow::ensure!(
            total <= MAX_TOTAL_ASSET_BYTES,
            "a visual's assets total at most {} MB",
            MAX_TOTAL_ASSET_BYTES / (1024 * 1024)
        );
        out.insert(
            name.clone(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    Ok(out)
}

/// Read `show_image`'s file (a path relative to the agent's working
/// directory), bounded however the file changes (or lies about its size).
async fn read_image(path: &str) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    use zeron_proto::IMAGE_MAX_BYTES;
    let too_large = || anyhow::anyhow!("{path} is over {} MB", IMAGE_MAX_BYTES / (1024 * 1024));
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("can't read {path}"))?;
    anyhow::ensure!(metadata.is_file(), "{path} is not a file");
    if metadata.len() > IMAGE_MAX_BYTES as u64 {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await
        .with_context(|| format!("can't read {path}"))?
        .take(IMAGE_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("can't read {path}"))?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(too_large());
    }
    Ok(bytes)
}

// ---- argument shapes ---------------------------------------------------------

#[derive(Deserialize)]
struct ShowImageArgs {
    path: String,
}

#[derive(Deserialize)]
struct WaitOnBackgroundArgs {
    task_ids: Vec<String>,
    #[serde(default)]
    waiting: Option<bool>,
}

#[derive(Deserialize)]
struct ShowVisualArgs {
    script: String,
    title: String,
    #[serde(default)]
    assets: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct BatchArgs {
    requests: Vec<Value>,
}

#[derive(Deserialize)]
struct ChatArgs {
    chat: String,
}

#[derive(Deserialize)]
struct ListModelsArgs {
    harness: String,
    device: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListChatsArgs {
    project: Option<String>,
    device: Option<String>,
    #[serde(default)]
    include_archived: bool,
    parent: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ChatKind {
    Chat,
    Side,
    Subagent,
}

#[derive(Deserialize, Default)]
struct DeviceArgs {
    device: Option<String>,
}

/// Where a turn's replies start: the message that began it, whose id the
/// sender chose. Replies are matched by position after it, not by time
/// (`createdAt` is stamped on the host's clock, which can run behind the
/// caller's when the chat lives on another device), and knowing it takes no
/// read of the transcript, so nothing about the transcript can hold up a send.
#[derive(Clone, Default)]
struct TurnStart {
    after_message: Option<String>,
}

impl TurnStart {
    fn window(&self) -> TranscriptWindow {
        match &self.after_message {
            Some(id) => TranscriptWindow::after(id.clone(), REPLIES_WINDOW),
            None => TranscriptWindow::tail(REPLIES_WINDOW),
        }
    }
}

/// The most entries a wait reports as a turn's replies.
const REPLIES_WINDOW: usize = 500;
/// How long a finished turn's wait keeps trying a transcript it can't read.
const FOLLOW_GRACE: Duration = Duration::from_secs(30);

/// What [`Tools::deliver`] did: the tool's receipt, and the id the message
/// will have in the transcript.
struct Delivered {
    receipt: Value,
    message_id: String,
}

#[derive(Clone)]
struct PendingTurn {
    baseline: Option<Session>,
    start: TurnStart,
}

#[derive(Deserialize, Default)]
struct CreateChatArgs {
    kind: Option<ChatKind>,
    project: Option<String>,
    device: Option<String>,
    parent: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    sandbox: Option<String>,
    access: Option<String>,
    #[serde(default)]
    plan: bool,
    title: Option<String>,
    branch: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    cloud: bool,
    prompt: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ReadChatArgs {
    chat: String,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    include_reasoning: bool,
    include_tools: Option<bool>,
}

#[derive(Deserialize)]
struct SendArgs {
    chat: String,
    text: String,
    mode: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct WaitArgs {
    chat: String,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct ArchiveArgs {
    chat: String,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct RespondArgs {
    chat: String,
    request_id: Option<String>,
    #[serde(default)]
    answers: Vec<AnswerArg>,
}

#[derive(Deserialize)]
struct AnswerArg {
    question_id: String,
    labels: Vec<String>,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn wait_duration(secs: Option<u64>) -> Duration {
    secs.map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

#[cfg(test)]
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn parse_enum<T: serde::de::DeserializeOwned>(what: &str, raw: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(raw.trim().to_owned()))
        .map_err(|_| format!("unknown {what}: {raw:?}"))
}

// ---- summaries ---------------------------------------------------------------

/// Live posture of one chat as the tools report it.
fn status_of(session: Option<&Session>) -> (String, Option<i64>) {
    let Some(session) = session else {
        return ("idle".into(), None);
    };
    let age = chrono::Utc::now() - session.updated_at;
    let label = match zeron_proto::view::effective_indicator(Some(session), chrono::Utc::now()) {
        zeron_proto::view::Indicator::Working => "working",
        zeron_proto::view::Indicator::AwaitingInput => "awaitingInput",
        zeron_proto::view::Indicator::Errored => "errored",
        zeron_proto::view::Indicator::None => "idle",
    };
    (label.into(), Some(age.num_seconds().max(0)))
}

fn summarize_chat(chat: &Chat, spaces: &[Space], sessions: &[Session]) -> Value {
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id));
    let session = session_for(sessions, chat);
    let (_, status_age) = status_of(session.as_ref());
    let now = chrono::Utc::now();
    let status = zeron_proto::view::display_status(chat, session.as_ref(), now);
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(|s| json!({
            "id": s.id,
            "name": s.display_name(),
            "path": s.path,
        })),
        "deviceId": chat.device_id,
        "cwd": chat.cwd.clone().or_else(|| space.map(|s| s.path.clone())),
        "branch": chat.branch,
        "harness": chat.config.as_ref().map(|c| c.harness),
        "model": chat.config.as_ref().and_then(|c| c.model.clone()),
        "reasoning": chat.config.as_ref().and_then(|c| c.reasoning),
        "archived": chat.archived,
        "parentChatId": chat.parent_chat_id,
        "childKind": chat.child_kind,
        "status": status,
        "sessionStatus": session.as_ref().map(|s| s.status),
        "statusStale": session.as_ref().is_some_and(|s| zeron_proto::view::session_stale(s, now)),
        "lastTurnOutcome": session.as_ref().and_then(|s| s.last_turn_outcome),
        "terminalActivity": session.as_ref().and_then(|s| s.terminal_activity.as_ref()),
        "statusAgeSecs": status_age,
        "lastMessageAt": chat.last_message_at,
        "lastMessagePreview": chat.last_message_preview,
        "createdAt": chat.created_at,
    })
}

// ---- dispatch ----------------------------------------------------------------

impl Tools {
    pub fn new(zeron: Arc<Zeron>) -> Self {
        Self {
            zeron,
            pending_turns: Default::default(),
        }
    }

    /// Whether the host launched this server's agent without its own
    /// subagent tools, so `initialize` says to delegate through chats.
    pub fn side_chats_only(&self) -> bool {
        self.zeron.origin().side_chats_only
    }

    /// Whether this server's agent is offered `wait_on_background`: a
    /// chat's, on a runtime that wakes it as background work ends.
    pub fn waits_on_background(&self) -> bool {
        let origin = self.zeron.origin();
        origin.chat_id.is_some() && origin.waits_on_background
    }

    pub fn list(&self) -> Vec<ToolDef> {
        let in_chat = self.zeron.origin().chat_id.is_some();
        let waits = self.waits_on_background();
        catalog()
            .into_iter()
            .filter(|tool| match tool.name {
                SHOW_VISUAL | SHOW_IMAGE => in_chat,
                WAIT_ON_BACKGROUND => waits,
                _ => true,
            })
            .collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.list().iter().any(|t| t.name == name)
    }

    /// `Ok` is the tool's structured result; `Err` is a message the model
    /// should read (surfaced as `isError`, never as a protocol error).
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let result = match name {
            "whoami" => self.whoami().await,
            "list_devices" => self.list_devices().await,
            DEVICE_STATS => self.device_stats(parse(args)?).await,
            "list_projects" => self.list_projects(parse(args)?).await,
            "list_harnesses" => self.list_harnesses(parse(args)?).await,
            "list_models" => self.list_models(parse(args)?).await,
            "list_chats" => self.list_chats(parse(args)?).await,
            "get_chat" => self.get_chat(parse(args)?).await,
            "create_chat" => self.create_chat(parse(args)?).await,
            "create_chats" => self.batch(parse(args)?, true).await,
            "send_messages" => self.batch(parse(args)?, false).await,
            "read_chat" => self.read_chat(parse(args)?).await,
            "send_message" => self.send_message(parse(args)?).await,
            "wait_for_turn" => self.wait_for_turn(parse(args)?).await,
            "interrupt_chat" => self.interrupt_chat(parse(args)?).await,
            "respond_to_input" => self.respond_to_input(parse(args)?).await,
            "archive_chat" => self.archive_chat(parse(args)?).await,
            SHOW_VISUAL => self.show_visual(parse(args)?).await,
            SHOW_IMAGE => self.show_image(parse(args)?).await,
            WAIT_ON_BACKGROUND => self.wait_on_background(parse(args)?).await,
            TRADING_SNAPSHOT_TOOL => self.trading_snapshot(parse(args)?).await,
            TRADING_BRIEF_TOOL => self.trading_brief().await,
            TRADING_SELECT_TOOL => self.trading_select(parse(args)?).await,
            TRADING_PROPOSE_TOOL => self.trading_propose(parse(args)?).await,
            TRADING_SCHEDULE_TOOL => self.trading_schedule(parse(args)?).await,
            TRADING_WATCH_TOOL => self.trading_watch(parse(args)?).await,
            TRADING_UNWATCH_TOOL => self.trading_unwatch(parse(args)?).await,
            theme if themes::NAMES.contains(&theme) => {
                return self.themes(theme, args).await;
            }
            other => return Err(format!("unknown tool: {other}")),
        };
        result.map_err(|e| format!("{e:#}"))
    }

    /// Poll every request together, including its optional wait. A waiting
    /// first chat must not prevent subsequent chats from receiving their work.
    async fn batch(&self, args: BatchArgs, create: bool) -> anyhow::Result<Value> {
        anyhow::ensure!(
            (1..=MAX_BATCH).contains(&args.requests.len()),
            "requests must contain between 1 and {MAX_BATCH} items"
        );
        let results = futures::future::join_all(args.requests.into_iter().enumerate().map(
            |(index, args)| async move {
                let result = if create {
                    match serde_json::from_value(args) {
                        Ok(args) => self.create_chat(args).await,
                        Err(error) => Err(error.into()),
                    }
                } else {
                    match serde_json::from_value(args) {
                        Ok(args) => self.send_message(args).await,
                        Err(error) => Err(error.into()),
                    }
                };
                match result {
                    Ok(result) => json!({"index": index, "isError": false, "result": result}),
                    Err(error) => {
                        json!({"index": index, "isError": true, "error": error.to_string()})
                    }
                }
            },
        ))
        .await;
        Ok(json!({"results": results}))
    }

    async fn trading_snapshot(&self, args: TradingSnapshotArgs) -> anyhow::Result<Value> {
        let snapshot = self
            .zeron
            .call(zeron_rpc::methods::TRADING_SNAPSHOT, json!({}))
            .await?;
        Ok(summarize_trading(&snapshot, args.logs.unwrap_or(20).min(100)))
    }

    async fn trading_brief(&self) -> anyhow::Result<Value> {
        let snapshot = self
            .zeron
            .call(zeron_rpc::methods::TRADING_SNAPSHOT, json!({}))
            .await?;
        Ok(trading_brief_value(&snapshot, chrono::Local::now()))
    }

    async fn trading_propose(&self, args: TradingProposeArgs) -> anyhow::Result<Value> {
        let action = match args.action.as_str() {
            "close_position" => json!({
                "kind": "close_position",
                "symbol": args
                    .symbol
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("close_position needs symbol"))?,
            }),
            "close_all" => json!({ "kind": "close_all" }),
            "panic" => json!({ "kind": "panic" }),
            "start_system" | "start_systems" => json!({
                "kind": "start_systems",
                "system_ids": proposal_system_ids(&args)?,
            }),
            "stop_system" | "stop_systems" => json!({
                "kind": "stop_systems",
                "system_ids": proposal_system_ids(&args)?,
            }),
            other => anyhow::bail!("unknown action: {other}"),
        };
        anyhow::ensure!(!args.reason.trim().is_empty(), "give the person a reason");
        let proposal = self
            .zeron
            .call(
                zeron_rpc::methods::TRADING_PROPOSE,
                json!({ "action": action, "reason": args.reason, "chatId": self.zeron.origin().chat_id }),
            )
            .await?;
        let id = proposal
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("the desk did not return a proposal id"))?
            .to_owned();
        let wait = Duration::from_secs(args.wait_seconds.unwrap_or(0).min(600));
        if wait.is_zero() {
            return Ok(json!({
                "id": id,
                "status": "pending",
                "note": "The card is in this chat, waiting for the person to approve or reject. Do not propose it again; when they write again, trading_snapshot shows in pendingProposals / recentProposals what they decided.",
            }));
        }
        let deadline = Instant::now() + wait;
        loop {
            let snapshot = self
                .zeron
                .call(zeron_rpc::methods::TRADING_SNAPSHOT, json!({}))
                .await?;
            let current = snapshot
                .get("proposals")
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter()
                        .find(|p| p.get("id").and_then(Value::as_str) == Some(id.as_str()))
                })
                .cloned();
            let status = current
                .as_ref()
                .and_then(|p| p.get("status").and_then(Value::as_str))
                .unwrap_or("pending")
                .to_owned();
            if status != "pending" || Instant::now() >= deadline {
                let note = if status == "pending" {
                    "Still waiting on the desk: the person has not answered yet. Do not propose it again; tell them it waits for their approval."
                } else {
                    "Resolved by the person on the desk."
                };
                return Ok(json!({
                    "id": id,
                    "status": status,
                    "result": current.as_ref().and_then(|p| p.get("result")).cloned().unwrap_or(Value::Null),
                    "note": note,
                }));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn trading_schedule(&self, args: TradingScheduleArgs) -> anyhow::Result<Value> {
        let task_kind = match args.action.as_str() {
            "start_system" | "start_systems" => "start-systems",
            "stop_system" | "stop_systems" => "stop-systems",
            other => anyhow::bail!("unknown action: {other}"),
        };
        let system_ids = schedule_system_ids(&args)?;
        anyhow::ensure!(
            !args.reason.trim().is_empty(),
            "give the person a reason"
        );
        let delay = args.delay_minutes;
        let at_time = args
            .at_time
            .as_deref()
            .map(str::trim)
            .filter(|at| !at.is_empty());
        anyhow::ensure!(
            delay.is_some() != at_time.is_some(),
            "give either delay_minutes or at_time, not both"
        );
        if let Some(minutes) = delay {
            anyhow::ensure!(
                minutes.is_finite() && minutes > 0.0,
                "delay_minutes must be a positive number of minutes"
            );
        }
        let mut action = json!({
            "kind": "schedule_task",
            "task_kind": task_kind,
            "system_ids": system_ids,
        });
        if let Some(minutes) = delay {
            action["delay_minutes"] = json!(minutes);
        }
        if let Some(at) = at_time {
            action["at_time"] = json!(at);
        }
        let proposal = self
            .zeron
            .call(
                zeron_rpc::methods::TRADING_PROPOSE,
                json!({ "action": action, "reason": args.reason, "chatId": self.zeron.origin().chat_id }),
            )
            .await?;
        let id = proposal
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("the desk did not return a proposal id"))?
            .to_owned();
        Ok(json!({
            "id": id,
            "status": "pending",
            "note": "The card is in this chat, waiting for the person to approve or reject. Do not schedule it again; approving it creates the desk task, which then runs on its own at its time. trading_snapshot shows the task in tasks and the card's outcome in pendingProposals / recentProposals.",
        }))
    }

    async fn trading_watch(&self, args: TradingWatchArgs) -> anyhow::Result<Value> {
        anyhow::ensure!(!args.reason.trim().is_empty(), "give the person a reason");
        let condition = watch_condition(&args)?;
        let proposal = watch_proposal(&args)?;
        let name = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(args.reason.trim())
            .chars()
            .take(80)
            .collect::<String>();
        let mut input = json!({
            "name": name,
            "condition": condition,
            "reason": args.reason,
        });
        if let Some(proposal) = proposal {
            input["proposal"] = proposal;
        }
        if let Some(minutes) = args.expires_minutes {
            input["expiresMinutes"] = json!(minutes);
        }
        let rule = self
            .zeron
            .call(zeron_rpc::methods::TRADING_RULE_ADD, input)
            .await?;
        Ok(json!({
            "rule": rule,
            "note": "The rule is watching on the desk. When it holds, Aurea notifies the person and, if the rule carries an action, leaves a proposal card in this chat; nothing runs without their approval. Do not add it again.",
        }))
    }

    async fn trading_unwatch(&self, args: TradingUnwatchArgs) -> anyhow::Result<Value> {
        let rule_id = args.rule_id.trim();
        anyhow::ensure!(!rule_id.is_empty(), "give the rule_id");
        let rule = self
            .zeron
            .call(
                zeron_rpc::methods::TRADING_RULE_CANCEL,
                json!({ "ruleId": rule_id }),
            )
            .await?;
        Ok(json!({
            "rule": rule,
            "note": "The rule stopped watching. A rule that already fired, expired or was cancelled cannot be cancelled again.",
        }))
    }

    async fn trading_select(&self, args: TradingSelectArgs) -> anyhow::Result<Value> {
        for (min, max, name) in [
            (args.pnl_pct_min, args.pnl_pct_max, "pnl_pct"),
            (args.pnl_usd_min, args.pnl_usd_max, "pnl_usd"),
        ] {
            if let (Some(min), Some(max)) = (min, max) {
                anyhow::ensure!(min <= max, "{name}_min is greater than {name}_max");
            }
        }
        let snapshot = self
            .zeron
            .call(zeron_rpc::methods::TRADING_SNAPSHOT, json!({}))
            .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let systems = snapshot
            .get("systems")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let matched = select_systems(systems, &args, now);
        let count = matched.len();
        let mut answer = json!({
            "criteria": args,
            "count": count,
            "matched": matched,
        });
        if count == 0 {
            answer["note"] = json!(
                "No system matches those filters: say so and do not propose anything for them."
            );
        } else {
            answer["note"] = json!(
                "Pass these `id`s together as system_ids to one trading_propose (start_systems / stop_systems); never invent ids."
            );
        }
        Ok(answer)
    }

    async fn whoami(&self) -> anyhow::Result<Value> {
        let origin = self.zeron.origin().clone();
        let local_device = self.zeron.local_device_id().await?;
        let engine = self.zeron.engine_info().await.unwrap_or(Value::Null);
        let chat = match origin.chat_id.as_deref() {
            Some(id) => match self.zeron.resolve_chat(id).await {
                Ok(chat) => {
                    let (spaces, sessions) =
                        tokio::try_join!(self.zeron.spaces(), self.zeron.sessions())?;
                    summarize_chat(&chat, &spaces, &sessions)
                }
                Err(_) => json!({ "id": id }),
            },
            None => Value::Null,
        };
        Ok(json!({
            "chat": chat,
            "originDeviceId": origin.device_id,
            "localDeviceId": local_device,
            "workspaceScope": engine.get("workspaceScope").cloned().unwrap_or(Value::Null),
            "note": if origin.chat_id.is_some() {
                "Messages you send are attributed to this chat; it cannot message itself."
            } else {
                "Not running inside a chat: messages are sent without attribution."
            },
        }))
    }

    async fn list_devices(&self) -> anyhow::Result<Value> {
        let (devices, local) =
            tokio::try_join!(self.zeron.devices(), self.zeron.local_device_id())?;
        Ok(json!({
            "devices": devices.iter().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "platform": d.platform,
                "local": d.id == local,
                "lastSeenAt": d.last_seen_at,
                "version": d.version,
            })).collect::<Vec<_>>()
        }))
    }

    /// The chosen device's machine stats (`DeviceStats`, routed by
    /// `targetDeviceId`). The engine words a remote failure (offline, too
    /// old, no answer); only this engine lacking the method is said here.
    async fn device_stats(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let (devices, local) =
            tokio::try_join!(self.zeron.devices(), self.zeron.local_device_id())?;
        let device = self.zeron.resolve_device_id(args.device.as_deref()).await?;
        let mut params = json!({});
        if device != local {
            params["targetDeviceId"] = json!(device);
        }
        let stats = match self
            .zeron
            .call_rpc(zeron_rpc::methods::DEVICE_STATS, params)
            .await
        {
            Ok(stats) => stats,
            Err(zeron_rpc::RpcError::UnknownMethod(_)) => anyhow::bail!(
                "This device's running Zeron is too old to report stats: update Zeron and restart it"
            ),
            Err(error) => anyhow::bail!("{error}"),
        };
        let name = devices.iter().find(|d| d.id == device).map(|d| d.name.clone());
        Ok(json!({
            "device": { "id": device, "name": name, "local": device == local },
            "stats": stats,
        }))
    }

    async fn list_projects(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let (mut spaces, devices) = tokio::try_join!(self.zeron.spaces(), self.zeron.devices())?;
        if let Some(device) = args.device.as_deref() {
            let device = self.zeron.resolve_device_id(Some(device)).await?;
            spaces.retain(|s| s.device_id == device);
        }
        let device_name = |id: &str| devices.iter().find(|d| d.id == id).map(|d| d.name.clone());
        Ok(json!({
            "projects": spaces.iter().map(|s| json!({
                "id": s.id,
                "name": s.display_name(),
                "path": s.path,
                "deviceId": s.device_id,
                "deviceName": device_name(&s.device_id),
                "git": s.git_detected,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_harnesses(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let harnesses = self.zeron.harnesses_on(device.as_deref()).await?;
        Ok(json!({
            "harnesses": harnesses.iter().map(|h| json!({
                "id": h.id,
                "name": h.name,
                "available": h.available(),
                "connected": h.connected(),
                "signedIn": h.signed_in,
                "installed": h.installed,
                "steersMidTurn": h.steers_mid_turn(),
                "reasoningLevels": h.reasoning_levels,
                "accessModes": h.access_modes,
                "supportsPlan": h.supports_plan,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_models(&self, args: ListModelsArgs) -> anyhow::Result<Value> {
        let harness: HarnessId =
            parse_enum("harness", &args.harness).map_err(anyhow::Error::msg)?;
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let models = self.zeron.models_on(harness, device.as_deref()).await?;
        Ok(json!({
            "harness": harness,
            "models": models.iter().map(|m| json!({
                "id": m.id,
                "label": m.label,
                "description": m.description,
                "reasoningLevels": m.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_chats(&self, args: ListChatsArgs) -> anyhow::Result<Value> {
        let (mut chats, spaces, sessions) = tokio::try_join!(
            self.zeron.chats(),
            self.zeron.spaces(),
            self.zeron.sessions()
        )?;
        if let Some(project) = args.project.as_deref() {
            let (space, _) = self
                .zeron
                .resolve_target(Some(project), args.device.as_deref())
                .await?;
            let space = space.expect("project provided");
            chats.retain(|c| c.space_id.as_deref() == Some(space.id.as_str()));
        }
        if args.device.is_some() {
            let device = self.zeron.resolve_device_id(args.device.as_deref()).await?;
            chats.retain(|c| c.device_id == device);
        }
        if !args.include_archived {
            chats.retain(|c| !c.archived);
        }
        if let Some(parent) = args.parent.as_deref() {
            let parent = self.zeron.resolve_chat(parent).await?;
            chats.retain(|c| c.parent_chat_id.as_deref() == Some(parent.id.as_str()));
        }
        chats.sort_by(|a, b| {
            let a_at = a.last_message_at.unwrap_or(a.created_at);
            let b_at = b.last_message_at.unwrap_or(b.created_at);
            b_at.cmp(&a_at)
        });
        let total = chats.len();
        let limit = args.limit.unwrap_or(50).clamp(1, 500);
        Ok(json!({
            "total": total,
            "chats": chats.iter().take(limit)
                .map(|c| summarize_chat(c, &spaces, &sessions))
                .collect::<Vec<_>>()
        }))
    }

    async fn get_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let (spaces, sessions) = tokio::try_join!(self.zeron.spaces(), self.zeron.sessions())?;
        let mut summary = summarize_chat(&chat, &spaces, &sessions);
        // The chat's facts don't depend on reaching its host.
        match self
            .zeron
            .window_of(&chat, TranscriptWindow::tail(1))
            .await
        {
            Ok(view) => {
                let rendered = render_entries(&view.entries, RenderOptions::default());
                summary["messageCount"] = json!(view.info.total);
                summary["pendingInput"] = if summary["statusStale"] == true {
                    Value::Null
                } else {
                    json!(view.info.pending_input)
                };
                summary["lastMessage"] = rendered.last().map(|m| json!(m)).unwrap_or(Value::Null);
            }
            Err(error) => summary["transcriptError"] = json!(format!("{error:#}")),
        }
        Ok(summary)
    }

    async fn create_chat(&self, args: CreateChatArgs) -> anyhow::Result<Value> {
        // Parent: the explicit `parent` argument, else the chat this server
        // speaks for. Resolved so a prefix/title works and a typo fails loud.
        let explicit_parent = args
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        anyhow::ensure!(
            args.kind != Some(ChatKind::Chat) || explicit_parent.is_none(),
            "kind chat cannot have a parent"
        );
        let parent_chat_id = if args.kind == Some(ChatKind::Chat) {
            None
        } else {
            match explicit_parent {
                Some(key) => Some(self.zeron.resolve_chat(key).await?.id),
                None => self.zeron.origin().chat_id.clone(),
            }
            // A hidden voice orchestrator creates top-level sessions.
            .filter(|parent| !zeron_proto::voice::is_orchestrator_chat(parent))
        };
        anyhow::ensure!(
            !matches!(args.kind, Some(ChatKind::Side | ChatKind::Subagent)) || parent_chat_id.is_some(),
            "kind {} requires a parent or origin chat",
            if args.kind == Some(ChatKind::Side) { "side" } else { "subagent" }
        );
        // A child is a subagent unless asked for as a side chat: delegated
        // work lists under its parent's Subagents, not its side chats.
        let kind = match (args.kind, parent_chat_id.is_some()) {
            (_, false) => ChatKind::Chat,
            (Some(ChatKind::Side), true) => ChatKind::Side,
            (_, true) => ChatKind::Subagent,
        };

        // Delegation is one level deep: a chat that has a parent never
        // creates chats, and a new chat's parent is a top-level chat.
        // The calling chat's sandbox and access cap the new one's: a
        // supervised or sandboxed agent must not hand its work to one that
        // asks nothing (the engine caps it again: rpc/agent_gate.rs).
        let mut ceiling = AccessMode::FullAccess;
        let mut sandbox_ceiling = SandboxLevel::DangerFullAccess;
        if let Some(origin) = self.zeron.origin().chat_id.as_deref()
            && !zeron_proto::voice::is_orchestrator_chat(origin)
        {
            let origin = self.zeron.resolve_chat(origin).await?;
            anyhow::ensure!(
                origin.parent_chat_id.is_none(),
                "A delegated chat can't create chats of its own. Do the work in this chat, or ask the chat that started you."
            );
            if let Some(config) = origin.config {
                ceiling = config.access;
                sandbox_ceiling = config.sandbox;
            }
        }
        if let Some(parent) = parent_chat_id.as_deref() {
            let parent = self.zeron.resolve_chat(parent).await?;
            anyhow::ensure!(
                parent.parent_chat_id.is_none(),
                "{} is itself a delegated chat: only top-level chats have side chats.",
                parent.title.as_deref().unwrap_or(&parent.id)
            );
        }
        let (space, device_id) = self
            .zeron
            .resolve_target(args.project.as_deref(), args.device.as_deref())
            .await?;
        let harnesses = self.zeron.harnesses_on(Some(&device_id)).await?;
        let harness = match args.harness.as_deref() {
            // Cloud runs its own agents, with your Cloud accounts.
            Some(raw) if args.cloud => {
                let id: HarnessId = parse_enum("harness", raw).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    CLOUD_HARNESSES.contains(&id),
                    "Cloud runs Codex and Claude Code only"
                );
                id
            }
            None if args.cloud => CLOUD_HARNESSES[0],
            Some(raw) => {
                let id: HarnessId = parse_enum("harness", raw).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    harnesses.iter().any(|h| h.id == id && h.available()),
                    "harness {raw} is not available on device {device_id} (see list_harnesses with device)"
                );
                id
            }
            None => default_harness(&harnesses).with_context(|| format!("device {device_id}"))?,
        };
        // A Cloud machine's catalog is checked when it runs, not here.
        if let Some(model) = args.model.as_deref().filter(|_| !args.cloud) {
            let models = self.zeron.models_on(harness, Some(&device_id)).await?;
            anyhow::ensure!(
                models.iter().any(|m| m.id == model),
                "model {model:?} is not offered by {harness:?} on device {device_id}; available: {}",
                models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let reasoning: Option<ReasoningLevel> = match args.reasoning.as_deref() {
            Some(raw) => Some(parse_enum("reasoning level", raw).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let asked_sandbox: SandboxLevel = match args.sandbox.as_deref() {
            Some(raw) => parse_enum("sandbox", raw).map_err(anyhow::Error::msg)?,
            None => SandboxLevel::WorkspaceWrite,
        };
        let asked_access: AccessMode = match args.access.as_deref() {
            Some(raw) => parse_enum("access", raw).map_err(anyhow::Error::msg)?,
            None => ceiling,
        };
        // Never more than this chat's own: lowered, and the reply says so.
        let (sandbox, access, capped) =
            zeron_proto::cap_access((asked_sandbox, asked_access), (sandbox_ceiling, ceiling));
        let capped = capped.then(|| {
            format!(
                "This chat runs {} in a {} sandbox, so the new chat runs with no more: {}, {} sandbox (asked {}, {}).",
                ceiling.label(),
                sandbox_ceiling.label(),
                access.label(),
                sandbox.label(),
                asked_access.label(),
                asked_sandbox.label(),
            )
        });
        // A Cloud machine's catalog is checked when it runs, not here; an
        // engine predating modes lists none and runs with full access.
        if let Some(info) = harnesses.iter().find(|h| h.id == harness).filter(|_| !args.cloud) {
            let modes = &info.access_modes;
            anyhow::ensure!(
                access.is_full_access() || modes.contains(&access),
                "{harness:?} on device {device_id} cannot run {} (accessModes: {})",
                access.label(),
                modes.iter().map(|m| m.label()).collect::<Vec<_>>().join(", ")
            );
            anyhow::ensure!(
                !args.plan || info.supports_plan,
                "{harness:?} on device {device_id} has no plan mode"
            );
        }
        let config = ChatConfig {
            harness,
            model: args.model.clone(),
            reasoning,
            model_options: Default::default(),
            sandbox,
            access,
            plan: args.plan,
        };

        let chat_id = uuid::Uuid::new_v4().to_string();
        let mut mutate = json!({
            "op": "createChat",
            "chatId": chat_id,
            "deviceId": device_id,
            "config": config,
        });
        if let Some(space) = &space {
            mutate["spaceId"] = json!(space.id);
        }
        if let Some(parent) = &parent_chat_id {
            mutate["parentChatId"] = json!(parent);
            mutate["childKind"] = json!(match kind {
                ChatKind::Side => zeron_proto::ChildKind::Side,
                _ => zeron_proto::ChildKind::Subagent,
            });
        }
        if let Some(branch) = args
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            mutate["branch"] = json!(branch);
        }
        let mut cwd = args
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        // A projectless chat runs in its own scratch folder on a host that
        // creates them; naming it here keeps createChat and the first run
        // in agreement.
        if space.is_none()
            && cwd.is_none()
            && !args.cloud
            && self.zeron.devices().await?.iter().any(|d| {
                d.id == device_id
                    && d.capabilities
                        .iter()
                        .any(|c| c == zeron_proto::capabilities::PROJECTLESS_SCRATCH_V1)
            })
        {
            cwd = Some(zeron_proto::projectless::scratch_cwd(
                &chat_id,
                chrono::Utc::now(),
            ));
        }
        if let Some(cwd) = &cwd {
            mutate["cwd"] = json!(cwd);
        }
        if args.cloud {
            mutate["cloud"] = json!(true);
        }
        let created = self.zeron.mutate(mutate).await?;
        // A Cloud chat's machine and checkout come back from the engine.
        let (device_id, cwd) = if args.cloud {
            (
                created["deviceId"]
                    .as_str()
                    .map(str::to_owned)
                    .context("the engine didn't name the Cloud machine")?,
                created["cwd"].as_str().map(str::to_owned).or(cwd),
            )
        } else {
            (device_id, cwd)
        };
        if let Some(title) = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            self.zeron
                .mutate(json!({ "op": "renameChat", "chatId": chat_id, "title": title }))
                .await?;
        }

        let signed_out = harnesses
            .iter()
            .find(|h| h.id == harness)
            .is_some_and(|h| h.signed_in == Some(false));
        let mut result = json!({
            "chatId": chat_id,
            "kind": kind,
            "deviceId": device_id,
            "project": space.as_ref().map(|s| json!({ "id": s.id, "name": s.display_name(), "path": s.path })),
            "harness": harness,
            "model": args.model,
            "reasoning": reasoning,
            "title": args.title,
            "parentChatId": parent_chat_id,
        });
        if let Some(capped) = &capped {
            result["accessCapped"] = json!(capped);
        }
        if signed_out {
            result["warning"] = json!(format!(
                "{harness:?} does not look signed in on this device, so its first turn may fail. Pick a connected harness (list_harnesses) if it does."
            ));
        }
        if let Some(prompt) = args.prompt.filter(|p| !p.trim().is_empty()) {
            // The row may not have folded into WatchChats yet; build the
            // chat locally from what we just wrote rather than re-reading.
            let chat = Chat {
                id: chat_id.clone(),
                device_id: device_id.clone(),
                title: args.title.clone(),
                archived: false,
                cwd: cwd.clone(),
                branch: args.branch.clone(),
                checkout_id: None,
                source_context: None,
                config: Some(config),
                last_message_preview: None,
                last_message_at: None,
                created_at: chrono::Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                harness_session_harness: None,
                parent_chat_id: parent_chat_id.clone(),
                child_kind: match kind {
                    ChatKind::Side => Some(zeron_proto::ChildKind::Side),
                    ChatKind::Subagent => Some(zeron_proto::ChildKind::Subagent),
                    ChatKind::Chat => None,
                },
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
                room_gen: None,
                sleep: None,
            };
            let delivered = self
                .deliver(&chat, space.as_ref(), &harnesses, None, prompt, "run")
                .await?;
            result["sent"] = delivered.receipt;
            let pending = Arc::new(PendingTurn {
                baseline: None,
                start: TurnStart {
                    after_message: Some(delivered.message_id),
                },
            });
            self.pending_turns
                .lock()
                .await
                .insert(chat.id.clone(), pending.clone());
            if args.wait {
                result["turn"] = self
                    .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?;
            }
        }
        Ok(result)
    }

    async fn read_chat(&self, args: ReadChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let limit = args.limit.unwrap_or(40).clamp(1, 500);
        let view = self
            .zeron
            .window_of(
                &chat,
                TranscriptWindow {
                    limit,
                    offset: args.offset,
                    after: None,
                },
            )
            .await?;
        let rendered = render_entries(
            &view.entries,
            RenderOptions {
                include_reasoning: args.include_reasoning,
                include_tools: args.include_tools.unwrap_or(true),
            },
        );
        let total = view.info.total;
        let start = view.info.start;
        Ok(json!({
            "chatId": chat.id,
            "title": chat.title,
            "total": total,
            "returned": rendered.len(),
            "olderRemaining": start,
            "newerSkipped": total - start - view.entries.len(),
            "pendingInput": view.info.pending_input,
            "messages": rendered,
        }))
    }

    async fn send_message(&self, args: SendArgs) -> anyhow::Result<Value> {
        let text = args.text.trim();
        if text.is_empty() {
            anyhow::bail!("text is empty");
        }
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        if self.zeron.origin().chat_id.as_deref() == Some(chat.id.as_str()) {
            anyhow::bail!(
                "refusing to send a message to your own chat ({})",
                short(&chat.id)
            );
        }
        let (spaces, sessions) = tokio::try_join!(self.zeron.spaces(), self.zeron.sessions())?;
        // Only a chat with no harness of its own needs its host's catalog:
        // the others are delivered through the doc, host reachable or not.
        let harnesses = if chat.config.is_some() {
            Vec::new()
        } else {
            self.zeron.harnesses_on(Some(&chat.device_id)).await?
        };
        let space = chat
            .space_id
            .as_deref()
            .and_then(|id| spaces.iter().find(|s| s.id == id));
        let baseline = session_for(&sessions, &chat);
        let mode = args.mode.as_deref().unwrap_or("auto");
        let body = self.attribute(&chat, text).await;
        let mut result = json!({
            "chatId": chat.id,
            "title": chat.title,
        });
        let delivered = self
            .deliver(&chat, space, &harnesses, baseline.as_ref(), body, mode)
            .await?;
        result["sent"] = delivered.receipt;
        let pending = Arc::new(PendingTurn {
            baseline,
            start: TurnStart {
                after_message: Some(delivered.message_id),
            },
        });
        self.pending_turns
            .lock()
            .await
            .insert(chat.id.clone(), pending.clone());
        if args.wait {
            // The message is on its way: not being able to watch the turn
            // must not read as not having sent it.
            result["turn"] = self
                .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                .await
                .unwrap_or_else(|error| {
                    json!({
                        "outcome": "unknown",
                        "followError": format!("{error:#}"),
                        "replies": [],
                    })
                });
        }
        Ok(result)
    }

    async fn wait_for_turn(&self, args: WaitArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let pending = self.pending_turns.lock().await.get(&chat.id).cloned();
        let turn = match pending {
            Some(pending) => {
                self.await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?
            }
            None => {
                self.await_turn(
                    &chat,
                    None,
                    false,
                    wait_duration(args.timeout_secs),
                    TurnStart::default(),
                )
                .await?
            }
        };
        Ok(json!({ "chatId": chat.id, "title": chat.title, "turn": turn }))
    }

    async fn archive_chat(&self, args: ArchiveArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let archived = args.archived.unwrap_or(true);
        self.zeron
            .mutate(json!({ "op": "setChatArchived", "chatId": chat.id, "archived": archived }))
            .await?;
        Ok(json!({ "chatId": chat.id, "title": chat.title, "archived": archived }))
    }

    async fn show_visual(&self, args: ShowVisualArgs) -> anyhow::Result<Value> {
        let chat_id = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("show_visual only works from an agent Zeron launched in a chat")?;
        let assets = read_visual_assets(&args.assets).await?;
        let shown = self
            .zeron
            .call(
                zeron_rpc::methods::PUBLISH_VISUAL,
                json!({
                    "chatId": chat_id,
                    "title": args.title,
                    "script": args.script,
                    "assets": assets,
                }),
            )
            .await?;
        Ok(json!({
            "visual": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the visual; reply with only what it doesn't already say.",
        }))
    }

    async fn show_image(&self, args: ShowImageArgs) -> anyhow::Result<Value> {
        use base64::Engine as _;
        let chat_id = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("show_image only works from an agent Zeron launched in a chat")?;
        let bytes = read_image(&args.path).await?;
        let name = std::path::Path::new(&args.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        let shown = self
            .zeron
            .call(
                zeron_rpc::methods::PUBLISH_IMAGE,
                json!({
                    "chatId": chat_id,
                    "name": name,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                }),
            )
            .await?;
        Ok(json!({
            "image": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the image; reply with only what it doesn't already show.",
        }))
    }

    async fn wait_on_background(&self, args: WaitOnBackgroundArgs) -> anyhow::Result<Value> {
        let chat_id = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("wait_on_background only works from an agent Zeron launched in a chat")?;
        let waiting = args.waiting.unwrap_or(true);
        let marked = self
            .zeron
            .call(
                zeron_rpc::methods::AWAIT_BACKGROUND,
                json!({ "chatId": chat_id, "taskIds": args.task_ids, "waiting": waiting }),
            )
            .await?;
        let none_matched = marked["matched"].as_array().is_none_or(Vec::is_empty);
        let message = if none_matched {
            "None of those are running background tasks of yours now (they may have ended): nothing is waited on."
        } else if waiting {
            "This chat reads as waiting on that work after your turn, until it ends and you're woken."
        } else {
            "No longer waited on."
        };
        Ok(json!({
            "matched": marked["matched"],
            "unmatched": marked["unmatched"],
            "message": message,
        }))
    }

    async fn interrupt_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let command_id = self
            .zeron
            .queue_command(&chat.id, &SessionCommandPayload::Interrupt {})
            .await?;
        Ok(json!({ "chatId": chat.id, "commandId": command_id }))
    }

    async fn respond_to_input(&self, args: RespondArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let request_id = match args.request_id {
            Some(id) => id,
            None => self
                .zeron
                .window_of(&chat, TranscriptWindow::tail(1))
                .await?
                .info
                .pending_input
                .map(|p| p.request_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("chat {} has no pending question", short(&chat.id))
                })?,
        };
        if args.answers.is_empty() {
            anyhow::bail!("answers is empty");
        }
        let answers = args
            .answers
            .into_iter()
            .map(|a| UserInputAnswer {
                question_id: a.question_id,
                labels: a.labels,
            })
            .collect();
        let command_id = self
            .zeron
            .queue_command(
                &chat.id,
                &SessionCommandPayload::RespondInput {
                    request_id: request_id.clone(),
                    answers,
                },
            )
            .await?;
        Ok(json!({ "chatId": chat.id, "requestId": request_id, "commandId": command_id }))
    }

    // ---- shared pieces -------------------------------------------------------

    /// Prefix the sender's identity when this server speaks for a chat, so
    /// the receiving agent (and the human reading that transcript) can tell
    /// an agent-to-agent message from a typed one. Reply instructions always
    /// use the full id: display prefixes need not uniquely identify a chat.
    async fn attribute(&self, target: &Chat, text: &str) -> String {
        let Some(origin_id) = self.zeron.origin().chat_id.as_deref() else {
            return text.to_owned();
        };
        if origin_id == target.id {
            return text.to_owned();
        }
        let title = match self.zeron.resolve_chat(origin_id).await {
            Ok(chat) => chat.title,
            Err(_) => None,
        };
        let label = match title {
            Some(title) if !title.trim().is_empty() => {
                format!("{} ({})", title.trim(), short(origin_id))
            }
            _ => short(origin_id).to_owned(),
        };
        zeron_proto::agent_messages::agent_message(&label, origin_id, text)
    }

    /// Pick and perform the delivery the composer would.
    async fn deliver(
        &self,
        chat: &Chat,
        space: Option<&Space>,
        harnesses: &[HarnessInfo],
        session: Option<&Session>,
        text: String,
        mode: &str,
    ) -> anyhow::Result<Delivered> {
        let harness = chat
            .config
            .as_ref()
            .map(|c| c.harness)
            .map_or_else(|| default_harness(harnesses), Ok)?;
        let (status, _) = status_of(session);
        // A quiet tool call can outlive the UI's stale-status window. Route
        // through steering and let the host decide whether a live run exists.
        let live = session.map(|s| s.status).unwrap_or(SessionStatus::Idle);
        let chosen = match mode {
            "auto" => match live {
                SessionStatus::Idle | SessionStatus::Errored => "run",
                SessionStatus::Working => "steer",
                SessionStatus::AwaitingInput => anyhow::bail!(
                    "chat {} is waiting for an answer; use respond_to_input (or mode 'queue' to hold this message for after the turn)",
                    short(&chat.id)
                ),
            },
            "run" | "steer" | "queue" => mode,
            other => anyhow::bail!("unknown mode {other:?}; expected auto, run, steer, or queue"),
        };
        let new_message_id = uuid::Uuid::new_v4().to_string();
        let (id, message_id) = match chosen {
            "run" => {
                let config = chat.config.clone();
                let cwd = chat
                    .cwd
                    .clone()
                    .or_else(|| space.map(|s| s.path.clone()))
                    .unwrap_or_else(|| "~".into());
                let request = RunRequest {
                    mcp: None,
                    prompt: text,
                    harness: Some(harness),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd,
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    access: config.as_ref().map(|c| c.access).unwrap_or_default(),
                    plan: config.as_ref().is_some_and(|c| c.plan),
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                    env: Default::default(),
                    extra_mcp: Vec::new(),
                };
                let id = self
                    .zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Run {
                            request,
                            message_id: new_message_id.clone(),
                        },
                    )
                    .await?;
                (id, new_message_id)
            }
            "steer" => {
                let id = self
                    .zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Steer {
                            prompt: text,
                            message_id: Some(new_message_id.clone()),
                        },
                    )
                    .await?;
                (id, new_message_id)
            }
            // A held message keeps its row's id when it's sent.
            _ => {
                let id = self.zeron.queue_message(&chat.id, &text).await?;
                (id.clone(), id)
            }
        };
        Ok(Delivered {
            receipt: json!({
                "delivery": chosen,
                "id": id,
                "chatStatusAtSend": status,
            }),
            message_id,
        })
    }

    async fn await_pending_turn(
        &self,
        chat: &Chat,
        pending: Arc<PendingTurn>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let turn = self
            .await_turn(
                chat,
                pending.baseline.as_ref(),
                true,
                timeout,
                pending.start.clone(),
            )
            .await?;
        if turn["timedOut"] == false {
            let mut turns = self.pending_turns.lock().await;
            // A concurrent send may already have installed a newer baseline.
            if turns
                .get(&chat.id)
                .is_some_and(|p| Arc::ptr_eq(p, &pending))
            {
                turns.remove(&chat.id);
            }
        }
        Ok(turn)
    }

    /// Wait, then report the outcome with the assistant messages that
    /// are not in `start`'s baseline.
    async fn await_turn(
        &self,
        chat: &Chat,
        baseline: Option<&Session>,
        expect_turn: bool,
        timeout: Duration,
        start: TurnStart,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        let (mut outcome, session) = self
            .zeron
            .wait_for_turn(chat, baseline, expect_turn, timeout)
            .await?;
        // Registry session updates can arrive before the separate transcript
        // doc: follow it until the reply lands, inside the same deadline.
        let mut pending_input = None;
        let mut replies: Vec<RenderedMessage> = Vec::new();
        let mut follow = None;
        let mut transcript_error: Option<String> = None;
        let mut failing_since: Option<Instant> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if outcome == TurnOutcome::Completed && expect_turn && transcript_error.is_none() {
                    outcome = TurnOutcome::TimedOut;
                }
                break;
            }
            let next = tokio::time::timeout(remaining, async {
                if follow.is_none() {
                    follow = Some(
                        self.zeron
                            .follow_transcript(&chat.id, Some(&chat.device_id), start.window())
                            .await?,
                    );
                }
                match follow.as_mut() {
                    Some(follow) => follow.next().await,
                    None => Ok(()),
                }
            })
            .await;
            match next {
                Err(_) => continue, // Out of time: the top of the loop settles it.
                Ok(Err(error)) => {
                    // A stream that ends after reading fine is a lifecycle
                    // boundary (the same contract as the UI's): attach again.
                    if follow.take().is_some_and(|f: TranscriptFollow| f.opened()) {
                        tokio::time::sleep(Duration::from_millis(300).min(remaining)).await;
                        continue;
                    }
                    // Not the send's failure. Say so if it doesn't come back.
                    transcript_error = Some(format!("{error:#}"));
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if !expect_turn
                        || outcome != TurnOutcome::Completed
                        || since.elapsed() >= FOLLOW_GRACE
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(300).min(remaining)).await;
                    continue;
                }
                Ok(Ok(())) => {
                    transcript_error = None;
                    failing_since = None;
                }
            }
            let Some(view) = follow.as_ref().map(|f| f.view()) else {
                continue;
            };
            pending_input = view.info.pending_input.map(|p| json!(p));
            replies = render_entries(&view.entries, RenderOptions::default())
                .into_iter()
                .filter(|m| m.role == zeron_doc::MessageRole::Assistant)
                .collect();
            if !expect_turn
                || outcome != TurnOutcome::Completed
                || replies
                    .iter()
                    .any(|m| m.status != Some(zeron_doc::MessageStatus::Streaming))
            {
                break;
            }
        }
        let now = chrono::Utc::now();
        let status = zeron_proto::view::display_status(chat, session.as_ref(), now);
        let stale = session
            .as_ref()
            .is_some_and(|s| zeron_proto::view::session_stale(s, now));
        let mut turn = json!({
            "outcome": outcome,
            "status": status,
            "sessionStatus": session.as_ref().map(|s| s.status),
            "lastTurnOutcome": session.as_ref().and_then(|s| s.last_turn_outcome),
            "statusStale": stale,
            "terminalActivity": session.as_ref().and_then(|s| s.terminal_activity.as_ref()),
            "timedOut": outcome == TurnOutcome::TimedOut,
            "pendingInput": if stale { None } else { pending_input },
            "replies": replies,
        });
        if let Some(error) = transcript_error {
            turn["transcriptError"] = json!(error);
        }
        Ok(turn)
    }
}

/// claude-code when it is ready here, else the first ready harness — a
/// provider that is installed but signed out only when nothing else is.
fn default_harness(harnesses: &[HarnessInfo]) -> anyhow::Result<HarnessId> {
    let real = |h: &&HarnessInfo| h.id != HarnessId::Mock;
    harnesses
        .iter()
        .find(|h| h.id == HarnessId::ClaudeCode && h.connected())
        .or_else(|| harnesses.iter().filter(real).find(|h| h.connected()))
        .or_else(|| {
            harnesses
                .iter()
                .find(|h| h.id == HarnessId::ClaudeCode && h.available())
        })
        .or_else(|| {
            harnesses
                .iter()
                .find(|h| h.available() && h.id != HarnessId::Mock)
        })
        .map(|h| h.id)
        .ok_or_else(|| {
            anyhow::anyhow!("no harness is available on this device (see list_harnesses)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zeron::Origin;
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::Mutex;
    use zeron_rpc::{RpcError, RpcReply, RpcService, memory_client, methods};

    /// Two devices with distinct catalogs and repeated project paths, plus
    /// two chats with a two-message transcript. Writes are recorded for assertions.
    #[derive(Default)]
    struct World {
        writes: Mutex<Vec<(String, Value)>>,
        dispatch_barrier: Option<tokio::sync::Barrier>,
        beta_parent: Option<String>,
        /// Adds two more chats below Beta: Beta → Gamma → Delta.
        deep: bool,
        /// `ListAgentAccounts` reply; absent = the method is unknown.
        accounts: Option<Value>,
        remote_accounts: Option<Value>,
        beta_remote: bool,
        /// `dev-remote` is asleep: every read addressed to it fails, the way
        /// a dial to a sleeping Cloud machine does.
        remote_dark: bool,
        extra_chats: Vec<Value>,
        catalog_error: Option<&'static str>,
        reads: Mutex<Vec<(String, Value)>>,
        /// `DeviceStats` reply; absent = an engine without the method.
        stats: Option<Value>,
    }

    fn stream(item: Value) -> RpcReply {
        RpcReply::Stream(futures::stream::iter(vec![item]).boxed())
    }

    #[async_trait]
    impl RpcService for World {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            if matches!(
                method,
                methods::LIST_HARNESSES | methods::LIST_MODELS | methods::LIST_AGENT_ACCOUNTS
            ) {
                self.reads
                    .lock()
                    .unwrap()
                    .push((method.into(), params.clone()));
                if self.catalog_error == Some(method) {
                    return Err(RpcError::Failed("catalog unavailable".into()));
                }
            }
            let remote = params["targetDeviceId"] == "dev-remote";
            if remote && self.remote_dark && method != methods::LIST_HARNESSES {
                self.reads
                    .lock()
                    .unwrap()
                    .push((method.into(), params.clone()));
                return Err(RpcError::Transport(
                    "peer dev-remote: device is offline (no recent presence)".into(),
                ));
            }
            Ok(match method {
                methods::LOCAL_DEVICE => RpcReply::Value(json!({ "deviceId": "dev-local" })),
                methods::ENGINE_INFO => RpcReply::Value(json!({
                    "deviceId": "dev-local", "workspaceScope": "local"
                })),
                methods::WATCH_DEVICES => stream(json!([{
                    "id": "dev-local", "name": "Laptop", "platform": "linux",
                    "lastSeenAt": null
                }, {"id": "dev-remote", "name": "Worker", "platform": "linux", "lastSeenAt": null}])),
                methods::WATCH_SPACES => stream(json!([{
                    "id": "space-1", "deviceId": "dev-local", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-remote", "deviceId": "dev-remote", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-unique", "deviceId": "dev-remote", "path": "/repo/unique",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }])),
                methods::WATCH_CHATS => {
                    let mut chats = json!([
                    {
                        "id": "chat-alpha-1", "deviceId": "dev-local", "title": "Alpha",
                        "archived": false, "spaceId": "space-1",
                        "config": { "harness": "claude-code", "model": "opus", "reasoning": null, "sandbox": "workspace-write" },
                        "createdAt": "2026-09-01T00:00:00Z"
                    },
                    {
                        "id": "chat-beta-2", "deviceId": if self.beta_remote { "dev-remote" } else { "dev-local" }, "title": "Beta",
                        "parentChatId": self.beta_parent,
                        "archived": false, "spaceId": "space-1",
                        "createdAt": "2026-09-02T00:00:00Z"
                    }
                    ]);
                    if self.deep {
                        for (id, title, parent) in [
                            ("chat-gamma-3", "Gamma", "chat-beta-2"),
                            ("chat-delta-4", "Delta", "chat-gamma-3"),
                        ] {
                            chats.as_array_mut().unwrap().push(json!({
                                "id": id, "deviceId": "dev-local", "title": title,
                                "parentChatId": parent, "archived": false, "spaceId": "space-1",
                                "createdAt": "2026-09-03T00:00:00Z"
                            }));
                        }
                    }
                    chats
                        .as_array_mut()
                        .unwrap()
                        .extend(self.extra_chats.clone());
                    stream(chats)
                }
                methods::LIST_AGENT_ACCOUNTS if remote && self.remote_accounts.is_some() => {
                    RpcReply::Value(self.remote_accounts.clone().unwrap())
                }
                methods::LIST_AGENT_ACCOUNTS if self.accounts.is_some() && !remote => {
                    RpcReply::Value(self.accounts.clone().unwrap())
                }
                methods::WATCH_SESSIONS => stream(json!([])),
                methods::LIST_HARNESSES if remote => RpcReply::Value(json!([
                    {"id":"codex", "name":"Remote Codex", "installed":true},
                    {"id":"cursor", "name":"Disabled Cursor", "installed":true, "enabled":false}
                ])),
                methods::LIST_HARNESSES => RpcReply::Value(json!([
                    { "id": "claude-code", "name": "Claude Code", "supportsSteering": true,
                      "steeringMode": "step-boundary", "reasoningLevels": [], "installed": true, "enabled": true,
                      "accessModes": ["supervised", "auto-accept-edits", "full-access"], "supportsPlan": true },
                    { "id": "codex", "name": "Codex", "supportsSteering": true,
                      "steeringMode": "turn-boundary", "reasoningLevels": [], "installed": false, "enabled": true }
                ])),
                methods::LIST_MODELS if remote => RpcReply::Value(json!([
                    {"id":"remote-model", "label":"Remote model"}
                ])),
                methods::LIST_MODELS => RpcReply::Value(json!([
                    { "id": "opus", "label": "Opus" }, { "id": "sonnet", "label": "Sonnet" }
                ])),
                methods::WATCH_DOC_MESSAGES => stream(json!({ "reset": [
                    { "id": "u1", "role": "user", "createdAt": 1, "deviceId": "dev-local",
                      "parts": [{ "kind": "text", "id": "t", "text": "hi" }] },
                    { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                      "status": "complete",
                      "parts": [{ "kind": "text", "id": "t", "text": "hello back" }] }
                ]})),
                methods::PUBLISH_VISUAL => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "v:1", "title": params["title"] }))
                }
                methods::PUBLISH_IMAGE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "img:1", "name": params["name"] }))
                }
                methods::MUTATE | methods::QUEUE_COMMAND | methods::QUEUE_MESSAGE => {
                    // A Cloud chat gets a machine of its own from the engine.
                    let cloud_create = method == methods::MUTATE
                        && params["op"] == "createChat"
                        && params["cloud"] == true;
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    if cloud_create {
                        return Ok(RpcReply::Value(json!({
                            "ok": true, "deviceId": "cloud-child", "cwd": "/home/user/app",
                        })));
                    }
                    if method == methods::QUEUE_COMMAND
                        && let Some(barrier) = &self.dispatch_barrier
                    {
                        // No dispatch can finish until both chats have work.
                        // A sequential batch deadlocks here, before any waits.
                        barrier.wait().await;
                    }
                    RpcReply::Value(json!({ "commandId": "cmd-1", "id": "q-1" }))
                }
                methods::TRADING_PROPOSE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({
                        "id": "p-new",
                        "action": params["action"],
                        "reason": params["reason"],
                        "status": "pending",
                        "createdAt": 100,
                    }))
                }
                methods::TRADING_RULE_ADD => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({
                        "id": "rul-1",
                        "name": params["name"],
                        "condition": params["condition"],
                        "proposal": params.get("proposal").cloned().unwrap_or(Value::Null),
                        "reason": params["reason"],
                        "status": "watching",
                        "createdAt": 100,
                        "expiresAt": 1000,
                    }))
                }
                methods::TRADING_RULE_CANCEL => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({
                        "id": params["ruleId"],
                        "status": "cancelled",
                    }))
                }
                methods::TRADING_RULE_LIST => RpcReply::Value(json!([])),
                methods::TRADING_SNAPSHOT => RpcReply::Value(json!({
                    "version": "0.1.0-mock",
                    "connected": false,
                    "systems": [{
                        "id": "01_a",
                        "name": "A",
                        "running": false,
                        "active": false,
                        "symbol": "EURUSD",
                        "timeframe": "M15",
                        "pnlDay": 127.4,
                        "capital": 12500,
                        "pnlPct": 1.02,
                        "lastActivityAt": 900,
                        "description": "cruce de medias",
                        "rules": { "entrada": "cruce" },
                    }, {
                        "id": "02_b",
                        "name": "B",
                        "running": true,
                        "active": true,
                        "symbol": "NQ",
                        "timeframe": "M5",
                        "pnlDay": -42.1,
                        "capital": 8000,
                        "pnlPct": -0.53,
                        "lastActivityAt": 950,
                    }, {
                        "id": "03_c",
                        "name": "C",
                        "running": false,
                        "active": false,
                        "symbol": "XAUUSD",
                        "timeframe": "H1",
                        "pnlDay": -12.5,
                        "lastActivityAt": 800,
                    }],
                    "positions": [],
                    "logs": [],
                    "tasks": [],
                    "rules": [],
                    "proposals": [
                        { "id": "p-new", "action": { "kind": "close_all" }, "reason": "ya",
                          "status": "pending", "createdAt": 100 },
                        { "id": "p-old", "action": { "kind": "panic" }, "reason": "antes",
                          "status": "done", "result": "todo cerrado", "createdAt": 1,
                          "resolvedAt": 50 },
                        { "id": "p-mid", "action": { "kind": "close_all" }, "reason": "medio",
                          "status": "failed", "result": "sin contrato", "createdAt": 2,
                          "resolvedAt": 20 },
                    ],
                })),
                methods::DEVICE_STATS if self.stats.is_some() => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.into(), params.clone()));
                    RpcReply::Value(self.stats.clone().unwrap_or_default())
                }
                other => return Err(RpcError::UnknownMethod(other.into())),
            })
        }
    }

    fn tools(world: Arc<World>, origin: Origin) -> Tools {
        let client = memory_client(world);
        Tools::new(Arc::new(Zeron::with_client(client, origin)))
    }

    #[tokio::test]
    async fn catalog_is_well_formed() {
        let defs = catalog();
        let mut names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), defs.len(), "tool names must be unique");
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object", "{}", def.name);
            assert!(!def.description.is_empty());
        }
    }

    #[test]
    fn the_brief_reads_the_day_like_the_desk_does() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 21, 18, 0, 0)
            .single()
            .unwrap();
        let at = |minutes_ago: i64| (now - chrono::Duration::minutes(minutes_ago)).timestamp_millis();
        let snapshot = json!({
            "version": "0.1.0-mock",
            "account": { "id": "DU123", "equity": 10085.3, "pnlDay": 85.3 },
            "systems": [
                {"id":"01_a","name":"EURUSD MT5","running":true,"pnlDay":127.4,"pnlPct":1.02},
                {"id":"02_b","name":"NQ TWS","running":false,"pnlDay":-42.1,"pnlPct":-0.53}
            ],
            "positions": [
                {"symbol":"EURUSD","pnl":80.5},
                {"symbol":"XAUD","pnl":-68.0}
            ],
            "tasks": [
                {"id":"t1","kind":"stop_systems","status":"pending","runAt": at(-60)},
                {"id":"t2","kind":"start_systems","status":"ran","runAt": at(120),"resolvedAt": at(120)},
                {"id":"t3","kind":"stop_systems","status":"missed","runAt": at(300),"resolvedAt": at(240)}
            ],
            "rules": [
                {"id":"r1","name":"NQ parado","status":"fired","createdAt": at(90),"firedAt": at(30)},
                {"id":"r2","name":"EURUSD rojo","status":"watching","createdAt": at(10)}
            ],
            "proposals": [
                {"id":"p1","action":{"kind":"close_all"},"reason":"cierra todo","status":"pending","createdAt": at(5)},
                {"id":"p2","action":{"kind":"stop_systems"},"reason":"para el NQ","status":"done","result":"1 sistema parado.","createdAt": at(100),"resolvedAt": at(60)},
                {"id":"p3","action":{"kind":"panic"},"reason":"pánico","status":"rejected","createdAt": at(200),"resolvedAt": at(150)},
                {"id":"p4","action":{"kind":"close_all"},"reason":"ayer","status":"done","result":"viejo","createdAt": at(2000),"resolvedAt": at(1500)}
            ],
            "logs": [
                {"ts": at(2),"level":"error","message":"TWS: reconectando."},
                {"ts": at(3),"level":"warn","message":"sin datos"},
                {"ts": at(2000),"level":"error","message":"de ayer"}
            ]
        });
        let brief = trading_brief_value(&snapshot, now);
        let text = brief["text"].as_str().unwrap();
        assert!(text.contains("Resumen de la mesa (demo, datos inventados) — 21/11 18:00"), "{text}");
        assert!(
            text.contains("Cuenta DU123: equity 10.085,30 $, P&L del día +85,30 $."),
            "{text}"
        );
        assert!(
            text.contains(
                "Sistemas: 1 de 2 en marcha. Mejor: EURUSD MT5 +127,40 $ (+1,02 %); peor: NQ TWS -42,10 $ (-0,53 %)."
            ),
            "{text}"
        );
        assert!(text.contains("Posiciones: 2 abiertas, P&L +12,50 $."), "{text}");
        assert!(
            text.contains("Tareas: 1 pendiente (a las 19:00); hoy 1 hecha, 1 perdida."),
            "{text}"
        );
        assert!(
            text.contains("Reglas: 1 vigilando; hoy 1 aviso y 0 caducadas."),
            "{text}"
        );
        assert!(
            text.contains("Peticiones: 1 esperando respuesta; hoy 1 aprobada, 1 rechazada y 0 fallidas."),
            "{text}"
        );
        assert!(
            text.contains("Hoy: aprobada «para el NQ» → 1 sistema parado; rechazada «pánico»."),
            "{text}"
        );
        assert!(
            text.contains("Registro hoy: 1 aviso y 1 error. Último error: TWS: reconectando. (17:58)."),
            "{text}"
        );
        assert_eq!(brief["mode"], "demo");
        assert_eq!(brief["highlights"]["systemsRunning"], 1);
        assert_eq!(brief["highlights"]["pendingProposals"], 1);
        assert_eq!(brief["highlights"]["rulesWatching"], 1);
    }

    #[test]
    fn the_brief_survives_an_empty_or_missing_desk() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 21, 9, 5, 0)
            .single()
            .unwrap();
        for snapshot in [
            json!({"version":"1","systems":[],"positions":[],"tasks":[],"rules":[],"proposals":[],"logs":[]}),
            json!({}),
        ] {
            let brief = trading_brief_value(&snapshot, now);
            let text = brief["text"].as_str().unwrap();
            assert!(text.contains("Cuenta: ninguna conectada."), "{text}");
            assert!(text.contains("Sistemas: ninguno."), "{text}");
            assert!(text.contains("Posiciones: ninguna abierta."), "{text}");
            assert!(text.contains("Tareas: 0 pendientes."), "{text}");
            assert!(text.contains("Reglas: 0 vigilando."), "{text}");
            assert!(text.contains("Peticiones: 0 esperando respuesta."), "{text}");
            assert!(text.contains("Registro hoy: sin avisos ni errores."), "{text}");
        }
        assert_eq!(trading_brief_value(&json!({"version":"1"}), now)["mode"], "live");
    }

    #[test]
    fn spanish_numbers_group_and_sign_the_desk_s_way() {
        assert_eq!(es_decimal(1_234_567.891, 2), "1.234.567,89");
        assert_eq!(es_decimal(-42.1, 2), "-42,10");
        assert_eq!(es_decimal(-0.001, 2), "0,00");
        assert_eq!(es_money(10085.3), "10.085,30 $");
        assert_eq!(es_signed_money(85.3), "+85,30 $");
        assert_eq!(es_signed_money(-68.0), "-68,00 $");
        assert_eq!(es_signed_pct(1.02), "+1,02 %");
        assert_eq!(es_signed_pct(-0.53), "-0,53 %");
    }

    #[tokio::test]
    async fn a_proposal_returns_at_once_and_leaves_the_card_in_the_chat() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
                ..Default::default()
            },
        );
        let answer = tools
            .call(
                TRADING_PROPOSE_TOOL,
                json!({
                    "action": "stop_systems",
                    "system_ids": ["01_a", "02_b", "01_a", " "],
                    "reason": "parar por la noche",
                }),
            )
            .await
            .unwrap();
        assert_eq!(answer["status"], "pending", "{answer}");
        assert!(answer["note"].as_str().unwrap().contains("card"), "{answer}");
        let writes = world.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1, "a non-blocking proposal never polls");
        assert_eq!(writes[0].0, methods::TRADING_PROPOSE);
        assert_eq!(writes[0].1["chatId"], "chat-alpha-1");
        assert_eq!(writes[0].1["action"]["kind"], "stop_systems");
        assert_eq!(
            writes[0].1["action"]["system_ids"],
            json!(["01_a", "02_b"]),
            "one card, blanks and duplicates dropped"
        );
    }

    #[tokio::test]
    async fn a_single_system_id_still_makes_a_group_of_one() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        tools
            .call(
                TRADING_PROPOSE_TOOL,
                json!({
                    "action": "start_system",
                    "system_id": "07_oro",
                    "reason": "arrancar el oro",
                }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap().clone();
        assert_eq!(writes[0].1["action"]["kind"], "start_systems");
        assert_eq!(writes[0].1["action"]["system_ids"], json!(["07_oro"]));

        let error = tools
            .call(
                TRADING_PROPOSE_TOOL,
                json!({ "action": "start_systems", "reason": "sin ids" }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("needs system_ids"), "{error}");
    }

    #[tokio::test]
    async fn trading_schedule_proposes_one_task_and_checks_its_time() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let answer = tools
            .call(
                TRADING_SCHEDULE_TOOL,
                json!({
                    "action": "stop_systems",
                    "system_ids": ["01_a", "03_c"],
                    "delay_minutes": 15,
                    "reason": "parar antes del cierre",
                }),
            )
            .await
            .unwrap();
        assert_eq!(answer["status"], "pending");
        let writes = world.writes.lock().unwrap().clone();
        assert_eq!(writes[0].1["action"]["kind"], "schedule_task");
        assert_eq!(writes[0].1["action"]["task_kind"], "stop-systems");
        assert_eq!(
            writes[0].1["action"]["system_ids"],
            json!(["01_a", "03_c"])
        );
        assert_eq!(writes[0].1["action"]["delay_minutes"], 15.0);

        let at = tools
            .call(
                TRADING_SCHEDULE_TOOL,
                json!({
                    "action": "start_system",
                    "system_id": "01_a",
                    "at_time": "22:05",
                    "reason": "arrancar por la noche",
                }),
            )
            .await
            .unwrap();
        assert_eq!(at["status"], "pending");
        let writes = world.writes.lock().unwrap().clone();
        let action = &writes[1].1["action"];
        assert_eq!(action["task_kind"], "start-systems");
        assert_eq!(action["system_ids"], json!(["01_a"]));
        assert_eq!(action["at_time"], "22:05");
        assert!(action.get("delay_minutes").is_none(), "{action}");

        // Both or neither time, a non-positive delay and no systems are caught
        // here, before the desk sees a malformed task.
        for bad in [
            json!({"action":"start_systems","system_ids":["01_a"],"reason":"x"}),
            json!({"action":"start_systems","system_ids":["01_a"],"delay_minutes":5,"at_time":"22:00","reason":"x"}),
            json!({"action":"start_systems","system_ids":["01_a"],"delay_minutes":0,"reason":"x"}),
            json!({"action":"start_systems","delay_minutes":5,"reason":"x"}),
        ] {
            let error = tools
                .call(TRADING_SCHEDULE_TOOL, bad)
                .await
                .unwrap_err();
            assert!(
                error.contains("delay_minutes")
                    || error.contains("at_time")
                    || error.contains("needs system_ids"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn trading_watch_leaves_one_rule_and_refuses_a_missing_threshold() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let answer = tools
            .call(
                TRADING_WATCH_TOOL,
                json!({
                    "condition": "system_pnl_pct",
                    "system_id": "01_a",
                    "at_most": -2,
                    "action": "stop_systems",
                    "system_ids": ["01_a"],
                    "name": "EURUSD en rojo",
                    "reason": "parar si pierde 2 %",
                    "expires_minutes": 60,
                }),
            )
            .await
            .unwrap();
        assert_eq!(answer["rule"]["id"], "rul-1");
        let writes = world.writes.lock().unwrap().clone();
        assert_eq!(writes[0].0, methods::TRADING_RULE_ADD);
        let input = &writes[0].1;
        assert_eq!(input["condition"]["kind"], "system_pnl_pct");
        assert_eq!(input["condition"]["system_id"], "01_a");
        assert_eq!(input["condition"]["at_most"], -2.0);
        assert!(input["condition"].get("at_least").is_some_and(Value::is_null), "{input}");
        assert_eq!(input["proposal"]["kind"], "stop_systems");
        assert_eq!(input["proposal"]["system_ids"], json!(["01_a"]));
        assert_eq!(input["name"], "EURUSD en rojo");
        assert_eq!(input["expiresMinutes"], 60.0);

        // Notify only: no proposal key at all.
        let answer = tools
            .call(
                TRADING_WATCH_TOOL,
                json!({
                    "condition": "account_pnl_pct",
                    "at_least": 3,
                    "reason": "avisar al +3 %",
                }),
            )
            .await
            .unwrap();
        assert_eq!(answer["rule"]["status"], "watching");
        let writes = world.writes.lock().unwrap().clone();
        let input = &writes[1].1;
        assert!(input.get("proposal").is_none(), "{input}");
        assert_eq!(input["name"], "avisar al +3 %", "the reason is the name");

        for bad in [
            json!({ "condition": "system_pnl_pct", "system_id": "01_a", "reason": "x" }),
            json!({ "condition": "system_pnl_pct", "at_most": -1, "reason": "x" }),
            json!({ "condition": "system_idle", "system_id": "01_a", "reason": "x" }),
            json!({ "condition": "position_pnl_usd", "at_most": -10, "reason": "x" }),
            json!({ "condition": "nope", "reason": "x" }),
            json!({ "condition": "system_stopped", "system_id": "01_a", "reason": " " }),
        ] {
            assert!(tools.call(TRADING_WATCH_TOOL, bad).await.is_err());
        }

        let cancelled = tools
            .call(TRADING_UNWATCH_TOOL, json!({ "rule_id": "rul-1" }))
            .await
            .unwrap();
        assert_eq!(cancelled["rule"]["status"], "cancelled");
        let writes = world.writes.lock().unwrap().clone();
        assert_eq!(writes.last().unwrap().0, methods::TRADING_RULE_CANCEL);
        assert_eq!(writes.last().unwrap().1["ruleId"], "rul-1");
    }

    #[tokio::test]
    async fn the_snapshot_brings_the_queue_the_verdicts_and_the_system_rules() {
        let world = Arc::new(World::default());
        let answer = tools(world, Origin::default())
            .call(TRADING_SNAPSHOT_TOOL, json!({}))
            .await
            .unwrap();
        assert_eq!(answer["mode"], "demo (invented data, nothing real)");
        assert_eq!(answer["pendingProposals"].as_array().unwrap().len(), 1);
        let recent = answer["recentProposals"].as_array().unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["id"], "p-old", "newest verdict first");
        assert_eq!(recent[0]["result"], "todo cerrado");
        assert_eq!(answer["systems"][0]["description"], "cruce de medias");
        assert_eq!(answer["systems"][0]["rules"]["entrada"], "cruce");
        assert_eq!(answer["systems"][0]["capital"], 12500);
        assert_eq!(answer["systems"][0]["pnlPct"], 1.02);
        assert_eq!(answer["systems"][0]["lastActivityAt"], 900);
    }

    #[tokio::test]
    async fn trading_select_answers_with_the_ids_and_why_they_match() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let answer = tools
            .call(TRADING_SELECT_TOOL, json!({ "running": false }))
            .await
            .unwrap();
        assert_eq!(answer["count"], 2, "{answer}");
        assert_eq!(answer["criteria"]["running"], false);
        assert_eq!(answer["matched"][0]["id"], "01_a");
        assert_eq!(answer["matched"][1]["id"], "03_c");
        let why = answer["matched"][0]["why"].as_str().unwrap();
        assert!(why.contains("parado"), "{why}");
        assert!(why.contains("gana 1,02 % hoy"), "{why}");
        assert!(
            answer["note"].as_str().unwrap().contains("system_ids"),
            "{answer}"
        );

        let error = tools
            .call(
                TRADING_SELECT_TOOL,
                json!({ "pnl_pct_min": 2, "pnl_pct_max": 1 }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("pnl_pct_min"), "{error}");
    }

    #[test]
    fn trading_select_filters_by_symbol_state_money_and_idle_time() {
        let now = 1_000_000_000_000i64;
        let systems = vec![
            json!({"id":"a","name":"A","symbol":"EURUSD","running":true,
                   "pnlDay":50.0,"capital":10000.0,"pnlPct":0.5,
                   "lastActivityAt": now - 60_000}),
            json!({"id":"b","name":"B","symbol":"NQ","running":true,
                   "pnlDay":-120.0,"capital":8000.0,"pnlPct":-1.5,
                   "lastActivityAt": now - 45 * 60_000}),
            json!({"id":"c","name":"C","symbol":"EURUSD","running":false,
                   "pnlDay":-3.0,"lastActivityAt": now - 5 * 60_000}),
            json!({"id":"d","name":"D","symbol":"XAUUSD","running":false,
                   "pnlDay":-30.0,"capital":5000.0,"pnlPct":-0.6}),
        ];
        let ids = |args: TradingSelectArgs| {
            select_systems(&systems, &args, now)
                .iter()
                .map(|system| system["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(TradingSelectArgs::default()),
            ["a", "b", "c", "d"],
            "no filters: every system"
        );
        assert_eq!(
            ids(TradingSelectArgs {
                symbol: Some("eurusd".into()),
                ..Default::default()
            }),
            ["a", "c"]
        );
        assert_eq!(
            ids(TradingSelectArgs {
                running: Some(false),
                ..Default::default()
            }),
            ["c", "d"]
        );
        assert_eq!(
            ids(TradingSelectArgs {
                pnl_pct_max: Some(-1.0),
                ..Default::default()
            }),
            ["b"],
            "a system without capital has no percent"
        );
        assert_eq!(
            ids(TradingSelectArgs {
                pnl_usd_min: Some(0.0),
                ..Default::default()
            }),
            ["a"]
        );
        assert_eq!(
            ids(TradingSelectArgs {
                idle_minutes: Some(30),
                ..Default::default()
            }),
            ["b", "d"],
            "never having logged counts as idle"
        );
        assert_eq!(
            ids(TradingSelectArgs {
                pnl_usd_max: Some(-10.0),
                idle_minutes: Some(5),
                ..Default::default()
            }),
            ["b", "d"]
        );

        let losers = select_systems(
            &systems,
            &TradingSelectArgs {
                pnl_pct_max: Some(-1.0),
                ..Default::default()
            },
            now,
        );
        assert_eq!(
            losers[0]["why"],
            "en marcha · pierde 1,50 % hoy · activo hace 45 min"
        );
        let never = &select_systems(&systems, &TradingSelectArgs::default(), now)[3];
        assert!(
            never["why"]
                .as_str()
                .unwrap()
                .contains("sin actividad registrada"),
            "{never}"
        );
    }

    #[tokio::test]
    async fn device_stats_asks_the_chosen_device_and_names_it() {
        let def = catalog().into_iter().find(|d| d.name == DEVICE_STATS).expect("listed");
        assert_eq!(def.input_schema["properties"]["device"]["type"], "string");
        assert!(def.input_schema.get("required").is_none(), "device is optional");

        let stats = json!({
            "os": { "name": "Darwin", "arch": "arm64" },
            "memory": { "totalBytes": 34359738368u64, "availableBytes": null },
            "zeron": { "engineVersion": "0.2.139" }
        });
        let world = Arc::new(World { stats: Some(stats.clone()), ..Default::default() });
        let tools = tools(world.clone(), Origin::default());
        assert!(tools.has(DEVICE_STATS));

        // Local: no target, the engine answers for itself.
        let local = tools.call(DEVICE_STATS, json!({})).await.unwrap();
        assert_eq!(local["device"], json!({ "id": "dev-local", "name": "Laptop", "local": true }));
        assert_eq!(local["stats"], stats, "the report passes through as is, nulls kept");
        // By name: routed to that device.
        let remote = tools.call(DEVICE_STATS, json!({ "device": "worker" })).await.unwrap();
        assert_eq!(remote["device"], json!({ "id": "dev-remote", "name": "Worker", "local": false }));
        let reads = world.reads.lock().unwrap().clone();
        let targets: Vec<&Value> = reads
            .iter()
            .filter(|(m, _)| m == methods::DEVICE_STATS)
            .map(|(_, p)| &p["targetDeviceId"])
            .collect();
        assert_eq!(targets, [&Value::Null, &json!("dev-remote")]);

        // Not one of this account's devices: refused before anything is asked.
        let error = tools.call(DEVICE_STATS, json!({ "device": "someone-elses" })).await.unwrap_err();
        assert!(error.contains("no device matches"), "{error}");
        assert_eq!(world.reads.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn device_stats_says_when_a_device_is_away_or_this_engine_is_old() {
        let dark = Arc::new(World { stats: Some(json!({})), remote_dark: true, ..Default::default() });
        let error = tools(dark, Origin::default())
            .call(DEVICE_STATS, json!({ "device": "dev-remote" }))
            .await
            .unwrap_err();
        assert!(error.contains("offline"), "{error}");

        let old = Arc::new(World::default());
        let error = tools(old, Origin::default()).call(DEVICE_STATS, json!({})).await.unwrap_err();
        assert!(error.contains("too old to report stats"), "{error}");
    }

    #[test]
    fn the_show_visual_example_passes_its_own_check() {
        let start = SHOW_VISUAL_DESCRIPTION.find("local s = ui.state").unwrap();
        let end = start
            + SHOW_VISUAL_DESCRIPTION[start..]
                .find("\n\nZeron runs the script")
                .unwrap();
        let report = zeron_visual::check(&SHOW_VISUAL_DESCRIPTION[start..end], 1).unwrap();
        assert_eq!(report.interactions, 2, "both tabs");
    }

    #[tokio::test]
    async fn show_visual_is_offered_only_inside_a_chat() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_visual"));
        assert!(
            outside
                .call(
                    "show_visual",
                    json!({ "script": "return ui.text 'x'", "title": "x" })
                )
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
                ..Default::default()
            },
        );
        assert!(inside.has("show_visual"));
        // The guidance names every color the runtime accepts.
        for name in zeron_visual::TOKEN_NAMES {
            assert!(SHOW_VISUAL_DESCRIPTION.contains(name), "{name}");
        }
        let shown = inside
            .call(
                "show_visual",
                json!({ "script": "return ui.text 'chart'", "title": "Chart" }),
            )
            .await
            .unwrap();
        assert_eq!(shown["visual"]["title"], "Chart");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().unwrap();
        assert_eq!(method, methods::PUBLISH_VISUAL);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["script"], "return ui.text 'chart'");
    }

    #[tokio::test]
    async fn show_image_sends_the_file_only_from_inside_a_chat() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("before.png");
        std::fs::write(&file, b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let path = file.to_string_lossy().into_owned();
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_image"));
        assert!(!outside.has("wait_on_background"), "a wait is a chat's agent's to say");
        assert!(
            outside
                .call("show_image", json!({ "path": path }))
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
                ..Default::default()
            },
        );
        assert!(inside.has("show_image"));
        // (Offered only where the runtime wakes its agent: its own test.)
        assert!(!inside.has("wait_on_background"));
        let shown = inside
            .call("show_image", json!({ "path": path }))
            .await
            .unwrap();
        assert_eq!(shown["image"]["name"], "before.png");
        {
            let writes = world.writes.lock().unwrap();
            let (method, params) = writes.last().unwrap();
            assert_eq!(method, methods::PUBLISH_IMAGE);
            assert_eq!(params["chatId"], "chat-alpha-1");
            let data = base64::engine::general_purpose::STANDARD
                .decode(params["data"].as_str().unwrap())
                .unwrap();
            assert_eq!(data, b"\x89PNG\r\n\x1a\nfixture");
        }

        let missing = inside
            .call("show_image", json!({ "path": "/nonexistent/shot.png" }))
            .await
            .unwrap_err();
        assert!(
            missing.contains("can't read /nonexistent/shot.png"),
            "{missing}"
        );
        let huge = dir.path().join("huge.png");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(zeron_proto::IMAGE_MAX_BYTES as u64 + 1)
            .unwrap();
        let error = inside
            .call("show_image", json!({ "path": huge.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(error.contains("over 10 MB"), "{error}");
        assert_eq!(
            world.writes.lock().unwrap().len(),
            1,
            "nothing else was sent"
        );
    }

    #[tokio::test]
    async fn list_and_read_chats() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let listed = tools.call("list_chats", json!({})).await.unwrap();
        assert_eq!(listed["total"], 2);
        // Newest activity first: Beta was created later.
        assert_eq!(listed["chats"][0]["title"], "Beta");
        assert_eq!(listed["chats"][1]["project"]["name"], "comet");
        assert_eq!(listed["chats"][1]["status"], "idle");

        let read = tools
            .call("read_chat", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert_eq!(read["total"], 2);
        assert_eq!(read["messages"][1]["text"], "hello back");

        let read = tools
            .call("read_chat", json!({ "chat": "chat-alpha", "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(read["returned"], 1);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(read["messages"][0]["id"], "a1");
    }

    #[tokio::test]
    async fn send_attributes_and_refuses_self() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: Some("dev-local".into()),
                ..Default::default()
            },
        );
        let err = tools
            .call("send_message", json!({ "chat": "beta", "text": "loop" }))
            .await
            .unwrap_err();
        assert!(err.contains("own chat"), "{err}");

        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "please review" }),
            )
            .await
            .unwrap();
        assert_eq!(sent["sent"]["delivery"], "run");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().expect("a queued command");
        assert_eq!(method, methods::QUEUE_COMMAND);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["command"]["kind"], "run");
        let prompt = params["command"]["request"]["prompt"].as_str().unwrap();
        assert!(
            prompt.starts_with("[Message from Zeron chat Beta (chat-bet)"),
            "{prompt}"
        );
        assert!(prompt.ends_with("please review"));
        assert!(prompt.contains("tool, chat chat-beta-2.]"), "{prompt}");
        assert_eq!(params["command"]["request"]["cwd"], "/repo/comet");
        assert_eq!(params["command"]["request"]["harness"], "claude-code");
        assert_eq!(params["command"]["request"]["model"], "opus");
    }

    #[tokio::test]
    async fn replies_use_the_full_id_with_multiple_voice_orchestrators() {
        let first = "voice-orchestrator-11111111-1111-4111-8111-111111111111";
        let second = "voice-orchestrator-22222222-2222-4222-8222-222222222222";
        let world = Arc::new(World {
            extra_chats: [first, second]
                .into_iter()
                .map(|id| {
                    json!({
                        "id": id, "deviceId": "dev-local", "title": "Voice session",
                        "archived": false, "createdAt": "2026-10-01T00:00:00Z"
                    })
                })
                .collect(),
            ..Default::default()
        });
        let orchestrator = tools(
            world.clone(),
            Origin {
                chat_id: Some(second.into()),
                device_id: None,
                ..Default::default()
            },
        );
        let error = orchestrator
            .zeron
            .resolve_chat("voice-or")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2 chats start with"), "{error}");
        orchestrator
            .call(
                "send_message",
                json!({
                    "chat": "alpha", "text": "please review"
                }),
            )
            .await
            .unwrap();
        let reply_id = {
            let writes = world.writes.lock().unwrap();
            let prompt = writes.last().unwrap().1["command"]["request"]["prompt"]
                .as_str()
                .unwrap();
            prompt
                .split_once("tool, chat ")
                .unwrap()
                .1
                .split_once(".]\n\n")
                .unwrap()
                .0
                .to_owned()
        };
        assert_eq!(reply_id, second);
        let worker = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
                ..Default::default()
            },
        );
        worker
            .call(
                "send_message",
                json!({
                    "chat": reply_id, "text": "review complete"
                }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.last().unwrap().1["chatId"], second);
    }

    #[tokio::test]
    async fn auto_steers_busy_chats_even_at_turn_boundaries_or_after_long_quiet_tools() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let chats = tools.zeron.chats().await.unwrap();
        let session = Session {
            chat_id: chats[0].id.clone(),
            device_id: "dev-local".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            last_completed_turn: None,
            last_turn_outcome: None,
            terminal_activity: None,
            running_subagents: 0,
            background_tasks: Vec::new(),
            waiting: None,
        };
        // No mid-turn capability is required for a live mailbox delivery.
        let sent = tools
            .deliver(
                &chats[0],
                None,
                &[],
                Some(&session),
                "follow up".into(),
                "auto",
            )
            .await
            .unwrap();
        assert_eq!(sent.receipt["delivery"], "steer");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[0].1["command"]["kind"], "steer");
    }

    #[tokio::test]
    async fn create_chat_writes_the_row_and_validates_model() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not offered"), "{err}");

        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "sonnet", "title": "Review", "prompt": "go" }),
            )
            .await
            .unwrap();
        let chat_id = created["chatId"].as_str().unwrap().to_owned();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 3, "createChat, renameChat, run");
        assert_eq!(writes[0].1["op"], "createChat");
        assert_eq!(writes[0].1["chatId"], chat_id);
        assert_eq!(writes[0].1["spaceId"], "space-1");
        assert!(
            writes[0].1.get("parentChatId").is_none(),
            "no origin, no parent"
        );
        assert_eq!(writes[0].1["config"]["harness"], "claude-code");
        assert_eq!(writes[0].1["config"]["model"], "sonnet");
        assert_eq!(writes[1].1["op"], "renameChat");
        assert_eq!(writes[2].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[2].1["command"]["request"]["prompt"], "go");
    }

    #[tokio::test]
    async fn create_chat_records_the_origin_as_parent() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
                ..Default::default()
            },
        );
        let created = tools
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-beta-2");
        assert_eq!(
            world.writes.lock().unwrap()[0].1["parentChatId"],
            "chat-beta-2"
        );

        // An explicit parent (by title) overrides the origin.
        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "Alpha" }),
            )
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no chat matches"), "{err}");
    }

    #[tokio::test]
    async fn wait_on_an_idle_chat_returns_immediately() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let waited = tools
            .call(
                "wait_for_turn",
                json!({ "chat": "alpha", "timeout_secs": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "completed");
        assert_eq!(waited["turn"]["replies"][0]["text"], "hello back");
    }

    #[tokio::test]
    async fn send_with_wait_keeps_waiting_until_a_turn_lands() {
        // The stub never grows a session row, so a send-then-wait must run
        // to its deadline rather than declaring the (unstarted) run done.
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let started = std::time::Instant::now();
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "hi", "wait": true, "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(sent["turn"]["outcome"], "timedOut");
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn batches_dispatch_all_chats_before_waiting_and_keep_partial_results() {
        for (name, requests) in [
            (
                "create_chats",
                json!([
                    {"prompt": "first", "wait": true, "timeout_secs": 1},
                    {"harness": "not-a-harness"},
                    {"prompt": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
            (
                "send_messages",
                json!([
                    {"chat": "alpha", "text": "first", "wait": true, "timeout_secs": 1},
                    {"chat": "missing", "text": "invalid"},
                    {"chat": "beta", "text": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
        ] {
            let world = Arc::new(World {
                dispatch_barrier: Some(tokio::sync::Barrier::new(2)),
                ..Default::default()
            });
            let tools = tools(world.clone(), Origin::default());
            let reply = tokio::time::timeout(
                Duration::from_secs(3),
                crate::jsonrpc::handle_request(
                    &tools,
                    json!(42),
                    "tools/call",
                    json!({"name": name, "arguments": {"requests": requests}}),
                ),
            )
            .await
            .expect("both dispatches must proceed while other requests are waiting");
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let results = &reply["result"]["structuredContent"]["results"];
            for i in [0, 2] {
                assert_eq!(results[i]["index"], i);
                assert_eq!(results[i]["isError"], false, "{reply}");
                assert_eq!(results[i]["result"]["turn"]["outcome"], "timedOut");
            }
            assert_eq!(results[1]["index"], 1);
            assert_eq!(results[1]["isError"], true);
            let writes = world.writes.lock().unwrap();
            let dispatched: Vec<_> = writes
                .iter()
                .filter(|(method, _)| method == methods::QUEUE_COMMAND)
                .collect();
            assert_eq!(dispatched.len(), 2);
            assert_ne!(dispatched[0].1["chatId"], dispatched[1].1["chatId"]);
        }
    }

    #[tokio::test]
    async fn invalid_batch_sizes_have_no_side_effects() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for name in ["create_chats", "send_messages"] {
            for requests in [vec![], vec![json!({"prompt": "hello"}); MAX_BATCH + 1]] {
                assert!(
                    tools
                        .call(name, json!({"requests": requests}))
                        .await
                        .is_err()
                );
            }
        }
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wait_on_background_is_offered_only_where_the_runtime_wakes_its_agent() {
        let world = Arc::new(World::default());
        let offered = |chat: Option<&str>, waits: bool| {
            let tools = tools(
                world.clone(),
                Origin {
                    chat_id: chat.map(str::to_owned),
                    waits_on_background: waits,
                    ..Default::default()
                },
            );
            (tools.has("wait_on_background"), tools.waits_on_background())
        };
        // A chat on a runtime that brings its agent back (Claude Code, Muse).
        assert_eq!(offered(Some("chat-alpha-1"), true), (true, true));
        // A chat on one that doesn't (Codex): a turn that ends is the agent
        // done, so there's nothing to say.
        assert_eq!(offered(Some("chat-alpha-1"), false), (false, false));
        // No chat at all.
        assert_eq!(offered(None, true), (false, false));
        // Not offered, not callable either.
        let codex = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                ..Default::default()
            },
        );
        let call = crate::jsonrpc::handle_request(
            &codex,
            json!(1),
            "tools/call",
            json!({ "name": "wait_on_background", "arguments": { "task_ids": ["b1"] } }),
        )
        .await;
        assert!(call["error"]["message"].as_str().unwrap().contains("unknown tool"), "{call}");
    }

    fn origin(chat: &str) -> Origin {
        Origin {
            chat_id: Some(chat.into()),
            device_id: None,
            ..Default::default()
        }
    }

    fn tools_from(world: &Arc<World>, chat: &str) -> Tools {
        tools(world.clone(), origin(chat))
    }

    #[tokio::test]
    async fn a_chat_creates_chats_with_no_more_access_than_its_own() {
        let supervised = json!({
            "id": "chat-careful", "deviceId": "dev-local", "title": "Careful",
            "archived": false, "spaceId": "space-1",
            "config": { "harness": "claude-code", "sandbox": "workspace-write", "access": "supervised" },
            "createdAt": "2026-09-01T00:00:00Z"
        });
        let world = Arc::new(World {
            extra_chats: vec![supervised],
            ..Default::default()
        });
        let tools = tools_from(&world, "chat-careful");
        tools.call("create_chat", json!({})).await.unwrap();
        tools
            .call("create_chat", json!({"access": "supervised", "plan": true}))
            .await
            .unwrap();
        // Asking for more is lowered to its own, silently, and said in the
        // reply: no full-access child, no wider sandbox.
        let capped = tools
            .call("create_chat", json!({"access": "full-access", "sandbox": "danger-full-access"}))
            .await
            .unwrap();
        assert!(capped["accessCapped"].as_str().unwrap().contains("no more"), "{capped}");
        let writes = world.writes.lock().unwrap();
        let created: Vec<&Value> = writes
            .iter()
            .filter(|(_, w)| w["op"] == "createChat")
            .map(|(_, w)| w)
            .collect();
        assert_eq!(created.len(), 3);
        assert_eq!(created[0]["config"]["access"], "supervised", "inherited");
        assert_eq!(created[1]["config"]["plan"], true);
        assert_eq!(created[2]["config"]["access"], "supervised", "capped");
        assert_eq!(created[2]["config"]["sandbox"], "workspace-write", "capped");
        drop(writes);
        // A top-level chat with no mode of its own runs with full access,
        // which its chats inherit (the field is omitted on the wire).
        tools_from(&world, "chat-alpha-1")
            .call("create_chat", json!({}))
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        let last = writes
            .iter()
            .rev()
            .find(|(_, w)| w["op"] == "createChat")
            .unwrap();
        assert!(last.1["config"].get("access").is_none());
    }

    #[tokio::test]
    async fn delegation_is_one_level_deep() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            deep: true,
            ..Default::default()
        });
        // A top-level chat delegates.
        let created = tools(world.clone(), origin("chat-alpha-1"))
            .call("create_chat", json!({}))
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        // A chat that has a parent creates nothing: no side chat, no
        // standalone chat, no child of another chat.
        for (from, args) in [
            ("chat-beta-2", json!({})),
            ("chat-beta-2", json!({"kind":"chat"})),
            ("chat-beta-2", json!({"parent":"Alpha"})),
            ("chat-delta-4", json!({})),
        ] {
            let refused = tools(world.clone(), origin(from))
                .call("create_chat", args)
                .await
                .unwrap_err();
            assert!(
                refused.contains("can't create chats of its own"),
                "{refused}"
            );
        }

        // Naming a delegate as the parent is refused too, and the batch form
        // reports it per item.
        let root = tools(world.clone(), Origin::default());
        assert!(
            root.call("create_chat", json!({"parent":"Beta"}))
                .await
                .unwrap_err()
                .contains("itself a delegated chat")
        );
        let batch = root
            .call(
                "create_chats",
                json!({"requests":[{"parent":"Beta"}, {"parent":"Alpha"}]}),
            )
            .await
            .unwrap();
        let results = batch["results"].as_array().unwrap();
        assert_eq!(results[0]["isError"], true);
        assert_ne!(results[1]["isError"], true);
        assert!(
            world
                .writes
                .lock()
                .unwrap()
                .iter()
                .all(|(_, w)| w["parentChatId"] != "chat-beta-2")
        );
    }

    #[tokio::test]
    async fn a_cloud_child_runs_on_its_own_machine_with_clouds_agents() {
        let world = Arc::new(World::default());
        let alpha = tools(world.clone(), origin("chat-alpha-1"));
        let created = alpha
            .call("create_chat", json!({"cloud": true}))
            .await
            .unwrap();
        // The engine names the new machine and its checkout.
        assert_eq!(created["deviceId"], "cloud-child");
        assert_eq!(
            created["harness"], "claude-code",
            "Cloud's first agent by default"
        );
        let sent = world
            .writes
            .lock()
            .unwrap()
            .iter()
            .find(|(method, w)| method == methods::MUTATE && w["op"] == "createChat")
            .map(|(_, w)| w.clone())
            .unwrap();
        assert_eq!(sent["cloud"], true);
        assert_eq!(sent["parentChatId"], "chat-alpha-1");
        // Only Cloud's agents.
        let refused = alpha
            .call("create_chat", json!({"cloud": true, "harness": "cursor"}))
            .await
            .unwrap_err();
        assert!(refused.contains("Codex and Claude Code"), "{refused}");
    }

    #[tokio::test]
    async fn connected_means_installed_enabled_and_not_known_signed_out() {
        let accounts = |active: bool| {
            json!({ "accounts": [{
                "id": "a1", "harness": "claude-code", "active": active,
                "email": null, "planLabel": null
            }], "warnings": [] })
        };
        // No accounts endpoint: login state unknown, nothing held against it.
        let unknown = tools(Arc::new(World::default()), Origin::default());
        let listed = unknown.call("list_harnesses", json!({})).await.unwrap();
        let claude = &listed["harnesses"][0];
        assert_eq!(claude["signedIn"], Value::Null);
        assert_eq!(claude["connected"], true);
        // Codex is not installed here: never connected.
        assert_eq!(listed["harnesses"][1]["connected"], false);

        let signed_out = Arc::new(World {
            accounts: Some(accounts(false)),
            ..Default::default()
        });
        let tools_out = tools(signed_out.clone(), Origin::default());
        let listed = tools_out.call("list_harnesses", json!({})).await.unwrap();
        assert_eq!(listed["harnesses"][0]["signedIn"], false);
        assert_eq!(listed["harnesses"][0]["connected"], false);
        assert_eq!(listed["harnesses"][0]["available"], true);
        let created = tools_out
            .call("create_chat", json!({"harness": "claude-code"}))
            .await
            .unwrap();
        assert!(
            created["warning"]
                .as_str()
                .unwrap()
                .contains("not look signed in"),
            "{created}"
        );

        let signed_in = Arc::new(World {
            accounts: Some(accounts(true)),
            ..Default::default()
        });
        let tools_in = tools(signed_in, Origin::default());
        let listed = tools_in.call("list_harnesses", json!({})).await.unwrap();
        assert_eq!(listed["harnesses"][0]["signedIn"], true);
        assert_eq!(listed["harnesses"][0]["connected"], true);
        let created = tools_in.call("create_chat", json!({})).await.unwrap();
        assert!(created.get("warning").is_none());
    }

    #[tokio::test]
    async fn remote_login_state_and_nested_kinds_use_the_selected_host() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            deep: true,
            accounts: Some(json!({"accounts": [], "warnings": []})),
            remote_accounts: Some(json!({"accounts": [{
                "id": "remote-account", "harness": "codex", "active": true,
                "email": null, "planLabel": null
            }], "warnings": []})),
            ..Default::default()
        });
        let top = tools(world.clone(), origin("chat-alpha-1"));
        let listed = top
            .call("list_harnesses", json!({"device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(listed["harnesses"][0]["signedIn"], true);
        assert_eq!(listed["harnesses"][0]["connected"], true);
        assert_eq!(listed["harnesses"][1]["connected"], false);
        let created = top
            .call("create_chat", json!({"kind":"side", "device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(created["kind"], "side");
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        assert_eq!(created["deviceId"], "dev-remote");
        assert_eq!(created["harness"], "codex");
        assert!(created.get("warning").is_none(), "{created}");
        // Delegates create nothing, whatever the kind or host; the batch
        // form reports it per item.
        for from in ["chat-beta-2", "chat-delta-4"] {
            let batch = tools(world.clone(), origin(from))
                .call(
                    "create_chats",
                    json!({"requests":[
                        {"kind":"side", "device":"Worker"},
                        {"kind":"chat", "device":"Worker"},
                        {"parent":"Alpha", "device":"Worker"}
                    ]}),
                )
                .await
                .unwrap();
            for result in batch["results"].as_array().unwrap() {
                assert_eq!(result["isError"], true);
                assert!(
                    result["error"]
                        .as_str()
                        .unwrap()
                        .contains("can't create chats of its own"),
                    "{result}"
                );
            }
        }
        let reads = world.reads.lock().unwrap();
        assert!(
            reads
                .iter()
                .any(|(method, params)| method == methods::LIST_AGENT_ACCOUNTS
                    && params["targetDeviceId"] == "dev-remote")
        );
        assert!(
            reads
                .iter()
                .all(|(_, params)| params["targetDeviceId"] == "dev-remote")
        );
    }

    #[tokio::test]
    async fn kinds_default_children_to_subagents_and_reject_invalid_arguments_before_writing() {
        let world = Arc::new(World::default());
        let root = tools(world.clone(), Origin::default());
        for args in [
            json!({"kind":"side"}),
            json!({"kind":"subagent"}),
            json!({"kind":"unknown"}),
            json!({"kind":"chat", "parent":"Alpha", "title":"must not write"}),
        ] {
            assert!(root.call("create_chat", args).await.is_err());
        }
        assert!(world.writes.lock().unwrap().is_empty());
        let from_chat = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
                ..Default::default()
            },
        );
        for (args, kind, parent) in [
            (json!({"kind":"chat"}), "chat", Value::Null),
            (json!({"kind":"chat", "parent":"  "}), "chat", Value::Null),
            (json!({"kind":"side"}), "side", json!("chat-alpha-1")),
            (json!({"kind":"subagent"}), "subagent", json!("chat-alpha-1")),
            // A child of your own chat, kind omitted: a subagent.
            (json!({}), "subagent", json!("chat-alpha-1")),
            (json!({"parent":"Beta"}), "subagent", json!("chat-beta-2")),
            (json!({"kind":"side", "parent":"Beta"}), "side", json!("chat-beta-2")),
        ] {
            let created = from_chat.call("create_chat", args).await.unwrap();
            assert_eq!(created["kind"], kind);
            assert_eq!(created["parentChatId"], parent);
            let writes = world.writes.lock().unwrap();
            let row = &writes.last().unwrap().1;
            if kind == "chat" {
                assert!(row.get("parentChatId").is_none());
                assert!(row.get("childKind").is_none());
            } else {
                assert_eq!(row["parentChatId"], parent);
                // The row says what the child is to its parent.
                assert_eq!(row["childKind"], kind);
            }
        }
        assert_eq!(
            root.call("create_chat", json!({})).await.unwrap()["kind"],
            "chat"
        );
        assert_eq!(
            root.call("create_chat", json!({"kind":"side", "parent":"Alpha"}))
                .await
                .unwrap()["kind"],
            "side"
        );
        // An explicit parent from outside any chat: a subagent of it.
        assert_eq!(
            root.call("create_chat", json!({"parent":"Alpha"})).await.unwrap()["kind"],
            "subagent"
        );
    }

    #[tokio::test]
    async fn project_and_device_resolution_scopes_names_and_exact_paths() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for args in [
            json!({"project":"comet"}),
            json!({"project":"/repo/comet"}),
            json!({"project":"space-1", "device":"Worker"}),
            json!({"project":"missing", "device":"Worker"}),
            json!({"device":"missing"}),
        ] {
            let error = tools.call("create_chat", args).await.unwrap_err();
            assert!(world.writes.lock().unwrap().is_empty(), "{error}");
        }
        let error = tools
            .call("create_chat", json!({"project":"/repo/comet"}))
            .await
            .unwrap_err();
        for candidate in ["space-1", "space-remote", "dev-local", "dev-remote"] {
            assert!(error.contains(candidate), "{error}");
        }
        for project in ["comet", "/repo/comet", "space-remote"] {
            let created = tools
                .call("create_chat", json!({"project":project,"device":"Worker"}))
                .await
                .unwrap();
            assert_eq!(created["deviceId"], "dev-remote");
            assert_eq!(created["project"]["id"], "space-remote");
            assert_eq!(created["harness"], "codex");
        }
        let created = tools
            .call("create_chat", json!({"project":"unique"}))
            .await
            .unwrap();
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools
            .call("create_chat", json!({"device":"dev-remote"}))
            .await
            .unwrap();
        assert!(created["project"].is_null());
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools.call("create_chat", json!({})).await.unwrap();
        assert_eq!(created["deviceId"], "dev-local");
    }

    #[tokio::test]
    async fn discovery_and_creation_use_the_selected_hosts_catalogs() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        assert_eq!(
            tools.call("list_projects", json!({})).await.unwrap()["projects"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let projects = tools
            .call("list_projects", json!({"device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        assert!(
            projects["projects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["deviceId"] == "dev-remote")
        );
        assert_eq!(
            tools.call("list_harnesses", json!({})).await.unwrap()["harnesses"][0]["id"],
            "claude-code"
        );
        assert_eq!(
            tools
                .call("list_harnesses", json!({"device":"Worker"}))
                .await
                .unwrap()["harnesses"][0]["id"],
            "codex"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"codex", "device":"Worker"}))
                .await
                .unwrap()["models"][0]["id"],
            "remote-model"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"claude-code"}))
                .await
                .unwrap()["models"][0]["id"],
            "opus"
        );
        for args in [
            json!({"device":"Worker", "harness":"claude-code"}),
            json!({"device":"Worker", "harness":"cursor"}),
            json!({"device":"Laptop", "harness":"codex"}),
            json!({"device":"Laptop", "harness":"mock"}),
            json!({"device":"Worker", "harness":"codex", "model":"opus"}),
        ] {
            assert!(tools.call("create_chat", args).await.is_err());
            assert!(world.writes.lock().unwrap().is_empty());
        }
        let created = tools
            .call(
                "create_chat",
                json!({"device":"Worker", "harness":"codex", "model":"remote-model"}),
            )
            .await
            .unwrap();
        assert_eq!(created["harness"], "codex");
        assert_eq!(created["model"], "remote-model");
        for method in [methods::LIST_HARNESSES, methods::LIST_MODELS] {
            let broken = Arc::new(World {
                catalog_error: Some(method),
                ..Default::default()
            });
            let tools = self::tools(broken.clone(), Origin::default());
            let error = tools
                .call(
                    "create_chat",
                    json!({"device":"Worker", "model":"remote-model"}),
                )
                .await
                .unwrap_err();
            assert!(
                error.contains("dev-remote") && error.contains("catalog unavailable"),
                "{error}"
            );
            assert!(broken.writes.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn remote_send_selects_the_remote_default_harness() {
        let world = Arc::new(World {
            beta_remote: true,
            ..Default::default()
        });
        let tools = tools(world.clone(), Origin::default());
        tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .unwrap();
        assert_eq!(
            world.reads.lock().unwrap().last().unwrap().1["targetDeviceId"],
            "dev-remote"
        );
        assert_eq!(
            world.writes.lock().unwrap().last().unwrap().1["command"]["request"]["harness"],
            "codex"
        );
    }

    #[tokio::test]
    async fn mixed_batch_preserves_order_and_errors_per_request() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
                ..Default::default()
            },
        );
        let result = tools
            .call(
                "create_chats",
                json!({"requests":[
                    {"kind":"chat", "device":"Worker", "project":"comet"},
                    {"kind":"chat", "parent":"Alpha"},
                    {"kind":"side", "project":"space-1"},
                    {"kind":"bogus"}
                ]}),
            )
            .await
            .unwrap();
        let results = &result["results"];
        assert_eq!(results[0]["result"]["kind"], "chat");
        assert_eq!(results[0]["result"]["deviceId"], "dev-remote");
        assert_eq!(results[1]["isError"], true);
        assert_eq!(results[2]["result"]["kind"], "side");
        assert_eq!(results[2]["result"]["parentChatId"], "chat-alpha-1");
        assert_eq!(results[3]["isError"], true);
        for i in 0..4 {
            assert_eq!(results[i]["index"], i);
        }
        assert_eq!(world.writes.lock().unwrap().len(), 2);
    }

    /// Sessions and transcripts are independently delivered. The command reply
    /// is deliberately slow, so the assistant starts before QueueCommand returns.
    struct DelayedTurn {
        world: World,
        sent: Mutex<Option<(i64, Instant, String)>>,
        reply_delay: Duration,
        never_reply: bool,
        /// The id the send gave its message, as the host stores it.
        message_id: Mutex<Option<String>>,
        /// The transcript stream ends at once, like a link that drops.
        no_transcript: bool,
        /// Host clock offset: a remote engine stamps createdAt on its own clock.
        host_skew_millis: i64,
    }

    #[async_trait]
    impl RpcService for DelayedTurn {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_CHATS => {
                    let reply = self.world.handle(method, params).await?;
                    let RpcReply::Stream(mut rows) = reply else {
                        unreachable!()
                    };
                    let mut rows = rows.next().await.unwrap();
                    if let Some((_, _, chat)) = self.sent.lock().unwrap().as_ref()
                        && chat != "chat-beta-2"
                    {
                        rows.as_array_mut().unwrap().push(json!({
                            "id": chat, "deviceId":"dev-remote", "archived":false,
                            "createdAt":chrono::Utc::now(), "config":{"harness":"codex", "sandbox":"workspace-write"}
                        }));
                    }
                    Ok(stream(rows))
                }
                methods::QUEUE_COMMAND => {
                    *self.message_id.lock().unwrap() =
                        params["command"]["messageId"].as_str().map(str::to_owned);
                    let reply = self.world.handle(method, params.clone()).await?;
                    *self.sent.lock().unwrap() = Some((
                        now_millis(),
                        Instant::now(),
                        params["chatId"].as_str().unwrap().into(),
                    ));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(reply)
                }
                methods::WATCH_SESSIONS => {
                    let sent = self.sent.lock().unwrap().clone();
                    let Some((_, _, chat)) = sent else {
                        return Ok(stream(json!([])));
                    };
                    Ok(RpcReply::Stream(
                        futures::stream::once(async { json!([]) })
                            .chain(futures::stream::once(async move {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                json!([{"chatId":chat, "deviceId":"dev-remote", "status":"idle",
                                "updatedAt":chrono::Utc::now(), "lastCompletedTurn":"new-turn"}])
                            }))
                            .boxed(),
                    ))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let sent = self.sent.lock().unwrap().clone();
                    // Only the existing chat has history. Its previous reply
                    // shares the send's timestamp, so only its id tells them apart.
                    let mut messages = json!([]);
                    if params["chatId"] == "chat-beta-2" {
                        let created_at = sent.as_ref().map_or_else(now_millis, |(t, _, _)| *t);
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"previous-reply", "role":"assistant", "createdAt":created_at,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"old reply"}]
                        }));
                    }
                    if let Some(message_id) = self.message_id.lock().unwrap().clone() {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":message_id, "role":"user", "createdAt":0, "deviceId":"dev-local",
                            "parts":[{"kind":"text", "id":"t", "text":"go"}]
                        }));
                    }
                    if self.no_transcript {
                        return Ok(RpcReply::Stream(futures::stream::empty().boxed()));
                    }
                    if let Some((timestamp, started, _)) = sent
                        && started.elapsed() >= self.reply_delay
                        && !self.never_reply
                    {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"new-reply", "role":"assistant", "createdAt":timestamp + self.host_skew_millis,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"new reply"}]
                        }));
                    }
                    Ok(stream(json!({"reset":messages})))
                }
                _ => self.world.handle(method, params).await,
            }
        }
    }

    /// A sleeping host can't be read, and only a send wakes it: the send must go
    /// out through this engine's doc regardless, and reading the chat falls back
    /// to the local replica.
    #[tokio::test]
    async fn a_send_to_a_sleeping_host_still_goes_out() {
        let world = Arc::new(World {
            beta_remote: true,
            remote_dark: true,
            ..Default::default()
        });
        let tools = tools(world.clone(), Origin::default());
        let result = tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .unwrap();
        assert_eq!(result["sent"]["delivery"], "run");
        {
            let writes = world.writes.lock().unwrap();
            let (method, params) = writes.last().unwrap();
            assert_eq!(method, methods::QUEUE_COMMAND);
            assert!(params.get("targetDeviceId").is_none(), "a local doc write, which wakes the host");
        }
        let read = tools
            .call("read_chat", json!({"chat":"Beta"}))
            .await
            .unwrap();
        assert!(read["total"].as_u64().unwrap() > 0);
        let info = tools.call("get_chat", json!({"chat":"Beta"})).await.unwrap();
        assert!(info["messageCount"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn wait_allows_a_missing_session_and_a_late_remote_transcript() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                message_id: Mutex::new(None),
                no_transcript: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(
                result["turn"]["replies"].as_array().unwrap().len(),
                1,
                "{result}"
            );
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    #[tokio::test]
    async fn standalone_wait_after_nonblocking_send_keeps_the_send_baseline() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                message_id: Mutex::new(None),
                no_transcript: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let sent = if create {
                tools
                    .call(
                        "create_chat",
                        json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            } else {
                tools
                    .call(
                        "send_message",
                        json!({"chat":"Beta", "text":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            };
            let result = tools
                .call(
                    "wait_for_turn",
                    json!({"chat":sent["chatId"], "timeout_secs":2}),
                )
                .await
                .unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(result["turn"]["replies"].as_array().unwrap().len(), 1);
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    /// The bug this guards against: a transcript that can't be read (a long
    /// chat over a bad link) must neither stop a send nor make it look failed.
    #[tokio::test]
    async fn an_unreadable_transcript_never_makes_a_send_fail() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            reply_delay: Duration::ZERO,
            never_reply: false,
            message_id: Mutex::new(None),
            no_transcript: true,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Zeron::with_client(
            memory_client(service.clone()),
            Origin::default(),
        )));
        let sent = tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .expect("delivery reads nothing");
        assert_eq!(sent["sent"]["delivery"], "run");
        assert!(service.sent.lock().unwrap().is_some());

        let waited = tools
            .call(
                "send_message",
                json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":1}),
            )
            .await
            .expect("a wait that can't follow the turn doesn't undo the send");
        assert_eq!(waited["sent"]["delivery"], "run", "{waited}");
        assert!(
            waited["turn"]["transcriptError"]
                .as_str()
                .is_some_and(|e| e.contains("stream ended")),
            "{waited}"
        );
        assert_eq!(waited["turn"]["replies"], json!([]));

        // The chat's facts still come back, with why the rest didn't.
        let chat = tools
            .call("get_chat", json!({"chat":"Beta"}))
            .await
            .unwrap();
        assert!(chat["transcriptError"].is_string(), "{chat}");
        assert_eq!(chat["id"], "chat-beta-2");
    }

    /// A host from before windows ignores the param and sends everything: the
    /// reader takes the same slice, so an old host and a new one read alike.
    #[tokio::test]
    async fn an_older_host_sending_everything_reads_the_same_as_a_window() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let read = tools
            .call("read_chat", json!({"chat":"Beta", "limit":1}))
            .await
            .unwrap();
        assert_eq!(read["total"], 2);
        assert_eq!(read["returned"], 1);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(read["messages"][0]["id"], "a1");
    }

    /// A remote host stamps replies on its own clock. One running behind the
    /// caller must not hide the reply to this send.
    #[tokio::test]
    async fn wait_returns_the_reply_from_a_host_whose_clock_is_behind() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(100),
                never_reply: false,
                message_id: Mutex::new(None),
                no_transcript: false,
                host_skew_millis: -30_000,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            let replies = result["turn"]["replies"].as_array().unwrap();
            assert!(replies.iter().any(|r| r["text"] == "new reply"), "{result}");
        }
    }

    #[tokio::test]
    async fn completed_session_without_new_transcript_times_out_without_old_reply() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            message_id: Mutex::new(None),
            no_transcript: false,
            reply_delay: Duration::ZERO,
            never_reply: true,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Zeron::with_client(
            memory_client(service),
            Origin::default(),
        )));
        let result = tools
            .call(
                "send_message",
                json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":1}),
            )
            .await
            .unwrap();
        assert_eq!(result["turn"]["outcome"], "timedOut");
        assert_eq!(result["turn"]["replies"], json!([]));
    }

    #[tokio::test]
    async fn remote_replies_do_not_require_synchronized_clocks() {
        for host_skew_millis in [-1_000, 1_000] {
            for wait in [true, false] {
                let service = Arc::new(DelayedTurn {
                    world: World {
                        beta_remote: true,
                        ..Default::default()
                    },
                    sent: Mutex::new(None),
                    reply_delay: Duration::from_millis(450),
                    never_reply: false,
                    message_id: Mutex::new(None),
                    no_transcript: false,
                    host_skew_millis,
                });
                let tools = Tools::new(Arc::new(Zeron::with_client(
                    memory_client(service),
                    Origin::default(),
                )));
                let sent = tools
                    .call(
                        "send_message",
                        json!({
                            "chat":"Beta", "text":"go", "wait":wait, "timeout_secs":2
                        }),
                    )
                    .await
                    .unwrap();
                let result = if wait {
                    sent
                } else {
                    tools
                        .call(
                            "wait_for_turn",
                            json!({
                                "chat":sent["chatId"], "timeout_secs":2
                            }),
                        )
                        .await
                        .unwrap()
                };
                assert_eq!(
                    result["turn"]["outcome"], "completed",
                    "offset={host_skew_millis}, wait={wait}: {result}"
                );
                assert_eq!(
                    result["turn"]["replies"].as_array().unwrap().len(),
                    1,
                    "{result}"
                );
                assert_eq!(result["turn"]["replies"][0]["id"], "new-reply");
            }
        }
    }

    #[tokio::test]
    async fn initialize_and_list_over_jsonrpc() {
        let world = Arc::new(World::default());
        // A server whose agent launched without its own subagent tools
        // tells it to delegate through chats; any other doesn't.
        let rule = "with `create_chats` (or `create_chat`).";
        for side_chats_only in [true, false] {
            let origin = Origin {
                side_chats_only,
                ..Default::default()
            };
            let init = crate::jsonrpc::handle_request(
                &tools(world.clone(), origin),
                json!(1),
                "initialize",
                json!({}),
            )
            .await;
            let instructions = init["result"]["instructions"].as_str().unwrap();
            assert_eq!(instructions.ends_with(rule), side_chats_only);
        }
        let tools = tools(world, Origin::default());
        let init = crate::jsonrpc::handle_request(
            &tools,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "zeron");
        let list =
            crate::jsonrpc::handle_request(&tools, json!(2), "tools/list", Value::Null).await;
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        let bad = crate::jsonrpc::handle_request(
            &tools,
            json!(3),
            "tools/call",
            json!({ "name": "nope" }),
        )
        .await;
        assert_eq!(bad["error"]["code"], -32602);
        let whoami = crate::jsonrpc::handle_request(
            &tools,
            json!(4),
            "tools/call",
            json!({ "name": "whoami" }),
        )
        .await;
        assert_eq!(whoami["result"]["isError"], false);
        assert_eq!(
            whoami["result"]["structuredContent"]["localDeviceId"],
            "dev-local"
        );
    }
}
