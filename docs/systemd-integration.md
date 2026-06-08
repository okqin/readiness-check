# systemd 集成

本文是 `readiness-check` 第一版交付时的 systemd `ExecStartPre` 使用说明。

## 示例 Readiness YAML

随仓库交付的示例配置位于：

```text
examples/service-a.readiness.yaml
```

安装到 `/etc` 前先验证配置语法和领域约束：

```bash
cargo run -- --config examples/service-a.readiness.yaml --validate-config
```

生产环境建议复制到 `/etc/readiness-check/service-a.yaml` 这类绝对路径，并替换为真实依赖 URL。

## YAML Config Mode

生产 unit 推荐使用 YAML config mode，因为一个配置文件可以描述全部 HTTP dependencies 及共享策略：

```ini
[Unit]
Description=Service A
After=network-online.target
Wants=network-online.target
After=dep-api.service dep-worker.service
Requires=dep-api.service dep-worker.service

[Service]
Type=simple
TimeoutStartSec=11min
ExecStartPre=/usr/local/bin/readiness-check --config /etc/readiness-check/service-a.yaml
ExecStart=/usr/local/bin/service-a
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

示例 YAML 使用 `max-wait: 10m`，所以 unit 设置 `TimeoutStartSec=11min`。只要 `max-wait`
是有限值，systemd 的 `TimeoutStartSec` 就必须大于工具自己的有限 wait budget。这样
`readiness-check` 才能自己判断 readiness timeout 并返回退出码 `1`；否则 systemd 可能先杀掉
pre-start 命令，导致工具的 timeout 日志被遮蔽。

如果服务愿意无限等待依赖恢复，可以使用 `max-wait: infinity` 或省略 `max-wait`，并设置：

```ini
TimeoutStartSec=infinity
```

## Inline Mode

Inline mode 适合少量依赖或本地验证：

```ini
[Service]
Type=simple
TimeoutStartSec=2min
ExecStartPre=/usr/local/bin/readiness-check --check dep-api=https://dep-api.internal/ready=200 --check dep-worker=http://127.0.0.1:9000/health=204 --interval 3s --request-timeout 2s --max-wait 90s --tls-insecure-skip-verify
ExecStart=/usr/local/bin/service-a
```

服务依赖多个 HTTP readiness endpoint 时，推荐在同一次 `readiness-check` 调用里重复传入
`--check`。工具会在同一个 readiness loop 中检查全部依赖，并且只在同一轮里全部 ready 时退出
`0`。

## 重复 ExecStartPre

systemd 允许一个 unit 中声明多个 `ExecStartPre=`。它会按文件顺序执行；任意一条返回非零，都会停止后续
pre-start 命令，并阻止 `ExecStart` 继续运行。

```ini
ExecStartPre=/usr/local/bin/readiness-check --check dep-api=https://dep-api.internal/ready=200 --max-wait 30s
ExecStartPre=/usr/local/bin/readiness-check --check dep-worker=http://127.0.0.1:9000/health=204 --max-wait 30s
```

除非确实存在必须隔离的非 HTTP pre-start 步骤，否则优先使用一次 `readiness-check` 调用检查多个
HTTP dependencies。单次调用能让 interval、request timeout、max wait、TLS policy、日志和退出行为保持一致。

## 第一版验收覆盖

交付本 slice 前运行完整 scoped Rust gate：

```bash
cargo build
cargo test
cargo +nightly fmt --all --check
cargo clippy -- -D warnings
```

PRD 第一版验收标准覆盖如下：

| PRD 验收标准 | 覆盖方式 |
| --- | --- |
| 可编译为单个二进制 `readiness-check` | `cargo build` |
| 支持 `--config` YAML 主接口 | `cargo test test_should_validate_yaml_config_without_running_http_requests` |
| 支持 inline `--check name=url=expected_status` 快捷格式 | `cargo test test_should_validate_inline_check_without_running_http_requests` |
| `--config` 与 `--check` 互斥 | `cargo test test_should_reject_config_and_inline_checks_together` |
| 默认 `max-wait=infinity` | `cargo test test_should_apply_yaml_config_defaults` |
| 支持 `--max-wait infinity` | `cargo test test_should_retry_with_explicit_infinite_max_wait` |
| 支持并发检查 | `cargo test test_should_check_dependencies_concurrently_within_each_round` |
| 不跟随 redirect | `cargo test test_should_compare_redirect_status_without_following_location` |
| 不读取或记录 response body | `cargo test test_should_not_wait_for_or_log_response_body` |
| 不打印 URL | `cargo test test_should_exit_not_ready_without_printing_url_when_status_differs`，以及 signal、TLS、connection-refused 相关测试 |
| 支持全局 `--tls-insecure-skip-verify` | `cargo test test_should_accept_self_signed_https_when_cli_enables_insecure_tls` |
| 支持 YAML TLS policy | `cargo test test_should_apply_yaml_insecure_tls_to_all_https_checks` |
| 支持 `--validate-config` | `cargo test test_should_validate_inline_check_without_running_http_requests` 和 `cargo test test_should_validate_yaml_config_without_running_http_requests` |
| systemd `ExecStartPre` 中可直接使用 | 本文档给出 YAML 和 inline 示例；安装后用 `readiness-check --config /etc/readiness-check/service-a.yaml --validate-config` 验证配置，再用 `systemd-analyze verify service-a.service` 验证 unit |
| 交付并验证 example readiness YAML | `cargo test test_should_validate_shipped_example_readiness_yaml` |
| 所有单元测试和集成测试通过 | `cargo test` |
