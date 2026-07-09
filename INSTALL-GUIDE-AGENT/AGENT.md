# CodexCont Rust - Installation Runbook for an AI Agent

> You are an AI coding agent reading this file because your user asked you to install CodexCont Rust for them. This is a runbook to execute on the user's machine, step by step.

CodexCont is a small local proxy between a coding agent and an upstream OpenAI Responses-compatible endpoint. It detects a reasoning-truncation fingerprint, silently asks the model to keep thinking, and folds multiple upstream streaming rounds into one downstream response.

## 0. How to Use This Runbook

1. Execute sections in order. Each section has a Verify step.
2. Speak the user's language. This file is English so agents parse it reliably.
3. Hard-stop steps require the user. Never fabricate credentials, skip the disclaimer, or modify an agent config before backing it up.
4. Use PowerShell commands on Windows unless the user is clearly using another shell.
5. Run build commands from the `rust` directory that contains `Cargo.toml` and `config.example.toml`; run the built proxy from the directory that contains `codex-cont.exe`.

## 1. Hard Stop: Get Consent

Tell the user, in their language, and wait for an explicit yes:

- CodexCont explicitly bypasses the observed OpenAI Codex reasoning-truncation behavior. If this violates terms, increases costs, or causes adverse consequences, the user is responsible.
- To wire this correctly, you may need to inspect local coding-agent configs such as `~/.codex/config.toml` or `~/.pi/agent/models.json`. These may contain secrets. You will not copy secret values into chat, this repo, memory, or committed files.

If the user declines, stop.

## 2. Preflight

```powershell
Get-Location
cargo --version
Get-ChildItem Cargo.toml, config.example.toml
```

Verify: `cargo --version` succeeds, and both `Cargo.toml` and `config.example.toml` exist. If Cargo is missing, stop and ask the user to install Rust from https://rustup.rs/ or point you at an existing toolchain.

## 3. Explore Agent Configs Read-Only

Read only the configs needed to understand how the user's agent reaches its model.

Likely paths:

- Codex: `~/.codex/config.toml`, `~/.codex/auth.json`
- Pi: `~/.pi/agent/models.json`, `~/.pi/agent/settings.json`

Determine:

- Which agent(s) exist.
- Whether each uses official OAuth login or a custom Responses-compatible provider.
- Current model, provider, base URL, and Responses vs Completions wire API.

Never print live secrets. Use placeholders in summaries.

## 4. Hard Stop: Ask What to Wire

Report what you found, then ask:

- Which agent(s) should use CodexCont: Codex, Pi, or both?
- Auth mode for `config.toml [auth].mode`: `passthrough`, `inject`, or `passthrough_then_inject`.
- Upstream: keep `https://chatgpt.com/backend-api/codex/responses` or use a custom Responses endpoint?

If the user uses a relay or aggregator, especially sub2api-based, warn them: many relays strip reasoning blocks, and CodexCont cannot work if reasoning is stripped across rounds.

## 5. Back Up Agent Configs Before Editing

Create a timestamped backup outside the repo.

```powershell
$ts = Get-Date -Format "yyyyMMdd-HHmmss"
$backup = Join-Path $env:USERPROFILE ".codexcont-backup\$ts"
New-Item -ItemType Directory -Force $backup | Out-Null

if (Test-Path "$env:USERPROFILE\.codex\config.toml") {
  Copy-Item "$env:USERPROFILE\.codex\config.toml" "$backup\codex.config.toml"
}
if (Test-Path "$env:USERPROFILE\.pi\agent\models.json") {
  Copy-Item "$env:USERPROFILE\.pi\agent\models.json" "$backup\pi.models.json"
}
```

Create `$backup\RESTORE.md` with:

- timestamp and OS;
- original path of each backed-up file;
- exact keys or blocks you are about to change;
- restore procedure using `Copy-Item`.

Verify: `Get-ChildItem $backup` shows the copied configs and `RESTORE.md`. Tell the user the backup path.

## 6. Configure CodexCont

Create local config without overwriting an existing one:

```powershell
if (-not (Test-Path config.toml)) {
  Copy-Item config.example.toml config.toml
}
```

The proxy reads `config.toml` from the same directory as the running executable. For a source build, copy the edited config to `target\release\config.toml` before starting `target\release\codex-cont.exe`. For a release zip, copy `config.example.toml` to `config.toml` beside `codex-cont.exe`.

Edit `config.toml` according to the user's choices:

- `[auth].mode`: usually `passthrough`.
- `[upstream].url`: default ChatGPT Codex backend or the chosen custom endpoint.
- `[upstream].mode`: `header` for per-request overrides, `fixed` for one configured upstream, `header_required` only when every request must supply `Responses-API-Base`.
- `[server].host` and `[server].port`: default `127.0.0.1:8787`.
- `[continue]`: keep defaults unless the user asks otherwise.

For `inject` or `passthrough_then_inject`, get `access_token` and any `chatgpt_account_id` from the user. Never invent credentials.

Show the resulting `[server]`, `[upstream]`, and `[auth]` blocks with secrets masked.

## 7. Build, Run, and Verify the Proxy

```powershell
cargo test
cargo build --release
Copy-Item config.toml target\release\config.toml -Force
target\release\codex-cont.exe
```

Expected startup output includes:

```text
[info] listening=http://127.0.0.1:8787
[info] paths=http://127.0.0.1:8787/v1/responses
```

From another shell, verify the port is reachable:

```powershell
Invoke-WebRequest -Method Post `
  -Uri http://127.0.0.1:8787/v1/responses `
  -ContentType application/json `
  -Body '{}' `
  -SkipHttpErrorCheck
```

Any HTTP response proves the proxy is listening. Connection refused means it is not running or the port differs.

The direct executable is:

```powershell
target\release\codex-cont.exe
```

The proxy must stay running while the agent uses it.

For a persistent Windows service, use an elevated PowerShell after the release config is next to the exe:

```powershell
target\release\codex-cont.exe install
Get-Service CodexCont
```

`install` registers `CodexCont` as an Automatic service and starts it immediately. Management commands are:

```powershell
target\release\codex-cont.exe stop
target\release\codex-cont.exe start
target\release\codex-cont.exe restart
target\release\codex-cont.exe uninstall
```

## 8. Point Agents at the Proxy

The local Responses base is:

```text
http://127.0.0.1:8787/v1
```

### Codex

If the user already has a custom provider, prefer changing only that provider's `base_url` to keep provider history grouped the same way:

```toml
[model_providers.<existing_id>]
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
```

Then set CodexCont `config.toml [upstream]` to the old provider URL:

```toml
[upstream]
url = "<old base_url>/responses"
mode = "fixed"
```

If the user uses official ChatGPT OAuth and needs a new provider, warn that switching `model_provider` can hide existing conversation history until restored. With approval, set Codex's top-level model selectors before the provider table:

```toml
model_provider = "codexcont"
model = "<current model>"

[model_providers.codexcont]
name = "CodexCont"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
```

Use proxy auth mode `passthrough` for OAuth so Codex's own auth is forwarded.

### Pi

Add or adjust a provider using the Responses API:

```json
{
  "providers": {
    "codexcont": {
      "baseUrl": "http://127.0.0.1:8787/v1",
      "api": "openai-responses",
      "apiKey": "<user-provided token>",
      "models": [
        { "id": "<model>", "name": "<model>", "reasoning": true }
      ]
    }
  }
}
```

`openai-completions` traffic will not be folded by this proxy.

## 9. End-to-End Verify

Run a real prompt through the selected agent and watch CodexCont stdout.

At `info` level, logs show request IDs, upstream status, fold/passthrough mode, and continuation summaries. When continuation fires, the final response metadata includes:

- `metadata.proxy_rounds`
- `metadata.proxy_billed_usage`
- `metadata.proxy_stopped_reason` when a guard or error stopped continuation

If `proxy_rounds` never appears, the prompt may simply not have hit the truncation fingerprint.

## 10. Troubleshoot

| Symptom | Likely Cause | Fix |
|---|---|---|
| Connection refused | Proxy not running or wrong port | Start it; confirm `[server].port`. |
| 404 or unknown endpoint | Client is not using Responses path/API | Use `/v1/responses` and Responses wire API. |
| 401 | Upstream auth missing or invalid | In `passthrough`, fix the agent login. In `inject`, set valid credentials. |
| 400 with `Responses-API-Base` | Credential-leak guard blocked configured credential injection | Use `passthrough` and caller-owned auth for per-request upstream overrides. |
| No folding | Reasoning stripped, reasoning disabled, or no truncation happened | Use an upstream that preserves reasoning and ensure reasoning is enabled. |
| Port in use | Another process owns 8787 | Change `[server].port` and update agent base URL. |

## 11. Restore / Uninstall

1. Stop `codex-cont.exe`, the running `target\release\codex-cont.exe` process, or run `target\release\codex-cont.exe uninstall` if installed as a service.
2. Restore agent configs from `$backup\RESTORE.md` using `Copy-Item`.
3. Remove shortcuts you created, if any.
4. Optionally remove local artifacts: `config.toml` and `target\`.
5. Verify the agent works against its original upstream again.
