# CodexCont Rust 版

Rust 版从运行目录读取 `config.toml`；文件不存在时使用内置默认配置。

## 运行

```powershell
cd rust
Copy-Item config.example.toml config.toml
cargo run --release
```

把客户端的 Responses 请求发到代理监听地址，例如：

```text
http://127.0.0.1:8787/v1/responses
```

如果 `[upstream].mode = "header"` 或 `"header_required"`，请求可通过 `Responses-API-Base` 传入上游 base URL，代理会自动拼成 `<base>/responses`；该控制头不会转发给上游。

## 日志

`[log].level` 支持 `off | error | warn | info | debug`，未知值按 `info` 处理。默认 `info` 会输出启动配置摘要、请求编号、请求路径、body 字节数、处理模式、上游状态和续轮摘要；`warn/error` 输出到 stderr，`info/debug` 输出到 stdout。日志不会打印请求正文、响应正文、`Authorization` 或 `encrypted_content`。

`[log].dump_rounds_dir` 非空时会继续写入每轮 SSE dump；只有 `debug` 级别会额外打印 dump 文件路径。

## 验证

```powershell
cd ..\python
uv run python tests/test_middleware.py

cd ..\rust
cargo test
```
