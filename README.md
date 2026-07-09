# CodexCont Rust

[English](README.md) | [中文](README_zh.md)

Continue-thinking middleware for Codex / OpenAI Responses-compatible APIs.

CodexCont is a small local Rust proxy between a coding agent and an upstream Responses endpoint. It detects the observed reasoning-truncation fingerprint (`usage.output_tokens_details.reasoning_tokens == 518 * n - 2`), silently opens continuation rounds, and folds the upstream SSE streams into one downstream response.

```text
Coding agent -> CodexCont -> Codex / Responses API
```

> Installing via an AI agent? Give it [`INSTALL-GUIDE-AGENT/AGENT.md`](INSTALL-GUIDE-AGENT/AGENT.md).

## Disclaimer

This project explicitly bypasses the observed OpenAI Codex reasoning-truncation behavior. If your use of this middleware is considered abusive, violates service terms, increases costs unexpectedly, or causes any other adverse consequence, you are solely responsible.

## What It Does

- Streams reasoning items to the client live.
- Buffers tentative final output (`message` and `function_call`) until the upstream terminal event proves whether the round was truncated.
- Drops tentative output and opens another upstream round when the truncation fingerprint is detected.
- Reconstructs one terminal response with proxy metadata after the final round.
- Passes non-matching traffic through unchanged.

The default continuation method is a hidden `phase: "commentary"` assistant message (`"Continue thinking..."`). A legacy synthetic `tool_pair` mode is also available.

## Requirements

- Rust toolchain with `cargo`
- Windows, macOS, or Linux for local source builds

The current release workflow publishes a Windows x64 binary.

## Quick Start

From this Rust project directory:

```powershell
cargo --version
cargo build --release
Copy-Item config.example.toml target\release\config.toml
target\release\codex-cont.exe
```

The server reads `config.toml` from the same directory as the running executable. If the file is absent, built-in defaults are used; start from `config.example.toml` for normal use.

The example config listens on `127.0.0.1:8787` and accepts POST requests at:

- `/v1/responses`

After building, you can also run the binary directly:

```powershell
target\release\codex-cont.exe
```

In a prebuilt Windows release zip, copy `config.example.toml` to `config.toml` beside `codex-cont.exe`, then run:

```powershell
Copy-Item config.example.toml config.toml
.\codex-cont.exe
```

## Windows Service

From an elevated PowerShell in the directory that contains `codex-cont.exe` and `config.toml`:

```powershell
.\codex-cont.exe install
.\codex-cont.exe stop
.\codex-cont.exe start
.\codex-cont.exe restart
.\codex-cont.exe uninstall
```

`install` registers the `CodexCont` service as Automatic and starts it immediately. The service command line is `codex-cont.exe service`; that subcommand is for the Windows Service Control Manager.

## Point Your Client at the Proxy

Use the proxy URL instead of the real upstream URL:

```text
http://127.0.0.1:8787/v1/responses
```

The example config uses:

```toml
[upstream]
url = "https://chatgpt.com/backend-api/codex/responses"
mode = "header"
```

With `mode = "header"`, the request header `Responses-API-Base` overrides the configured `url`. Without the header, requests fall back to the configured Codex URL.

For a generic Responses-compatible endpoint, send:

```text
Responses-API-Base: https://api.openai.com/v1
```

CodexCont appends `/responses` unless the supplied value already ends with `/responses`. This control header is stripped before forwarding upstream.

## Authentication

`config.toml` supports three auth modes. The example default is `passthrough`:

```toml
[auth]
mode = "passthrough"               # passthrough | inject | passthrough_then_inject
access_token = ""                  # sent as Authorization: Bearer <access_token>
chatgpt_account_id = ""            # sent as chatgpt-account-id when non-empty
```

Modes:

- `passthrough`: forward caller auth headers only; inject nothing.
- `inject`: override or set auth headers from config.
- `passthrough_then_inject`: keep caller auth when present, otherwise inject from config.

Security guard: if a request supplies `Responses-API-Base`, the proxy will not leak configured credentials to that request-supplied URL. If the current auth mode would inject configured credentials, the request is rejected with `400`.

Do not commit secrets. `config.toml` is ignored by `.gitignore`.

## When Continuation Is Applied

Continuation is applied only when all of these are true:

- `[continue].enabled = true`
- the request body is a JSON object
- `stream` is truthy
- reasoning is not explicitly disabled (`"reasoning": false`)
- when using `method = "tool_pair"`, the request does not declare a real tool named like `[continue].continue_tool_name`

Other requests are proxied as passthrough streams.

## Continuation Logic

For each upstream round:

1. Reasoning item events are forwarded live with rewritten `sequence_number` and `output_index`.
2. Message and function-call events are buffered as tentative output.
3. On the terminal event, CodexCont reads `usage.output_tokens_details.reasoning_tokens`.
4. If the token count matches `518 * n - 2`, is inside the configured tier window, has encrypted reasoning content, and safety caps allow it, CodexCont drops tentative output and opens another streaming round with the prior reasoning plus a continuation marker.
5. Otherwise it flushes the final buffered output and emits one reconstructed terminal event.

## Response Metadata

The final reconstructed response includes proxy metadata:

- `metadata.proxy_rounds`: per-round reasoning token counts and detected tier `n`.
- `metadata.proxy_billed_usage`: summed upstream token usage across hidden rounds.
- `metadata.proxy_stopped_reason`: present when a guard or error stops continuation.

Agent-facing `usage` is reconstructed to look like one response: first-round input and cached tokens, summed reasoning tokens, and the final round's non-reasoning output.

## Logging

`[log].level` supports `off | error | warn | info | debug`; unknown values are treated as `info`.

`warn` and `error` go to stderr. `info` and `debug` go to stdout. Logs include startup config, request IDs, request paths, body sizes, fold/passthrough mode, upstream status, and continuation summaries. Logs do not print request bodies, response bodies, `Authorization`, or `encrypted_content`.

Set `[log].dump_rounds_dir` to write per-round SSE dumps. Dump file paths are printed only at `debug` level.

## Tests

```powershell
cd rust
cargo test
```

Current offline coverage includes truncation math, incremental SSE parsing, fold/rewrite behavior with captured SSE fixtures, commentary and tool-pair continuation payloads, header transparency, upstream URL resolution, auth safety guard, EOF handling, and upstream-error handling.

## Project Layout

```text
src/
  app.rs       # axum router and request handler
  codex.rs     # truncation math and continuation payload builders
  config.rs    # exe-adjacent config.toml loader and config structs
  creds.rs     # upstream header/auth construction
  logging.rs   # stdout/stderr logging
  proxy.rs     # fold_stream state machine
  sse.rs       # incremental SSE parser/serializer
  store.rs     # in-memory ID store for optional stateful repair

tests/
  middleware.rs
  fixtures/

config.example.toml
```

## Limitations

- Final answer text is buffered until the terminal round proves it is not truncated, so final-answer first-token latency can be higher than a normal stream.
- Non-streaming requests are passed through rather than folded.
- The truncation detector is intentionally specific to the observed `518 * n - 2` fingerprint.
- Optional `repair_followup = "stateful"` uses in-memory process-local state and is not shared across proxy instances.

## Acknowledgements

This project would not exist without discussions in the LINUX DO community. Special thanks to @shinorochi and @dskdkj for pinning down the truncation mechanism and GPT's thinking model, and to @shinorochi for proposing the `commentary` input approach instead of faked tool calls.
