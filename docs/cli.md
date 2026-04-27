# Command-Line Client

Enable the `cli` feature to build a `repe` binary that talks to any REPE server over TCP or WebSocket:

```
cargo install repe --features cli
```

## Transports and URLs

Transport is auto-detected from `--url`. `ws://` and `wss://` URLs use the WebSocket transport; anything else (including bare `host` or `host:port`) uses TCP. The default port is 5099.

```
repe --url 127.0.0.1:8082 get /counter
repe --url 127.0.0.1:8082 set /counter 42
repe --url 127.0.0.1:8082 call /add '{"a":1,"b":2}'
repe --url ws://127.0.0.1:8081/repe call /echo '"hello"'
```

A bare path triggers inferred mode: `repe /path` reads (empty body, response printed) and `repe /path '<json>'` writes (JSON body, response suppressed). Run `repe --help` for the full subcommand list, `--raw` / `--timeout` flags, and exit codes (`0` success, `1` connection or usage error, `2` server-returned RPC error).

## Body Sources

For larger or piped payloads, pass `-` as the positional body to read from stdin, or `--body-file PATH` to read from a file. The three body sources are mutually exclusive.

```
echo '{"a":3,"b":4}' | repe call /api/v1/add -
repe set /api/v1/config --body-file config.json
```

JSON bodies are validated client-side as syntactically valid JSON before the request is sent, so a typo gets a parse error locally rather than a round-trip to the server. Semantic validation (does this value match the schema for `/foo`?) remains the server's responsibility.

## Response Decoding

Responses are decoded uniformly across body formats: JSON and BEVE bodies are both rendered as pretty-printed JSON (or compact with `--raw`), UTF-8 bodies become JSON strings by default and print verbatim under `--raw` (so `repe --raw get /motd` behaves like a plain text fetch), and unparseable raw-binary responses surface as an RPC error rather than a silent decode failure.

## `REPE_URL` Environment Variable

Set `REPE_URL` to skip `--url` for repeated calls against the same server:

```
export REPE_URL=ws://127.0.0.1:8081/repe
repe get /api/v1/counter
repe set /api/v1/counter 42
```

An explicit `--url` flag always overrides `REPE_URL`.

## Shell Completions

```
repe completions zsh > ~/.zsh/_repe
```

Supported shells: `bash`, `zsh`, `fish`, `elvish`, `powershell`. Pipe the output into the location your shell expects.

## Troubleshooting

- **`tcp connect to ... failed: Connection refused`**: nothing is listening on that host:port. Check that the server is running and that `REPE_URL` / `--url` point at the right endpoint.
- **`websocket connect to ... failed`**: same idea, plus the server's WebSocket path must match (registry servers usually mount under something like `/repe`). The path is the part after `host:port` in the URL.
- **`server error (Method not found): Method not found: /foo`**: the server does not have a handler at that path. Registry-backed servers prefix all routes with their mount point, so `/foo` may need to be `/api/v1/foo`.
- **`server error (Invalid body): Expected JSON body`**: the server's handler required a body but the CLI sent none (the typical failure when `repe /path` is used against a function endpoint). Use `repe call /path '{}'` instead, or pass an explicit body.
- **`invalid JSON body: ...`**: client-side parse error before any bytes hit the wire. Quote your JSON properly; on most shells single quotes are safest: `repe call /add '{"a":1,"b":2}'`.
- **`request ... timed out after Nms`**: the server didn't respond inside the `--timeout` window. Either raise `--timeout` or remove it.
