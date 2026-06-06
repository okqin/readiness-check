# PRD: readiness-check

## 目标

实现一个 Linux 下可被 systemd `ExecStartPre` 调用的 readiness check 工具，用于在服务 A 启动前检查一组依赖服务是否满足预期状态。

所有依赖在同一轮检查中 ready 后，工具退出 `0`，systemd 才继续启动服务 A。否则工具持续检查，直到所有依赖 ready、达到有限 `max-wait`、收到终止信号，或遇到启动前配置错误。

## 背景

服务 A 的初始化依赖其他服务。仅使用 systemd 的 `After=` / `Requires=` 只能保证启动顺序，不能保证依赖服务已经真正可用。现有 shell 方案可用，但依赖 `httpcli` / `curl` 输出解析，长期维护性和可观测性有限。

Rust 实现可以直接发起 HTTP 请求、读取状态码、控制超时、并发检查多个依赖，并生成单个二进制用于部署。

## 用户场景

作为运维或服务开发者，我希望用一个可维护的 YAML 配置文件声明依赖服务：

```yaml
interval: 3s
request-timeout: 10s
max-wait: infinity
tls:
  insecure-skip-verify: false
checks:
  - name: dep1
    url: http://192.168.1.2:8000
    expected-status: 200
  - name: dep2
    url: https://192.168.1.3:9000/health
    expected-status: 204
```

并在 systemd 中配置：

```ini
ExecStartPre=/usr/local/bin/readiness-check \
  --config /etc/readiness-check/service-a.yaml
```

当 `dep1` 和 `dep2` 在同一轮检查中都返回预期状态码时，服务 A 才启动。

## 命令行接口

基础格式：

```sh
readiness-check \
  (--config path | --check name=url=expected_status [--check name=url=expected_status ...]) \
  [--interval duration] \
  [--request-timeout duration] \
  [--max-wait duration|infinity] \
  [--tls-insecure-skip-verify] \
  [--validate-config]
```

`--config` 是长期主接口，适合生产 systemd 配置。`--check` 是 inline 快捷格式，适合少量依赖或临时测试。

`--config` 和 `--check` 互斥，并且二者至少提供一种。混用时按参数错误处理，退出 `2`。

### 配置文件模式

```sh
readiness-check \
  --config /etc/readiness-check/service-a.yaml
```

全局运行参数可由 CLI 覆盖：

```sh
readiness-check \
  --config /etc/readiness-check/service-a.yaml \
  --max-wait 30m \
  --interval 5s \
  --tls-insecure-skip-verify
```

覆盖优先级：

```text
per-check request-timeout > CLI global option > config global option > built-in default
```

`interval`、`max-wait`、`tls.insecure-skip-verify` 没有 per-check 覆盖。

### Inline 模式

```sh
readiness-check \
  --interval 3s \
  --max-wait infinity \
  --check dep1=http://192.168.1.2:8000=200 \
  --check dep2=http://192.168.1.3:9000/health=204
```

`--check` 格式：

```text
name=url=expected_status
```

解析规则：

```text
第一个 = 分隔 name
最后一个 = 分隔 expected_status
中间全部属于 url
```

因此以下参数合法：

```sh
--check dep=https://example.com/health?tenant=a&ready=true=200
```

解析结果：

```text
name=dep
url=https://example.com/health?tenant=a&ready=true
expected-status=200
```

Inline 模式也允许使用全局 TLS 跳过校验开关：

```sh
readiness-check \
  --tls-insecure-skip-verify \
  --check dep1=https://127.0.0.1:8443/ready=200
```

该开关全局影响本次运行中的所有 HTTPS checks。

## YAML 配置 Schema

第一版配置文件使用 kebab-case 字段，并拒绝未知字段。

完整示例：

```yaml
interval: 3s
request-timeout: 10s
max-wait: infinity
tls:
  insecure-skip-verify: false
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
  - name: dep2
    url: https://127.0.0.1:8443/ready
    expected-status: 204
    request-timeout: 30s
```

根字段：

```text
interval                optional, default 3s
request-timeout         optional, default 10s
max-wait                optional, default infinity
tls                     optional
checks                  required, 1..64
```

`tls` 字段：

```text
insecure-skip-verify    optional, default false
```

`check` 字段：

```text
name                    required, unique
url                     required
expected-status         required
request-timeout         optional, overrides global request-timeout
```

第一版不支持配置文件环境变量展开。例如不支持：

```yaml
url: ${DEP1_URL}
```

配置文件必须指向存在且可读的普通文件。systemd 示例使用绝对路径，但 CLI 本身允许相对路径，便于本地测试和 CI。不强制文件权限，不禁止 symlink。

## 参数语义与校验

### Check 数量

`checks` 数量必须是 `1..64`。超过限制按参数错误处理，退出 `2`。

### Name

`name` 是日志和排障定位键。日志不会打印 URL，因此 `name` 必须全局唯一。

合法规则：

```text
regex: ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$
length: 1..64 bytes
```

合法示例：

```text
dep1
api-gateway
postgres.primary
cache_redis
```

非法示例：

```text
-prod
dep 1
服务A
dep/foo
dep=foo
```

### URL

URL 校验规则：

```text
只允许 http / https
必须包含 host
拒绝 userinfo
最大长度 2048 bytes
允许 localhost、127.0.0.1、内网 IP、IPv6、内网 DNS 名称
```

允许：

```text
http://127.0.0.1:8080/health
https://service.internal/ready
```

拒绝：

```text
file:///tmp/health
ftp://example.com/health
http://user@example.com/health
https://user:pass@example.com/health
```

### Expected Status

`expected-status` 必须是 `100..599` 的单个整数。

第一版不支持多个状态码或状态码范围。

### Duration

Duration 使用简单语法：

```text
格式：正整数 + 单位
单位：ms, s, m, h, d
不允许空格
不允许小数
不允许 0
infinity 仅允许用于 max-wait
```

支持：

```text
500ms
3s
10m
1h
30d
infinity
```

不支持：

```text
1m30s
1h 30m
00:01:30
PT1M30S
```

范围：

```text
interval:        100ms..=1h, default 3s
request-timeout: 1ms..=5m,  default 10s
max-wait:        1ms..=30d 或 infinity, default infinity
```

`interval` 是轮次之间的 sleep 时间，从本轮检查结束后开始计算，不做固定节拍调度。

## HTTP 检查语义

第一版只支持 HTTP GET，不提供 `method` 配置项。

检查规则：

1. 每轮并发检查所有 checks。
2. 对每个依赖发起 HTTP GET。
3. 不自动跟随重定向，3xx 按真实状态码参与匹配。
4. 比较实际 HTTP 状态码和 `expected-status`。
5. 只有同一轮所有 checks 都匹配时，退出 `0`。
6. 不锁存 ready 状态。每轮都重新检查所有依赖。
7. 不读取、不匹配、不记录响应 body。

`request-timeout` 的目标语义是单个 HTTP check 的总耗时上限。实现应尽量只等待拿到 HTTP status；如果所选 HTTP client 不支持精确到 status 的 timeout，则按该库可稳定实现的正常请求超时语义处理。使用 `reqwest` 时，推荐用 `tokio::time::timeout(client.get(url).send())` 包住 `send()`，拿到 `Response::status()` 后不主动读取 body。

第一版 HTTP client 采用 `reqwest` async + rustls，关闭默认 features，显式启用 rustls。启动时根据全局 TLS 配置构建一个共享 client，并配置不跟随 redirect。

## Max Wait 边界

`max-wait` 默认是 `infinity`。

有限 `max-wait` 的边界语义：

```text
每轮开始前检查 elapsed。
elapsed >= max-wait 时退出 1。
remaining = max-wait - elapsed。
本轮请求 effective-request-timeout = min(request-timeout, remaining)。
本轮执行完后如果全部 ready，退出 0。
否则如果 elapsed >= max-wait，退出 1。
否则 sleep min(interval, max-wait - elapsed)。
```

这样可以避免有限等待被最后一轮请求或最后一次 sleep 明显拖长，同时允许临近截止时间时做一次短请求。

无限等待时不能因为计时溢出或 duration 处理错误退出。

## TLS 策略

TLS 校验策略是全局配置，不是 per-check 配置。

默认严格校验证书：

```yaml
tls:
  insecure-skip-verify: false
```

可以通过 YAML 或 CLI 全局开启跳过证书校验：

```sh
readiness-check \
  --config /etc/readiness-check/service-a.yaml \
  --tls-insecure-skip-verify
```

CLI 开关只把最终值覆盖为 `true`，不提供 CLI 关闭开关。

启动日志必须输出最终值：

```text
readiness-check: waiting dependencies=2 interval=3s max-wait=infinity tls-insecure-skip-verify=false
```

## 不支持范围

第一版不支持：

- 自定义 HTTP headers。
- Authorization / token / API key 配置。
- POST / PUT / HEAD。
- 响应 body matcher。
- 自动跟随 redirect。
- JSON logs。
- 环境变量展开。
- JSON Schema 生成。
- `--dry-run`。
- `--print-effective-config`。

健康检查端点应优先设计成内网可访问、无敏感凭据的 readiness endpoint。

## 检查循环

工具启动后进入循环：

1. 打印启动日志。
2. 并发检查所有 checks。
3. 如果本轮全部 ready，打印成功日志并退出 `0`。
4. 如果存在未 ready 的依赖，记录首次详情、状态变化详情和周期摘要。
5. 如果达到有限 `max-wait`，打印超时日志并退出 `1`。
6. 否则 sleep `interval` 后继续下一轮。

每轮必须等待所有依赖完成或各自达到有效 request timeout。单个依赖失败不能导致工具崩溃，也不能提前中断本轮。

每轮一次性并发发出所有 checks，不提供并发上限参数。`checks` 数量上限 `64` 已作为资源限制。

## 信号处理

收到 `SIGTERM` 或 `SIGINT` 时，工具应优雅退出，退出码为 `1`，并打印终止日志：

```text
readiness-check: interrupted signal=SIGTERM elapsed=12.034s
```

该场景不是参数错误，也不是依赖 ready，因此不能退出 `2` 或 `0`。

## 日志格式

第一版使用 stderr 输出纯文本 key-value 日志，便于 systemd journal 收集。不提供 JSON 日志模式。

日志不打印 URL。通过 `name` 定位依赖，需要 URL 时回配置文件查看。

启动：

```text
readiness-check: waiting dependencies=2 interval=3s max-wait=infinity tls-insecure-skip-verify=false
```

首次发现依赖未 ready：

```text
readiness-check: dependency not ready name=dep1 expected=200 actual=503
readiness-check: dependency not ready name=dep2 expected=204 error=connection-refused
```

状态变化：

```text
readiness-check: dependency state changed name=dep1 expected=200 actual=200 ready=true
readiness-check: dependency state changed name=dep2 expected=204 actual=503 ready=false
```

周期摘要：

```text
readiness-check: still waiting not-ready=2 elapsed=30s
```

全部 ready：

```text
readiness-check: all dependencies ready elapsed=42.134s
```

超时失败：

```text
readiness-check: timeout waiting for dependencies elapsed=10m
```

配置验证成功：

```text
readiness-check: configuration valid dependencies=2 max-wait=infinity tls-insecure-skip-verify=false
```

配置错误：

```text
readiness-check: invalid configuration path=checks[1].expected-status error="must be between 100 and 599"
```

## 错误处理

配置和参数错误应给出精确字段路径，退出 `2`：

```text
readiness-check: invalid configuration path=checks[1].expected-status error="must be between 100 and 599"
readiness-check: invalid configuration path=max-wait error="duration must be 1ms..=30d or infinity"
readiness-check: invalid configuration path=checks[0].url error="userinfo is not allowed"
```

运行时请求错误应输出稳定错误类别，不打印 URL 和敏感底层细节：

```text
readiness-check: dependency not ready name=dep1 expected=200 error=connection-refused
readiness-check: dependency not ready name=dep2 expected=204 error=request-timeout
readiness-check: dependency not ready name=dep3 expected=200 error=tls
readiness-check: dependency not ready name=dep4 expected=200 error=dns
```

状态码不算 error，单独打印 `actual=503`。

第一版运行错误类别：

```text
request-timeout
dns
connection-refused
connection-closed
tls
http-protocol
request-error
```

错误分类尽力而为。无法可靠归类时使用 `request-error`，不要把底层库错误字符串直接打入日志。

## 退出码

```text
0  所有依赖在同一轮检查中 ready
1  超过 max-wait、收到终止信号，或 readiness 未完成
2  参数或配置错误
```

实现中应定义显式领域类型集中映射退出码，避免在代码中散落裸数字。

## Validate Config 模式

第一版支持 `--validate-config`：

```sh
readiness-check --config /etc/readiness-check/service-a.yaml --validate-config
```

行为：

1. 解析 CLI。
2. 读取配置文件或 inline checks。
3. 应用 CLI global overrides。
4. 执行完整 domain validation。
5. 成功时打印一行摘要到 stderr 并退出 `0`。
6. 失败时打印具体错误到 stderr 并退出 `2`。
7. 不发起 HTTP 请求。

Inline 模式也支持 `--validate-config`。

## systemd 集成

推荐使用配置文件：

```ini
[Unit]
Description=Service A
After=network-online.target
Wants=network-online.target

After=dep1.service dep2.service
Requires=dep1.service dep2.service

[Service]
Type=simple
TimeoutStartSec=infinity

ExecStartPre=/usr/local/bin/readiness-check \
  --config /etc/readiness-check/service-a.yaml

ExecStart=/usr/local/bin/service-a
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

少量依赖可使用 inline 快捷格式：

```ini
ExecStartPre=/usr/local/bin/readiness-check \
  --check dep1=http://192.168.1.2:8000=200 \
  --check dep2=http://192.168.1.3:9000/health=204
```

如果设置有限等待：

```ini
TimeoutStartSec=11min
ExecStartPre=/usr/local/bin/readiness-check --config /etc/readiness-check/service-a.yaml --max-wait 10m
```

systemd 的 `TimeoutStartSec` 应大于工具自己的有限 `--max-wait`。如果希望无限等待依赖恢复，可设置：

```ini
TimeoutStartSec=infinity
```

`ExecStartPre=` 可以在 unit 文件里重复声明，systemd 会按顺序执行，任意一个返回非零都会阻止 `ExecStart` 继续。本工具的推荐方式是用一个 `readiness-check` 调用检查多项 HTTP 依赖，减少 unit 文件复杂度。

## 代码结构建议

采用库 + CLI 分层：

```text
src/
  cli.rs
  config.rs
  duration.rs
  error.rs
  http.rs
  lib.rs
  main.rs
```

库层负责 parsing、validation、配置合并、HTTP check、检查循环等可测试逻辑。`main.rs` 负责 CLI entrypoint、日志初始化、信号处理和退出码映射。

## 依赖策略

应用依赖使用最新兼容主版本，关键运行时依赖显式 features，避免默认 features。`Cargo.lock` 负责锁定实际 patch 版本。

建议依赖：

```toml
[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
config = { version = "0.15", default-features = false, features = ["yaml"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "time"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt"] }
url = "2"

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
rstest = "0.26"
tempfile = "3"
wiremock = "0.6"
```

`config` crate 关闭默认 features，只启用 YAML。第一版不使用 env expansion、多 source layering，只把它作为 YAML 文件到 typed config 的入口。

`reqwest` 关闭默认 features，并显式使用 rustls。

## 测试要求

单元测试：

- 解析 inline check：`dep=http://x=200`。
- 支持 URL query 中包含 `=`。
- 拒绝空 name。
- 拒绝非法 name。
- 拒绝重复 name。
- 拒绝非法 URL。
- 拒绝 URL userinfo。
- 拒绝非法状态码。
- 解析合法 duration：`500ms`、`3s`、`10m`、`1h`、`30d`、`infinity`。
- 拒绝非法 duration：`0s`、`1m30s`、`1.5s`、`1 h`。
- `infinity` 仅允许用于 `max-wait`。
- 校验 duration 范围。
- 校验 `checks` 数量范围。
- 校验 YAML unknown fields 被拒绝。
- 校验 CLI global options 覆盖 config global options。
- 校验 per-check `request-timeout` 覆盖全局 `request-timeout`。
- 校验 `--config` 与 `--check` 互斥。
- 校验 `--validate-config` 不发起 HTTP 请求。

集成测试：

- mock HTTP 服务返回 `200`，工具退出 `0`。
- mock HTTP 服务返回 `503` 后变为 `200`，工具最终退出 `0`。
- mock HTTP 服务一直返回 `503`，有限 `--max-wait` 后退出 `1`。
- mock HTTP 服务不可连接，日志记录错误类别并持续重试。
- 多个依赖并发检查，全部 ready 后退出 `0`。
- 一个依赖 ready、一个依赖不 ready，工具继续等待。
- 不锁存 ready 状态：某依赖先 ready 后不 ready，本轮不应误判全部 ready。
- 不跟随 redirect：返回 `301` 且 expected 为 `200` 时判定 not ready。
- `--tls-insecure-skip-verify` 在 HTTPS 自签场景下生效。
- `--validate-config` 成功时打印摘要并退出 `0`。
- 参数错误退出 `2`。

测试工具：

```text
wiremock      HTTP mock
assert_cmd    CLI binary 断言
rstest        参数化测试
predicates    stderr 断言
tempfile      临时配置文件
```

## 示例配置交付

第一版提供示例 YAML，不生成 JSON Schema。

建议路径：

```text
examples/service-a.readiness.yaml
```

示例内容应覆盖：

- 全局 `interval`。
- 全局 `request-timeout`。
- 全局 `max-wait`。
- 全局 TLS 策略。
- 至少两个 checks。
- 至少一个 per-check `request-timeout`。

## 第一版验收标准

- 可编译为单个二进制 `readiness-check`。
- 支持 `--config` YAML 主接口。
- 支持 inline `--check name=url=expected_status` 快捷格式。
- `--config` 与 `--check` 互斥。
- 默认 `max-wait=infinity`。
- 支持 `--max-wait infinity`。
- 支持并发检查。
- 不跟随 redirect。
- 不读取或记录 response body。
- 不打印 URL。
- 支持全局 `--tls-insecure-skip-verify`。
- 支持 `--validate-config`。
- systemd `ExecStartPre` 中可直接使用。
- 所有单元测试和集成测试通过。
