//! `repe` — command-line client for REPE RPC servers.
//!
//! Auto-detects transport from the URL: `ws://` / `wss://` use the WebSocket
//! transport, anything else (including bare `host:port`) uses TCP. The wire
//! protocol has no operation type, so `get` / `set` / `call` all dispatch the
//! same request; the only difference is whether a body is sent and whether
//! the response is printed.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use repe::{AsyncClient, BodyFormat, Message, QueryFormat, RepeError, WebSocketClient};
use serde_json::Value;

const DEFAULT_PORT: u16 = 5099;
const DEFAULT_HOST: &str = "localhost";

/// Global flags that take a value (`--foo VALUE`). The argv rewriter must skip
/// over both the flag and its value when scanning for the first positional, or
/// it will treat the value as the inferred path. Kept in sync with the global
/// `ArgAction::Set` flags on `Cli` by the `value_flag_list_matches_clap_globals`
/// unit test below.
const VALUE_FLAGS: &[&str] = &["--url", "--timeout", "--body-file"];

#[derive(Parser, Debug)]
#[command(
    name = "repe",
    version,
    about = "Command-line client for REPE RPC servers",
    long_about = None,
    after_help = "Examples:\n  \
        repe /status                                  # inferred get\n  \
        repe /counter 42                              # inferred set\n  \
        repe get /config\n  \
        repe set /counter 0\n  \
        repe call /add '{\"a\":1,\"b\":2}'\n  \
        repe call /add - < payload.json               # body from stdin\n  \
        repe set /config --body-file config.json      # body from file\n  \
        repe notify /events/refresh '{}'\n  \
        repe --url 127.0.0.1:9000 get /status\n  \
        repe --url ws://localhost:8080 call /echo '\"hello\"'\n\n\
        Exit codes:\n  \
        0  success\n  \
        1  connection or usage error\n  \
        2  RPC error returned by server\n"
)]
struct Cli {
    /// Server URL. Accepts `host:port`, `tcp://host:port`, `ws://host:port[/path]`,
    /// or `wss://host:port[/path]`. If only a host is given, port 5099 is used.
    /// Defaults from the `REPE_URL` environment variable, then `localhost:5099`.
    #[arg(
        long,
        global = true,
        env = "REPE_URL",
        default_value = "localhost:5099"
    )]
    url: String,

    /// Print response bodies as compact JSON (default is pretty-printed).
    #[arg(long, global = true)]
    raw: bool,

    /// Per-call timeout in seconds. Disabled when omitted.
    #[arg(long, global = true)]
    timeout: Option<f64>,

    /// Read the request body from a file. Mutually exclusive with a positional body.
    #[arg(long, global = true, value_name = "PATH")]
    body_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read a value: send an empty body, print the response.
    Get(MethodOnly),
    /// Write a value: send a JSON body, suppress the response.
    Set(MethodWithBody),
    /// Generic RPC call with optional JSON body; prints the response.
    Call(MethodMaybeBody),
    /// Fire-and-forget: send a notify with optional JSON body. No response.
    Notify(MethodMaybeBody),
    /// Print a shell completion script to stdout. Pipe it into the right place
    /// for your shell, e.g. `repe completions zsh > _repe`.
    Completions(CompletionsArgs),
}

#[derive(Args, Debug)]
struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    shell: Shell,
}

#[derive(Args, Debug)]
struct MethodOnly {
    /// JSON-pointer method path (e.g. `/status`).
    method: String,
}

#[derive(Args, Debug)]
struct MethodWithBody {
    /// JSON-pointer method path.
    method: String,
    /// JSON body. Pass `-` to read from stdin. Validated client-side as syntactically
    /// valid JSON.
    body: Option<String>,
}

#[derive(Args, Debug)]
struct MethodMaybeBody {
    /// JSON-pointer method path.
    method: String,
    /// Optional JSON body. Pass `-` to read from stdin. Validated client-side as
    /// syntactically valid JSON.
    body: Option<String>,
}

/// Pre-process argv to support the inferred-mode shortcut:
/// `repe /path` -> `repe get /path` and `repe /path '<json>'` -> `repe set /path '<json>'`.
/// We rewrite only when the first non-flag positional starts with `/`, so existing
/// subcommand invocations are untouched. A literal `--` is the user explicitly
/// opting out of inferred mode: bail without inserting anything.
fn rewrite_inferred(mut argv: Vec<String>) -> Vec<String> {
    let mut idx = 1;
    while idx < argv.len() {
        let arg = &argv[idx];
        if arg == "--" {
            return argv;
        }
        if !arg.starts_with('-') {
            break;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            idx += 2;
        } else {
            idx += 1;
        }
    }
    if idx < argv.len() && argv[idx].starts_with('/') {
        let inferred = if idx + 1 < argv.len() && !argv[idx + 1].starts_with('-') {
            "set"
        } else {
            "get"
        };
        argv.insert(idx, inferred.to_string());
    }
    argv
}

fn main() -> ExitCode {
    let argv = rewrite_inferred(std::env::args().collect());
    let cli = Cli::parse_from(argv);

    // Completions don't need a runtime, network, or the global flags. Handle
    // them before we spin up tokio so users don't pay startup cost for shell
    // shellouts and so the subcommand can be invoked even with no server up.
    if let Command::Completions(CompletionsArgs { shell }) = &cli.command {
        print_completions(*shell);
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("failed to start runtime: {err}");
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
        Err(CliError::Connection(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
        Err(CliError::Rpc(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    Connection(String),
    Rpc(String),
}

enum Transport {
    Tcp(AsyncClient),
    WebSocket(WebSocketClient),
}

impl Transport {
    /// Send a request and return the raw response message. The body bytes (if any)
    /// are sent verbatim with the supplied `body_format`; the caller is responsible
    /// for ensuring they parse correctly.
    async fn send(
        &self,
        method: &str,
        body: Option<&[u8]>,
        body_format: u16,
        timeout: Option<Duration>,
    ) -> Result<Message, RepeError> {
        let qf = QueryFormat::JsonPointer as u16;
        match self {
            Transport::Tcp(c) => match timeout {
                Some(d) => {
                    c.call_with_formats_and_timeout(method, qf, body, body_format, d)
                        .await
                }
                None => c.call_with_formats(method, qf, body, body_format).await,
            },
            Transport::WebSocket(c) => match timeout {
                Some(d) => {
                    c.call_with_formats_and_timeout(method, qf, body, body_format, d)
                        .await
                }
                None => c.call_with_formats(method, qf, body, body_format).await,
            },
        }
    }

    async fn notify(
        &self,
        method: &str,
        body: Option<&[u8]>,
        body_format: u16,
        timeout: Option<Duration>,
    ) -> Result<(), RepeError> {
        let qf = QueryFormat::JsonPointer as u16;
        let send = async {
            match self {
                Transport::Tcp(c) => c.notify_with_formats(method, qf, body, body_format).await,
                Transport::WebSocket(c) => {
                    c.notify_with_formats(method, qf, body, body_format).await
                }
            }
        };
        match timeout {
            Some(d) => match tokio::time::timeout(d, send).await {
                Ok(result) => result,
                Err(_) => Err(RepeError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("notify timed out after {}ms", d.as_millis()),
                ))),
            },
            None => send.await,
        }
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let timeout = parse_timeout(cli.timeout)?;

    // Pre-flight: any flag-combination check that doesn't need a live server
    // runs before connect(), so a misconfigured invocation against a down
    // server still surfaces the usage error instead of the connect failure.
    if let Command::Get(_) = &cli.command {
        if cli.body_file.is_some() {
            return Err(CliError::Usage(
                "--body-file is not valid with `get`".into(),
            ));
        }
    }

    let transport = connect(&cli.url).await?;

    match cli.command {
        Command::Get(MethodOnly { method }) => {
            let response = transport
                .send(&method, None, BodyFormat::RawBinary as u16, timeout)
                .await
                .map_err(map_call_error)?;
            print_response(&response, cli.raw)?;
        }
        Command::Set(MethodWithBody { method, body }) => {
            let body_text = resolve_body(body.as_deref(), cli.body_file.as_deref())?
                .ok_or_else(|| CliError::Usage("`set` requires a body".into()))?;
            // `set` suppresses the response body on success: a successful write
            // doesn't have anything interesting to print, and silence makes the
            // command pleasant to use in scripts.
            let _ = transport
                .send(
                    &method,
                    Some(body_text.as_bytes()),
                    BodyFormat::Json as u16,
                    timeout,
                )
                .await
                .map_err(map_call_error)?;
        }
        Command::Call(MethodMaybeBody { method, body }) => {
            let body_text = resolve_body(body.as_deref(), cli.body_file.as_deref())?;
            let response = match body_text.as_deref() {
                Some(text) => {
                    transport
                        .send(
                            &method,
                            Some(text.as_bytes()),
                            BodyFormat::Json as u16,
                            timeout,
                        )
                        .await
                }
                None => {
                    transport
                        .send(&method, None, BodyFormat::RawBinary as u16, timeout)
                        .await
                }
            }
            .map_err(map_call_error)?;
            print_response(&response, cli.raw)?;
        }
        Command::Notify(MethodMaybeBody { method, body }) => {
            let body_text = resolve_body(body.as_deref(), cli.body_file.as_deref())?;
            match body_text.as_deref() {
                Some(text) => {
                    transport
                        .notify(
                            &method,
                            Some(text.as_bytes()),
                            BodyFormat::Json as u16,
                            timeout,
                        )
                        .await
                }
                None => {
                    transport
                        .notify(&method, None, BodyFormat::RawBinary as u16, timeout)
                        .await
                }
            }
            .map_err(map_call_error)?;
        }
        Command::Completions(_) => {
            // Handled in `main` before the runtime is even built so that
            // `repe completions <shell>` works without a server, a runtime,
            // or even network permissions.
            unreachable!("completions should be intercepted in main()");
        }
    }

    Ok(())
}

/// Write a clap-derived completion script for `shell` to stdout. Invoked from
/// `main` before the tokio runtime is constructed; runs synchronously and
/// performs no network I/O.
fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "repe", &mut std::io::stdout());
}

/// Resolve the request body from the three possible sources, validating it as JSON.
/// Precedence: positional argument (with `-` meaning stdin) wins over `--body-file`.
/// Returns `None` only when no source was supplied.
fn resolve_body(
    positional: Option<&str>,
    body_file: Option<&Path>,
) -> Result<Option<String>, CliError> {
    match (positional, body_file) {
        (Some(_), Some(_)) => Err(CliError::Usage(
            "cannot combine a positional body with --body-file".into(),
        )),
        (Some("-"), None) => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| CliError::Usage(format!("could not read stdin: {e}")))?;
            validate_json(&text)?;
            Ok(Some(text))
        }
        (Some(text), None) => {
            validate_json(text)?;
            Ok(Some(text.to_owned()))
        }
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                CliError::Usage(format!(
                    "could not read --body-file {}: {e}",
                    path.display()
                ))
            })?;
            validate_json(&text)?;
            Ok(Some(text))
        }
        (None, None) => Ok(None),
    }
}

fn validate_json(text: &str) -> Result<(), CliError> {
    serde_json::from_str::<Value>(text)
        .map(|_| ())
        .map_err(|err| CliError::Usage(format!("invalid JSON body: {err}")))
}

/// Convert the raw `--timeout` value into a `Duration`, rejecting NaN, +/-inf,
/// and negative numbers explicitly. The previous behavior silently clamped
/// negatives to zero, which then fired immediately - a user-mistake outcome
/// that masquerades as a working configuration.
fn parse_timeout(secs: Option<f64>) -> Result<Option<Duration>, CliError> {
    match secs {
        None => Ok(None),
        Some(v) if !v.is_finite() || v < 0.0 => Err(CliError::Usage(format!(
            "--timeout must be a non-negative finite number, got {v}"
        ))),
        Some(v) => Ok(Some(Duration::from_secs_f64(v))),
    }
}

fn print_response(msg: &Message, raw: bool) -> Result<(), CliError> {
    let Some(decoded) = decode_response(msg)? else {
        // Empty bodies are common for void methods. Suppress them so the
        // user doesn't see a stray blank line on success. Note: a body
        // containing literal JSON `null` still decodes and prints as "null".
        return Ok(());
    };
    match decoded {
        DecodedBody::Json(value) => {
            let rendered = if raw {
                serde_json::to_string(&value)
            } else {
                serde_json::to_string_pretty(&value)
            };
            // serde_json::to_string is infallible for `Value`: every variant
            // has a defined serialization and no I/O is involved. If this
            // ever does fail we want a panic, not a silently-zero exit code.
            let s = rendered.expect("serde_json cannot fail to render a Value");
            println!("{s}");
        }
        DecodedBody::Utf8Raw(text) => {
            // `--raw` on a UTF-8 body emits the bytes verbatim, so
            // `repe --raw get /motd` behaves like a normal text fetch.
            print!("{text}");
        }
    }
    Ok(())
}

/// Decoded response body, after dispatching on the wire body format. UTF-8
/// stays separate from `Json(Value::String(..))` so callers can opt out of
/// JSON quoting (e.g. `--raw` printing a plain `/motd` body).
#[derive(Debug)]
enum DecodedBody {
    Json(Value),
    Utf8Raw(String),
}

/// Decode a response message body for printing. Returns `None` for empty
/// bodies (void responses); see [`print_response`].
fn decode_response(msg: &Message) -> Result<Option<DecodedBody>, CliError> {
    if msg.body.is_empty() {
        return Ok(None);
    }
    match BodyFormat::try_from(msg.header.body_format) {
        Ok(BodyFormat::Json) => Ok(Some(DecodedBody::Json(msg.json_body::<Value>().map_err(
            |e| CliError::Rpc(format!("server returned malformed JSON body: {e}")),
        )?))),
        Ok(BodyFormat::Beve) => Ok(Some(DecodedBody::Json(msg.beve_body::<Value>().map_err(
            |e| CliError::Rpc(format!("server returned malformed BEVE body: {e}")),
        )?))),
        Ok(BodyFormat::Utf8) => Ok(Some(DecodedBody::Utf8Raw(msg.body_utf8()))),
        Ok(BodyFormat::RawBinary) | Err(_) => Err(CliError::Rpc(format!(
            "server returned {} raw-binary bytes (cannot render as JSON)",
            msg.body.len()
        ))),
    }
}

fn map_call_error(err: RepeError) -> CliError {
    match err {
        RepeError::ServerError { code, message } => {
            CliError::Rpc(format!("server error ({code}): {message}"))
        }
        RepeError::Io(io) => CliError::Connection(format!("io error: {io}")),
        other => CliError::Rpc(format!("{other}")),
    }
}

async fn connect(url: &str) -> Result<Transport, CliError> {
    let scheme = detect_scheme(url);
    match scheme {
        Scheme::WebSocket(full_url) => WebSocketClient::connect(&full_url)
            .await
            .map(Transport::WebSocket)
            .map_err(|e| {
                CliError::Connection(format!("websocket connect to {full_url} failed: {e}"))
            }),
        Scheme::Tcp(addr) => AsyncClient::connect(&addr)
            .await
            .map(Transport::Tcp)
            .map_err(|e| CliError::Connection(format!("tcp connect to {addr} failed: {e}"))),
    }
}

enum Scheme {
    Tcp(String),
    WebSocket(String),
}

fn detect_scheme(url: &str) -> Scheme {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        return Scheme::WebSocket(url.to_string());
    }
    let stripped = url.strip_prefix("tcp://").unwrap_or(url);
    let addr = if stripped.is_empty() {
        format!("{DEFAULT_HOST}:{DEFAULT_PORT}")
    } else if has_port(stripped) {
        stripped.to_string()
    } else {
        format!("{stripped}:{DEFAULT_PORT}")
    };
    Scheme::Tcp(addr)
}

/// True if `s` already includes a port number. For IPv4/hostname inputs that
/// means a single literal `:`. For bracketed IPv6 the port lives after the
/// closing `]` (e.g. `[::1]:7000`); a bare `[::1]` is treated as portless so
/// the caller can append the default.
fn has_port(s: &str) -> bool {
    if s.starts_with('[') {
        s.split_once(']')
            .is_some_and(|(_, after)| after.starts_with(':'))
    } else {
        s.contains(':')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_get_when_only_path_given() {
        let argv = vec!["repe".into(), "/foo".into()];
        let out = rewrite_inferred(argv);
        assert_eq!(out, vec!["repe", "get", "/foo"]);
    }

    #[test]
    fn rewrites_set_when_path_and_body_given() {
        let argv = vec!["repe".into(), "/foo".into(), "42".into()];
        let out = rewrite_inferred(argv);
        assert_eq!(out, vec!["repe", "set", "/foo", "42"]);
    }

    #[test]
    fn skips_url_value_when_inferring() {
        let argv = vec![
            "repe".into(),
            "--url".into(),
            "ws://x".into(),
            "/foo".into(),
        ];
        let out = rewrite_inferred(argv);
        assert_eq!(out, vec!["repe", "--url", "ws://x", "get", "/foo"]);
    }

    #[test]
    fn skips_body_file_value_when_inferring() {
        let argv = vec![
            "repe".into(),
            "--body-file".into(),
            "/tmp/x.json".into(),
            "/foo".into(),
        ];
        let out = rewrite_inferred(argv);
        // Inferred mode picks `get` because no positional body follows; `--body-file`
        // is invalid with `get` and will be rejected at run-time, but the rewriter's
        // job is just to not get confused by the file path.
        assert_eq!(
            out,
            vec!["repe", "--body-file", "/tmp/x.json", "get", "/foo"]
        );
    }

    #[test]
    fn leaves_explicit_subcommand_alone() {
        let argv = vec!["repe".into(), "call".into(), "/foo".into()];
        let out = rewrite_inferred(argv.clone());
        assert_eq!(out, argv);
    }

    #[test]
    fn detects_websocket_url() {
        match detect_scheme("ws://localhost:9001/repe") {
            Scheme::WebSocket(s) => assert_eq!(s, "ws://localhost:9001/repe"),
            _ => panic!("expected websocket"),
        }
    }

    #[test]
    fn defaults_port_for_bare_host() {
        match detect_scheme("example.com") {
            Scheme::Tcp(s) => assert_eq!(s, "example.com:5099"),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn strips_tcp_prefix() {
        match detect_scheme("tcp://10.0.0.1:8080") {
            Scheme::Tcp(s) => assert_eq!(s, "10.0.0.1:8080"),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn resolve_body_rejects_double_source() {
        let err = resolve_body(Some("42"), Some(Path::new("/tmp/x"))).unwrap_err();
        match err {
            CliError::Usage(_) => (),
            _ => panic!("expected usage error"),
        }
    }

    #[test]
    fn resolve_body_returns_none_when_unspecified() {
        assert!(resolve_body(None, None).unwrap().is_none());
    }

    #[test]
    fn resolve_body_validates_positional() {
        let err = resolve_body(Some("not-json"), None).unwrap_err();
        match err {
            CliError::Usage(_) => (),
            _ => panic!("expected usage error"),
        }
    }

    #[test]
    fn resolve_body_reads_from_file() {
        let path =
            std::env::temp_dir().join(format!("repe-cli-resolve-body-{}.json", std::process::id()));
        std::fs::write(&path, br#"{"x":1}"#).unwrap();
        let result = resolve_body(None, Some(&path)).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.unwrap(), r#"{"x":1}"#);
    }

    #[test]
    fn resolve_body_rejects_missing_file() {
        let path = std::env::temp_dir().join(format!(
            "repe-cli-missing-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err = resolve_body(None, Some(&path)).unwrap_err();
        match err {
            CliError::Usage(msg) => assert!(msg.contains("could not read")),
            _ => panic!("expected usage error"),
        }
    }

    // ---------- detect_scheme ----------

    #[test]
    fn detects_secure_websocket_url() {
        match detect_scheme("wss://example.com:443/repe") {
            Scheme::WebSocket(s) => assert_eq!(s, "wss://example.com:443/repe"),
            _ => panic!("expected websocket"),
        }
    }

    #[test]
    fn empty_url_uses_default_host_and_port() {
        match detect_scheme("") {
            Scheme::Tcp(s) => assert_eq!(s, format!("{DEFAULT_HOST}:{DEFAULT_PORT}")),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn bare_ipv6_literal_gets_default_port_appended() {
        match detect_scheme("[::1]") {
            Scheme::Tcp(s) => assert_eq!(s, format!("[::1]:{DEFAULT_PORT}")),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn ipv6_literal_with_port_passes_through() {
        match detect_scheme("[::1]:7000") {
            Scheme::Tcp(s) => assert_eq!(s, "[::1]:7000"),
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn has_port_recognizes_ipv4_and_ipv6() {
        assert!(has_port("127.0.0.1:8080"));
        assert!(!has_port("example.com"));
        assert!(has_port("[::1]:9000"));
        assert!(!has_port("[::1]"));
        // A trailing colon with nothing after still counts as "has a port"
        // for our purposes; let the OS reject it on bind.
        assert!(has_port("host:"));
    }

    // ---------- rewrite_inferred edge cases ----------

    #[test]
    fn does_not_rewrite_non_slash_first_arg() {
        let argv = vec!["repe".into(), "status".into()];
        let out = rewrite_inferred(argv.clone());
        assert_eq!(out, argv, "no `/` prefix means no rewrite");
    }

    #[test]
    fn does_not_rewrite_when_help_flag_first() {
        let argv = vec!["repe".into(), "--help".into()];
        let out = rewrite_inferred(argv.clone());
        assert_eq!(out, argv);
    }

    #[test]
    fn dash_dash_disables_inferred_mode() {
        // `--` is the user explicitly opting out of inferred mode. The rewriter
        // must not insert `get`/`set` after it; clap then sees the path as a
        // literal positional and rejects it (the user can still invoke the
        // explicit `repe get /foo` form).
        let argv = vec!["repe".into(), "--".into(), "/foo".into()];
        let out = rewrite_inferred(argv.clone());
        assert_eq!(out, argv);
    }

    #[test]
    fn value_flag_list_matches_clap_globals() {
        // `VALUE_FLAGS` drives how the argv rewriter skips values. If a new
        // global value-taking flag is added to `Cli`, this test catches the
        // omission before users hit a confused inferred-mode rewrite.
        use clap::ArgAction;
        let cmd = Cli::command();
        let mut expected: Vec<String> = cmd
            .get_arguments()
            .filter(|a| a.is_global_set() && matches!(a.get_action(), ArgAction::Set))
            .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
            .collect();
        expected.sort();
        let mut actual: Vec<String> = VALUE_FLAGS.iter().map(|s| (*s).to_string()).collect();
        actual.sort();
        assert_eq!(actual, expected);
    }

    // ---------- decode_response ----------

    #[test]
    fn decode_response_returns_none_for_empty_body() {
        let msg = repe::Message::builder()
            .body_format(BodyFormat::RawBinary)
            .build();
        assert!(decode_response(&msg).unwrap().is_none());
    }

    #[test]
    fn decode_response_handles_json() {
        let msg = repe::Message::builder()
            .body_json(&serde_json::json!({"k": 1}))
            .unwrap()
            .build();
        match decode_response(&msg).unwrap().unwrap() {
            DecodedBody::Json(value) => assert_eq!(value["k"], 1),
            DecodedBody::Utf8Raw(text) => panic!("expected Json variant, got Utf8Raw({text:?})"),
        }
    }

    #[test]
    fn decode_response_handles_beve() {
        let msg = repe::Message::builder()
            .body_beve(&serde_json::json!({"k": 2}))
            .unwrap()
            .build();
        match decode_response(&msg).unwrap().unwrap() {
            DecodedBody::Json(value) => assert_eq!(value["k"], 2),
            DecodedBody::Utf8Raw(text) => panic!("expected Json variant, got Utf8Raw({text:?})"),
        }
    }

    #[test]
    fn decode_response_keeps_utf8_separate_from_json() {
        // The CLI prints UTF-8 bodies verbatim under `--raw`, so the decoder
        // must surface them distinctly from `Json(Value::String(..))`.
        let msg = repe::Message::builder().body_utf8("hello").build();
        match decode_response(&msg).unwrap().unwrap() {
            DecodedBody::Utf8Raw(text) => assert_eq!(text, "hello"),
            DecodedBody::Json(value) => panic!("expected Utf8Raw, got Json({value})"),
        }
    }

    #[test]
    fn decode_response_errors_on_nonempty_raw_binary() {
        let msg = repe::Message::builder()
            .body_format(BodyFormat::RawBinary)
            .body_bytes(vec![1, 2, 3])
            .build();
        match decode_response(&msg) {
            Err(CliError::Rpc(msg)) => assert!(msg.contains("raw-binary")),
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[test]
    fn decode_response_errors_on_unknown_body_format() {
        let msg = repe::Message::builder()
            .body_format_code(0xBEEF)
            .body_bytes(vec![1, 2, 3])
            .build();
        // Unknown format codes fall through to the same raw-binary diagnostic
        // path; the user just gets a clear "cannot render as JSON" message.
        match decode_response(&msg) {
            Err(CliError::Rpc(_)) => (),
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    // ---------- parse_timeout ----------

    #[test]
    fn parse_timeout_passes_through_none() {
        assert!(parse_timeout(None).unwrap().is_none());
    }

    #[test]
    fn parse_timeout_accepts_zero_and_positive() {
        // `0` is allowed: it is a valid `Duration` and the user may legitimately
        // want a "fire immediately" probe. We document, not reject.
        assert_eq!(parse_timeout(Some(0.0)).unwrap(), Some(Duration::ZERO));
        assert_eq!(
            parse_timeout(Some(1.5)).unwrap(),
            Some(Duration::from_secs_f64(1.5))
        );
    }

    #[test]
    fn parse_timeout_rejects_negative() {
        match parse_timeout(Some(-0.5)) {
            Err(CliError::Usage(msg)) => assert!(msg.contains("non-negative")),
            other => panic!("expected Usage error, got {other:?}"),
        }
    }

    #[test]
    fn parse_timeout_rejects_non_finite() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            match parse_timeout(Some(v)) {
                Err(CliError::Usage(msg)) => assert!(msg.contains("non-negative")),
                other => panic!("expected Usage error for {v}, got {other:?}"),
            }
        }
    }
}
