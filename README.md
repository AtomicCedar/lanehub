# LaneHub — 高性能协议传输与模型路由器

**LaneHub**（`lanehub`）是一套以 Rust 打造的高性能底层传输核心、CLI 与 OpenAI 兼容的本地代理网关，提供 **OpenCode v2.2.5 与 Kilo 协议规格兼容性**，并具备**多线路高可用路由与双栈连接池**（支持 IPv6/IPv4 双栈自动切换、SOCKS5/HTTP 传输代理池与本地网卡接口绑定）。

---

## 协议兼容与系统架构

OpenCode v2.2.5 客户端线协议（wire protocol）及其上游 `inference-next` 网关对协议帧格式、会话头与载荷结构有严格要求。未做协议对齐的普通 HTTP 客户端通常无法通过握手校验（`403` 或 `429`）。LaneHub 提供严格遵循 OpenCode v2.2.5 客户端规范的线协议兼容层：

1. **OpenCode v2.2.5 线协议兼容与会话头格式化（`x-opencode-session`）**：
   - 格式：`ses_<12-hex><14-base62>`，其中 12 位十六进制前缀编码 48 位时间戳位掩码 `~((now_ms << 12) | seq) & 0xffff_ffff_ffff`，保证会话单调递增。
2. **上游 SSE 流归一化**：
   - 上游端点要求 SSE 流式帧（`stream: true`）。LaneHub 透明管理上游 SSE 连接，并在下游客户端请求 `stream: false` 时将响应聚合为标准 JSON 补全结构。
3. **工具 schema 别名与线兼容**：
   - 上游协议帧要求逐字复刻 OpenCode 的 `read` / `shell` 工具 schema（`"strict": false`、`"max_completion_tokens": 32000`）以及顶层 JSON key 序列化顺序（`model`、`messages`、`tools`、`stream`、`stream_options`、`max_completion_tokens`）。
   - Rust 的 `serde_json` 输出的标准 UTF-8 字节与浏览器/JavaScript `JSON.stringify` 序列化一致。
   - 当下游客户端自带自定义 `read` / `shell` 工具时，LaneHub 在上游出口侧将其别名为 `read_client` / `shell_client`，并在入口侧还原工具调用名。
4. **多宿主 IPv6/IPv4 双栈传输与端点健康池**：
   - 并发预算与速率限制按 `(egress_ip, model)` 粒度隔离。
   - 默认配置（`--egress auto`）下，LaneHub 分别建立 IPv6（`[::]`）与 IPv4（`0.0.0.0`）独立 socket——让 Cloudflare WARP（`1.1.1.1`）和双栈环境拥有两条独立出口路径——同时支持任意 HTTP/SOCKS5 代理池与本地网卡 IP 绑定（`bind://<ip>`）。

---

## 支持的模型端点

### Lane 1：OpenCode Lane（`opencode/*`）

| Model ID | Context | Max Output | Vision | Reasoning | Notes |
| :--- | :---: | :---: | :---: | :---: | :--- |
| `deepseek-v4.1-flash-free` | 200K | 32K | ✓ | ✓ | DeepSeek V4.1 Flash reasoning & coding |
| `glm-5.3-flash-free` | 200K | 32K | ✓ | ✓ | GLM-5.3 Flash agentic & coding |
| `mimo-v2.6-flash-free` | 200K | 32K | ✓ | ✓ | Xiaomi MiMo V2.6 Flash |
| `beam-free` | 1M | 128K | — | ✓ | 1M-context high-speed reasoning |
| `nemotron-3.5-super-vl-free` | 1M | 128K | ✓ | ✓ | Vision-Language reasoning |
| `nemotron-3.5-lightning-free` | 262K | 262K | — | ✓ | Ultra-low-latency reasoning |
| `flint-40b-free` | 262K | 64K | — | ✓ | Recursive Flint 40B |
| `big-pickle` | 200K | 32K | — | ✓ | General reasoning |

### Lane 2：Kilo Lane（`kilo/*`）

| Model ID | Context | Max Output | Vision | Reasoning | Notes |
| :--- | :---: | :---: | :---: | :---: | :--- |
| `kilo-auto/free` | 256K | 32K | — | ✓ | Kilo 自动路由，轮转可用免费模型 |
| `reflection/beam-free` | 1M | 128K | — | ✓ | 1M-context reasoning（与 Lane 1 `beam-free` 同源） |
| `nvidia/nemotron-3.5-super-vl:free` | 1M | 128K | ✓ | ✓ | Vision-Language reasoning |
| `nvidia/nemotron-3-ultra-550b-a55b:free` | 1M | 64K | — | ✓ | NVIDIA 550B MoE 旗舰 |
| `nvidia/nemotron-3.5-lightning:free` | 1M | 64K | — | ✓ | Ultra-low-latency MoE |
| `cohere/north-mini-code:free` | 256K | 64K | — | ✓ | Cohere agentic coding 专用 |
| `dots-studio/dots-3-note-preview:free` | 512K | 448K | ✓ | ✓ | dots-3 笔记场景预览版 |
| `stealth/glyph-cluster` | 256K | 256K | — | ✓ | 隐身集群路由 |
| `openrouter/free` | 200K | 64K | ✓ | ✓ | OpenRouter 免费聚合池 |

（2026-11-08 实测全部可用）

> 模型元数据（上下文窗口 / 最大输出 / 视觉能力）在启动时**自动从两个上游的 `/models` 元数据端点刷新一次**（best-effort，拉取失败沿用内置值），内置表为兜底基线，元数据永不过时。

---

## 安装与快速上手

### 源码编译（Rust 1.80+）

```bash
git clone https://github.com/AtomicCedar/lanehub.git
cd lanehub
cargo build --release
```

编译产物为 `target/release/lanehub`，无其他运行时依赖。

### 1. 探测模型可用状态

```bash
cargo run --release -- probe
```

### 2. CLI 直接推理

```bash
cargo run --release -- run -m deepseek-v4.1-flash-free "Explain Rust ownership in one sentence."
```

### 3. 启动 OpenAI 兼容网关服务

```bash
# 默认监听 http://127.0.0.1:18788/v1，出口为 IPv6 + IPv4 双栈轮换
cargo run --release -- serve --bind 127.0.0.1:18788

# 搭配自定义 SOCKS5 / HTTP 代理池或本地网卡绑定：
cargo run --release -- --egress ipv6,ipv4,socks5://127.0.0.1:1080,http://127.0.0.1:7890#pool serve
```


### 4. Anthropic 协议兼容（Claude Code 直连）

网关同时暴露 Anthropic Messages 端点 `/v1/messages`（含流式），Claude Code 等 Anthropic SDK 客户端可直接使用免费模型池：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:18798
export ANTHROPIC_API_KEY=local
export CLAUDE_CODE_SIMPLE=1
claude --print "hello"
```

> 注意：纯对话完整可用；agent/工具调用模式依赖上游模型的结构化 tool_use 能力，免费模型可能以文本形式输出调用指令，属上游模型差异。


### 4. 用标准 OpenAI SDK / `curl` 调用（`stream: true` 与 `stream: false` 均支持）

```bash
curl http://127.0.0.1:18788/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4.1-flash-free",
    "stream": false,
    "messages": [{"role": "user", "content": "Hello from LaneHub!"}]
  }'
```

---

## Serverless 出口中继（Modal — 发请求开机，每次冷启动全新 IP）

`relay://` 出口将上游请求转发到部署在 [Modal](https://modal.com) 上的轻量鉴权 HTTP 中继（`modal/relay.py`）。中继容器是 serverless 的：按请求唤醒，每次冷启动都会从 Modal 云池获得一个**几乎全新**的出口 IP——因此一个 IP 上的免费额度 `429` 不必苦等可能长达数小时的 `Retry-After`，等容器回收后换新 IP 重试即可。

```bash
# 一次性设置（Modal Starter 计划免费，每月含 $30 额度）
pip install modal fastapi httpx
modal setup
modal secret create lanehub-relay RELAY_KEY=$(python -c "import secrets; print(secrets.token_urlsafe(24))")

# 部署中继 — 打印出的 URL 即中继端点
modal deploy modal/relay.py

# 与其他出口一起加入出口池
lanehub serve --egress auto,relay://https://<endpoint>.modal.run#key=<RELAY_KEY>
```

行为说明：

- 空闲容器在 `scaledown_window`（默认 60s）后被回收，**零成本**；下一个请求冷启动新容器（镜像已缓存时约数秒，首次启动较久）。
- 上游 `429`（免费额度耗尽）时，relay 出口忽略长 `Retry-After`，仅冷却 `RELAY_QUOTA_COOLDOWN_SECS`（90s）——恰好等容器回收完毕，让重试落在全新 IP 上。
- `#key=` 片段会从诊断信息中剥离；`/v1/egress` 永远不会显示它。
- 上游状态码、`Retry-After`、速率限制头、SSE 流与错误体原样透传，冷却分类与 SSE 改写行为与直连/代理出口完全一致。
- Starter 免费额度足够覆盖该负载；IP 池属于数据中心（云厂商）段，上游免费条款/反滥用政策请自行评估。

---

## 后台运行

### Daemon 模式（无需管理员权限）

```bash
# 后台分离启动 — 日志写入 ~/.local/share/lanehub/lanehub.log
lanehub serve --bind 127.0.0.1:18788 --daemon

# 停止
lanehub stop
```

`--daemon` 将 `lanehub serve` 作为分离进程（无控制台窗口）重新启动，PID 写入 lanehub 目录下的 `lanehub.pid`，日志重定向到 `lanehub.log`。其余参数（`--bind`、`--egress`、`--api-key`、`--probe-interval`、`--config`）原样转发给后台实例。分离的子进程不继承调用方的 stdio 句柄，可安全用于 shell 管道和脚本。

### 后台健康探测（配额感知）

`serve` 运行期间，后台 tokio 任务每隔 `--probe-interval` 秒（默认 **300**，`0` 禁用）对所有目录模型在全部出口路径上巡检：

- 至少有一个健康出口的模型正常探测；结果写入 `(exit, model)` 冷却状态，后台观测到的故障会立即把线上流量切到健康路由。
- **所有**出口都在冷却中的模型（如每日免费额度耗尽、`Retry-After` 约 11 小时）**直接跳过**——不浪费上游请求去续期冷却窗口。它们在 `/v1/status` 中显示为 `skipped: all exits on cooldown (quota429, 40763s left)`。
- 持续故障的路由按**指数退避**（30s → 1m → 2m → … 上限 1h），而 `Quota429` 窗口始终遵循服务器的 `Retry-After`。

最新快照通过 `GET /v1/status` 暴露：

```json
{
  "service": "lanehub", "version": "0.2.0", "uptime_secs": 42,
  "exits": 2, "active_cooldowns": [],
  "prober": { "enabled": true, "interval_secs": 300, "models_ok": 21, "models_total": 23, "reports": [...] }
}
```

> 注意：探测器会消耗免费额度（每轮每模型一个小请求）。若希望网关完全被动，用 `--probe-interval 0` 禁用。

---

## 网关端点

| Endpoint | Description |
| :--- | :--- |
| `GET /health`, `GET /v1/health` | 存活 + 出口数 + 活跃冷却（永不要求鉴权） |
| `GET /v1/models` | OpenAI 兼容模型列表 |
| `POST /v1/chat/completions` | OpenAI 兼容 chat completions（SSE 或 JSON） |
| `GET /v1/egress` | 已配置出口 + 活跃冷却详情 |
| `GET /v1/probe` | 手动触发全模型探测 |
| `GET /v1/status` | 运行时长、探测快照、冷却 |

## API Key 鉴权

```bash
lanehub serve --api-key my-secret
```

设置 API key 后，除 `/health` 外所有端点都要求 `Authorization: Bearer <key>`，否则返回 `401 invalid_api_key`。key 也可以来自 `LANEHUB_API_KEY` 环境变量或配置文件。

## 配置文件

可选 JSON 配置，搜索顺序（先命中先用）：

1. `--config <path>`（不存在则报错）
2. `$LANEHUB_CONFIG`
3. `./lanehub.json`

```json
{
  "bind": "127.0.0.1:18788",
  "egress": ["auto"],
  "no_failover": false,
  "api_key": null,
  "probe_interval_secs": 300
}
```

优先级：**CLI 参数 > 环境变量 > 配置文件 > 默认值**。

支持的环境变量：`LANEHUB_EGRESS`、`LANEHUB_API_KEY`、`LANEHUB_BIND`、`LANEHUB_PROBE_INTERVAL`、`LANEHUB_CONFIG`、`RUST_LOG`（tracing 过滤器，默认 `info`）。

## 配额无缝衔接（quota-wait 与随机轮换）

> relay 出口默认**严格备胎**：直连出口（ipv4/ipv6/本地代理）全部健康时绝不消耗 relay 额度；只有直连都在配额冷却时，relay 才作为兜底启用（并享受 90s 短冷却换新 IP）。

> **实测说明（2026-11-08/09）**：OpenCode 免费端点按出口 IP 限额（本机实测 `Retry-After` 最长观察到 ~2h；同类项目 Nystic-Shadow/Opencode2api 实测免费配额**每天 UTC 午夜重置**）。窗口重置后同一 IP 可直接出字；窗口内则所有模型一律 429。
> 出口 IP 类型决定体验：**住宅/家宽 IP** 配额干净、成功率高；**数据中心 IP**（云 VPS/机房 ASN）的匿名免费池常被多人烧光，容易长时间 429——搭配 relay 出口或多代理池可缓解。配合本页的随机轮换 + quota-wait + 多出口，可在窗口间隔内保持可用性。


- **随机轮换**：无会话亲和的请求每次从出口池中**随机**选起点，配额冷却结束后流量自动摊开到各出口，不会总压同一个 IP。
- **quota-wait**（默认开启，`--quota-wait 120`）：当某模型的所有出口都返回 429 且最短的 `Retry-After` ≤ 等待上限时，网关在请求内倒计时并自动重试一次——调用方无感知，等价于“配额窗口一到立刻续上”。设 `--quota-wait 0` 关闭（立即返回 429）。
- **冷却恢复**：全部出口冷却时，探测/重试优先选**最先到期**的出口，窗口一过立即恢复。
- 想真正“一直可用”：配多个出口（`--egress auto,socks5://...,relay://...`），轮换 + quota-wait + failover 三层叠加后，单 IP 的 2 小时空窗由其他出口顶上。

---

## 测试

```bash
cargo test   # 32 个单元测试：config、catalog、egress 冷却池（含退避与可用性）、线协议指纹、SSE 改写、HTTP 鉴权中间件
```

---

## License

MIT


---

## 测试

```bash
cargo test   # 32 个单元测试：config、catalog、egress 冷却池（含退避与可用性）、线协议指纹、SSE 改写、HTTP 鉴权中间件
```

---

## License

MIT
