//! End-to-end tests for the `repe` CLI binary.
//!
//! These spin up an in-process registry-backed `AsyncServer` on an ephemeral
//! port, then invoke the compiled `repe` binary via `Command` and assert on
//! its stdout, stderr, and exit code. The unit tests in `src/bin/repe.rs`
//! cover argv rewriting and URL parsing in isolation; this file covers the
//! actual wire path.

#![cfg(feature = "cli")]

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use repe::{AsyncServer, ErrorCode, Registry, Router, TypedResponse, WebSocketServer};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Path to the freshly-built `repe` binary. Cargo populates this for
/// `[[bin]]` targets when the integration test compiles.
const REPE_BIN: &str = env!("CARGO_BIN_EXE_repe");

/// Build the registry-backed router used by both the TCP and WebSocket test
/// servers. Centralizing this keeps the two transports exercising the same
/// surface area; any added endpoint becomes testable on both immediately.
fn build_router() -> Router {
    let registry = Arc::new(Registry::new());
    registry.register_value("/counter", json!(0)).unwrap();
    registry
        .register_value("/config", json!({"timeout": 30, "retries": 3}))
        .unwrap();
    registry
        .register_function("/add", |params| {
            let Some(Value::Object(map)) = params else {
                return Err((ErrorCode::InvalidBody, "expected object body".into()));
            };
            let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
            let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
            Ok(json!({"result": a + b}))
        })
        .unwrap();
    registry
        .register_function("/refresh", |_params| Ok(Value::Null))
        .unwrap();
    // Sleeps long enough that any reasonable `--timeout` setting will trip.
    // Uses std::thread::sleep because the registry's callable signature is
    // synchronous; with `worker_threads = 2`, the second worker keeps the
    // server's accept loop and CLI subprocess responsive while the handler
    // blocks.
    registry
        .register_function("/slow_call", |_params| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(Value::Null)
        })
        .unwrap();

    // `/beve_echo` exists specifically to exercise the CLI's BEVE response
    // decoding: it accepts a JSON body and replies with the same payload
    // re-encoded as BEVE.
    Router::new()
        // Turbofish on `with_typed` is required: `TypedResponse<Value>` matches
        // both blanket `IntoTypedResponse` impls, so `R` cannot be inferred.
        .with_typed::<Value, Value, _>(
            "/beve_echo",
            |params: Value| -> Result<TypedResponse<Value>, (ErrorCode, String)> {
                Ok(TypedResponse::beve(json!({
                    "format": "beve",
                    "echo": params,
                })))
            },
        )
        .with_registry("/api/v1", registry)
}

/// Bind a TCP `AsyncServer` to an ephemeral port and return its `host:port`.
async fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = AsyncServer::new(build_router());
    tokio::spawn(async move {
        let _ = server.serve(listener).await;
    });
    format!("{}:{}", addr.ip(), addr.port())
}

/// Bind a WebSocket `WebSocketServer` to an ephemeral port and return its
/// `ws://host:port/repe` URL.
async fn spawn_ws_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = WebSocketServer::new(build_router());
    tokio::spawn(async move {
        let _ = server.serve_listener(listener, "/repe").await;
    });
    format!("ws://{}:{}/repe", addr.ip(), addr.port())
}

/// Run the binary with the given args (server URL is prepended automatically)
/// and return the captured output. The blocking `Command::output` call is
/// dispatched to a tokio blocking thread so the server task running on the
/// same runtime stays responsive.
async fn run_cli(url: &str, args: &[&str]) -> Output {
    let url = url.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(REPE_BIN);
        cmd.arg("--url").arg(&url).args(&args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.output().expect("spawn repe binary")
    })
    .await
    .expect("blocking task panicked")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_returns_registered_value() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["get", "/api/v1/counter"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_then_inferred_get_roundtrip() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["set", "/api/v1/counter", "42"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(
        stdout_of(&out).is_empty(),
        "set should suppress response body"
    );

    // Inferred-mode get: bare path with no body.
    let out = run_cli(&url, &["/api/v1/counter"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_with_json_body_returns_object() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["call", "/api/v1/add", r#"{"a":17,"b":25}"#]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let value: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(value["result"], 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_flag_emits_compact_json() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["--raw", "get", "/api/v1/config"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    let trimmed = stdout.trim();
    assert!(
        !trimmed.contains('\n'),
        "expected single-line raw output, got: {stdout:?}"
    );
    let value: Value = serde_json::from_str(trimmed).unwrap();
    assert_eq!(value["timeout"], 30);
    assert_eq!(value["retries"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_emits_no_output() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["notify", "/api/v1/refresh", "{}"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_client_side_json_exits_one() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["set", "/api/v1/counter", "not-json"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("invalid JSON body"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_method_exits_two() {
    let url = spawn_server().await;

    // Inferred get against a path the registry does not know about.
    let out = run_cli(&url, &["/api/v1/does_not_exist"]).await;
    assert_eq!(out.status.code(), Some(2));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("server error") || stderr.contains("Method not found"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_from_stdin_dash() {
    let url = spawn_server().await;

    let out = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(REPE_BIN)
            .args(["--url", &url, "call", "/api/v1/add", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn repe");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(br#"{"a":3,"b":4}"#)
            .unwrap();
        // Closing stdin signals EOF so the CLI's read_to_string returns.
        drop(child.stdin.take());
        child.wait_with_output().expect("collect output")
    })
    .await
    .expect("blocking task panicked");

    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let value: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(value["result"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_from_file() {
    let url = spawn_server().await;

    let path = std::env::temp_dir().join(format!(
        "repe-cli-body-{}-{}.json",
        std::process::id(),
        rand_token()
    ));
    std::fs::write(&path, br#"{"a":11,"b":31}"#).unwrap();

    let out = run_cli(
        &url,
        &["--body-file", path.to_str().unwrap(), "call", "/api/v1/add"],
    )
    .await;
    let _ = std::fs::remove_file(&path);

    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let value: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(value["result"], 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn beve_response_is_decoded_to_json() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["call", "/beve_echo", r#"{"n":7}"#]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let value: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(value["format"], "beve");
    assert_eq!(value["echo"]["n"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_file_and_positional_conflict() {
    let url = spawn_server().await;

    let out = run_cli(
        &url,
        &[
            "--body-file",
            "/dev/null",
            "call",
            "/api/v1/add",
            r#"{"a":1,"b":2}"#,
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("cannot combine"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_get_set_call_roundtrip() {
    // Exercises the `Transport::WebSocket` arms of send() against a live
    // WebSocketServer; previously every integration test used TCP.
    let url = spawn_ws_server().await;

    let out = run_cli(&url, &["get", "/api/v1/counter"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "0");

    let out = run_cli(&url, &["set", "/api/v1/counter", "99"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).is_empty());

    // Inferred mode against a WebSocket URL exercises the rewriter + WS path.
    let out = run_cli(&url, &["/api/v1/counter"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "99");

    let out = run_cli(&url, &["call", "/api/v1/add", r#"{"a":40,"b":2}"#]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let value: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(value["result"], 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_flag_aborts_slow_call() {
    // The /slow_call handler sleeps 500 ms; --timeout 0.1 should fire well
    // before that, surfacing the timeout as a connection-class error (exit 1).
    let url = spawn_server().await;

    let out = run_cli(
        &url,
        &["--timeout", "0.1", "call", "/api/v1/slow_call", "{}"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let stderr = stderr_of(&out).to_lowercase();
    assert!(
        stderr.contains("timed out") || stderr.contains("timeout"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_with_body_file_is_rejected() {
    let url = spawn_server().await;

    let out = run_cli(
        &url,
        &["--body-file", "/dev/null", "get", "/api/v1/counter"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("not valid with `get`"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_without_body_is_rejected() {
    let url = spawn_server().await;

    let out = run_cli(&url, &["set", "/api/v1/counter"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("`set` requires a body"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_file_pointing_to_missing_path_exits_one() {
    let url = spawn_server().await;
    let missing = format!(
        "/tmp/repe-cli-definitely-missing-{}-{}.json",
        std::process::id(),
        rand_token()
    );

    let out = run_cli(&url, &["--body-file", &missing, "call", "/api/v1/add"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("could not read --body-file"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_stdin_body_fails_validation() {
    // The CLI dials the server before reading stdin, so this test needs a
    // live server even though the failure path is purely client-side.
    let url = spawn_server().await;

    let out = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(REPE_BIN)
            .args(["--url", &url, "call", "/api/v1/add", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn repe");
        // Closing stdin immediately gives the CLI an empty body.
        drop(child.stdin.take());
        child.wait_with_output().expect("collect output")
    })
    .await
    .expect("blocking task panicked");

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr_of(&out).contains("invalid JSON body"),
        "stderr: {}",
        stderr_of(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_failure_exits_one() {
    // Bind a TcpListener just long enough to learn the OS-assigned port,
    // then drop it. The kernel may briefly TIME_WAIT-protect the slot, but
    // a fresh connect will still fail with "connection refused" because
    // nothing is accepting.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let out = run_cli(&format!("127.0.0.1:{port}"), &["get", "/api/v1/counter"]).await;
    assert_eq!(out.status.code(), Some(1));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("tcp connect to") && stderr.contains("failed"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_writes_zsh_script_without_server() {
    // Completions must not require a running server or even a reachable URL.
    let out = tokio::task::spawn_blocking(|| {
        Command::new(REPE_BIN)
            .args(["completions", "zsh"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn repe")
    })
    .await
    .expect("blocking task panicked");

    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(
        stdout.starts_with("#compdef repe"),
        "expected zsh completion header, got: {stdout:.80}"
    );
    assert!(
        stdout.contains("_repe"),
        "expected the zsh function name in the script"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repe_url_env_var_is_honored() {
    let url = spawn_server().await;

    // Run without `--url`; REPE_URL should fill it in.
    let out = tokio::task::spawn_blocking(move || {
        Command::new(REPE_BIN)
            .env("REPE_URL", &url)
            .args(["get", "/api/v1/counter"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn repe")
    })
    .await
    .expect("blocking task panicked");

    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_url_overrides_repe_url_env() {
    let url = spawn_server().await;

    let out = tokio::task::spawn_blocking(move || {
        Command::new(REPE_BIN)
            // Bogus env var: if the explicit flag didn't win, we'd see a
            // connection failure here.
            .env("REPE_URL", "127.0.0.1:1")
            .args(["--url", &url, "get", "/api/v1/counter"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn repe")
    })
    .await
    .expect("blocking task panicked");

    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert_eq!(stdout_of(&out).trim(), "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_accepts_timeout_flag() {
    // `--timeout` bounds how long the client waits for the kernel to accept
    // the notify bytes. Under healthy conditions the send completes in
    // microseconds, so we can't reliably trip the timeout - this test just
    // pins down that the flag is accepted with notify (it used to be silently
    // ignored: --timeout was a global flag with no plumbing through to
    // Transport::notify).
    let url = spawn_server().await;

    let out = run_cli(&url, &["--timeout", "5", "notify", "/api/v1/refresh", "{}"]).await;
    assert!(out.status.success(), "stderr: {}", stderr_of(&out));
    assert!(stdout_of(&out).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dash_dash_disables_inferred_mode() {
    // `--` is the user opting out of the inferred-mode rewrite. clap then sees
    // `/api/v1/counter` as a literal positional and rejects the invocation;
    // exit code 2 is clap's default for argv-parse failures.
    let url = spawn_server().await;

    let out = run_cli(&url, &["--", "/api/v1/counter"]).await;
    assert!(!out.status.success(), "stderr: {}", stderr_of(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_timeout_is_rejected_as_usage_error() {
    let url = spawn_server().await;

    // Use `--timeout=-1` so clap parses `-1` as the value rather than a flag;
    // our `parse_timeout` then rejects it with a Usage error (exit 1).
    let out = run_cli(&url, &["--timeout=-1", "get", "/api/v1/counter"]).await;
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("--timeout") && stderr.contains("non-negative"),
        "stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_with_body_file_is_rejected_without_connecting() {
    // Pre-flight validation should fire before the connect attempt, so a
    // misconfigured `get --body-file` against an unreachable server still
    // surfaces the usage error rather than a connect failure.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let out = run_cli(
        &format!("127.0.0.1:{port}"),
        &["--body-file", "/dev/null", "get", "/api/v1/counter"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("not valid with `get`"),
        "expected usage error, got: {stderr}"
    );
    assert!(
        !stderr.contains("tcp connect"),
        "should not have attempted connect, got: {stderr}"
    );
}

/// Tiny non-cryptographic disambiguator so concurrent tests on the same PID
/// pick distinct temp paths without pulling in a UUID dependency.
fn rand_token() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
