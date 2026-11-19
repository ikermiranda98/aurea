//! Windows-native ACP transport coverage. No npm download or credentials;
//! `.cmd`/`.bat` shims launch through a wrapped cmd.exe with literal arguments.
#![cfg(all(windows, feature = "native-fixture"))]

use futures::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel};

#[test]
fn managed_process_protocol_progresses_with_one_blocking_worker() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use zeron_harness::process::Stdio;
        let mut command =
            zeron_harness::process::Command::new(env!("CARGO_BIN_EXE_harness-native-fixture"));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
                .await
                .unwrap();
            lines.next_line().await.unwrap().unwrap()
        })
        .await;
        // Release even when the progress assertion fails.
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        let reply: Value = serde_json::from_str(&reply.expect("protocol I/O starved")).unwrap();
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], 1);
    });
}

#[tokio::test]
async fn output_captures_both_streams_with_default_or_null_stdio() {
    for null_streams in [false, true] {
        let mut command =
            zeron_harness::process::Command::new(env!("CARGO_BIN_EXE_harness-native-fixture"));
        command.arg("--capture-output");
        if null_streams {
            command
                .stdout(zeron_harness::process::Stdio::null())
                .stderr(zeron_harness::process::Stdio::null());
        }
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"captured stdout");
        assert_eq!(output.stderr, b"captured stderr");
    }
}

struct ProcessHandle(*mut std::ffi::c_void);

#[tokio::test]
async fn native_launch_matches_tokio_argv_environment_cwd_and_path() {
    use zeron_harness::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let exe = fixture(dir.path());
    let arguments = [
        "",
        "plain",
        "a b",
        "tab\there",
        "quote\"here",
        "\\",
        "trailing \\",
        "slashes\\\\\"quote",
        "line\nbreak",
        "日本語 😀",
        "%PATH% & | > < ^ ! $()",
    ];
    for clear in [false, true] {
        // Search the explicit child PATH with the .exe suffix omitted.
        let name = exe.file_stem().unwrap();
        let mut native = Command::new(name);
        let mut baseline = tokio::process::Command::new(name);
        if clear {
            native.env_clear();
            baseline.env_clear();
        }
        native
            .arg("--launch-report")
            .args(arguments)
            .current_dir(dir.path())
            .env("PATH", dir.path())
            .env("zeron_launch_marker", "old")
            .env("ZERON_LAUNCH_MARKER", "new 日本語")
            .env("ZERON_LAUNCH_REMOVED", "old")
            .env_remove("zeron_launch_removed")
            .env("ZERON_ä_KEY", "unicode value")
            .stdin(Stdio::null());
        baseline
            .arg("--launch-report")
            .args(arguments)
            .current_dir(dir.path())
            .env("PATH", dir.path())
            .env("zeron_launch_marker", "old")
            .env("ZERON_LAUNCH_MARKER", "new 日本語")
            .env("ZERON_LAUNCH_REMOVED", "old")
            .env_remove("zeron_launch_removed")
            .env("ZERON_ä_KEY", "unicode value")
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .kill_on_drop(true);
        let expected = baseline.output().await.unwrap();
        let actual = tokio::time::timeout(Duration::from_secs(5), native.output())
            .await
            .unwrap()
            .unwrap();
        assert!(expected.status.success() && actual.status.success());
        let expected: Value = serde_json::from_slice(&expected.stdout).unwrap();
        let actual: Value = serde_json::from_slice(&actual.stdout).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual["argv"], serde_json::json!(arguments));
        assert_eq!(actual["marker"], "new 日本語");
        assert_eq!(actual["removed"], Value::Null);
        assert_eq!(actual["unicode"], "unicode value");
    }
}

#[tokio::test]
async fn invalid_launch_inputs_fail_without_starting_a_child() {
    use zeron_harness::process::Command;
    let exe = env!("CARGO_BIN_EXE_harness-native-fixture");
    for command in [
        Command::new(exe).arg("NUL\0argument"),
        Command::new(exe).env("BAD=KEY", "value"),
        Command::new(exe).env("KEY", "NUL\0value"),
        Command::new(exe).current_dir("NUL\0cwd"),
    ] {
        assert_eq!(
            command.spawn().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let mut missing = Command::new(dir.path().join("missing.exe"));
    assert_eq!(
        missing.spawn().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut bad_cwd = Command::new(exe);
    bad_cwd.current_dir(dir.path().join("missing"));
    assert!(bad_cwd.spawn().is_err());
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}
impl ProcessHandle {
    fn open(pid: u32) -> Self {
        let handle = unsafe { OpenProcess(0x00100000, 0, pid) }; // SYNCHRONIZE
        assert!(
            !handle.is_null(),
            "open live fixture process: {}",
            std::io::Error::last_os_error()
        );
        Self(handle)
    }
    async fn assert_exited(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while unsafe { WaitForSingleObject(self.0, 0) } != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fixture process was not reaped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn fixture(dir: &Path) -> PathBuf {
    let bin = dir.join("Native O'Brien 日本語 & space.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_harness-native-fixture"), &bin).unwrap();
    bin
}
fn request(cwd: &Path, prompt: &str, resume: Option<&str>) -> RunRequest {
    RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        access: Default::default(),
        plan: false,
        attachments: Vec::new(),
        worktree: None,
        resume: resume.map(str::to_owned),
        env: Default::default(),
        extra_mcp: Vec::new(),
    }
}
#[tokio::test]
async fn process_exit_drains_buffered_output_despite_inherited_descendant_pipes() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = zeron_harness::process::Command::new(fixture(dir.path()));
    command
        .arg("--output-tree")
        .current_dir(dir.path())
        .stdin(zeron_harness::process::Stdio::null())
        .stdout(zeron_harness::process::Stdio::piped())
        .stderr(zeron_harness::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Like the protocol drivers, wait for pipe EOF before calling child.wait().
    // Root-exit monitoring must terminate descendants independently of wait().
    tokio::time::timeout(Duration::from_secs(5), async {
        use tokio::io::AsyncReadExt;
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))
    })
    .await
    .expect("descendant pipes must close on root exit")
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        out,
        format!("{}FINAL-STDOUT", "o".repeat(128 * 1024)).as_bytes()
    );
    assert_eq!(
        err,
        format!("{}FINAL-STDERR", "e".repeat(128 * 1024)).as_bytes()
    );
}

#[tokio::test]
async fn application_exit_without_destructors_kills_owned_processes() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    // The owner is deliberately outside our job wrapper. It creates its own
    // managed job, then exits without running Drop after the test opens handles.
    let mut owner = tokio::process::Command::new(fixture(dir.path()))
        .arg("--job-owner")
        .current_dir(dir.path())
        .creation_flags(0x08000000)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(owner.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("owner startup deadline")
        .unwrap()
        .unwrap();
    let pids: Vec<u32> = serde_json::from_str(&line).unwrap();
    assert_eq!(pids.len(), 2);
    let handles: Vec<_> = pids.into_iter().map(ProcessHandle::open).collect();
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(b"exit\n")
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), owner.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    for handle in handles {
        handle.assert_exited().await;
    }
}

#[tokio::test]
async fn batch_arguments_resist_shell_interpretation() {
    use zeron_harness::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("shim 日本語 Ħ &!.cmd");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\n\"{}\" --launch-report %*\r\n",
            env!("CARGO_BIN_EXE_harness-native-fixture")
        ),
    )
    .unwrap();
    let arguments = [
        "",
        "plain",
        "a b",
        "日本語 😀",
        "quote\"here",
        "trailing \\",
        "slashes\\\\\"quote",
        "%ZERON_BATCH_ATTACK%",
        "!ZERON_BATCH_ATTACK!",
        "a\"&echo injected>injected.txt&rem \"b",
        "& | < > ^ ( )",
        "{\"model\":\"a&b\"}",
    ];
    for arg in arguments {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&script)
                .arg(arg)
                .current_dir(dir.path())
                .env("ZERON_BATCH_ATTACK", "EXPANDED&echo injected>injected.txt")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !dir.path().join("injected.txt").exists(),
            "command injection: {arg:?}"
        );
        assert!(
            output.status.success(),
            "{arg:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["argv"],
            serde_json::json!([arg]),
            "argument changed: {arg:?}"
        );
    }
    for arg in [
        "line\nbreak",
        "line\rbreak",
        "\"\n&echo injected>injected.txt",
    ] {
        assert_eq!(
            Command::new(&script).arg(arg).spawn().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!dir.path().join("injected.txt").exists());
    }
}

#[tokio::test]
async fn batch_executable_path_rejects_percent_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("shim%ZERON_BATCH_NAME%.cmd");
    std::fs::write(&script, "@exit /b 0\r\n").unwrap();
    assert_eq!(
        zeron_harness::process::Command::new(&script)
            .spawn()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}
