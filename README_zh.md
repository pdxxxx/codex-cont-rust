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

## 验证

```powershell
cd ..\python
uv run python tests/test_middleware.py

cd ..\rust
cargo test
```
