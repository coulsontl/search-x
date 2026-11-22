# search-x

轻量级多渠道搜索聚合网关：一个 Rust 单二进制，同时提供 **MCP** 和 **REST** 接口。

- 渠道：SearXNG（免费）、Exa、Tavily、TinyFish、Firecrawl、grok（兜底）、WorkBuddy（走 agent2api）
- 每个渠道支持**多个 key 轮询**与失败冷却，**base URL 可自定义**（便于走反代）
- 三档路由：`fast` / `balanced` / `full`，并行调用、独立超时、失败降级
- **RRF 融合排序** + URL 归一化去重（保留最长正文），SearXNG 摘要结果统一排最后
- **查询缓存**（TTL + 容量上限，请求可传 `no_cache` 绕过）
- **并行抓取**：多个 URL 同时抓，内容过短自动换渠道重试，正文按字符安全截断
- 认证层：多个自定义 API key，支持 `Authorization: Bearer`、`X-API-Key` 或自定义 header
- SSRF 防护、请求体/数量/长度限制；JSONL 搜索日志轮转；配置 `POST /reload` 热重载；可选全局出站代理

文档：[AGENTS.md](AGENTS.md)（架构与开发约定）、[docs/PITFALLS.md](docs/PITFALLS.md)（踩坑记录）。

## 快速开始

```bash
cp config.example.toml config.toml
# 填入你的 key
cargo run
```

```bash
curl -s http://127.0.0.1:3088/health
curl -s -X POST http://127.0.0.1:3088/search \
  -H "Authorization: Bearer <your-api-key>" \
  -H "Content-Type: application/json" \
  -d '{"queries":["NVIDIA 621.35 hotfix"],"mode":"balanced"}'
```

## 配置

完整示例见 `config.example.toml`。关键字段：

```toml
[auth]
enabled = true
keys = ["key-1", "key-2"]   # 自定义 API key，可多个
mode = "both"               # bearer | x_api_key | both | custom
# custom_header = "x-search-x-key"

[router]
default_mode = "balanced"
default_limit = 20

[proxy]                              # 可选：所有渠道共用出站代理
url = "http://proxy.example.com:7890"
no_proxy = "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16"

[channels.exa]
base_url = "https://api.exa.ai"     # 可换成反代地址
keys = ["..."]
strategy = "round_robin"            # round_robin | random | failover
timeout_secs = 15
```

### 多 key 策略

| 策略 | 行为 |
|---|---|
| `round_robin` | 依次轮询可用 key |
| `random` | 随机取一个可用 key |
| `failover` | 优先第一个，失败再换下一个 |

失败处理：`401/403` 禁用该 key；`429` 冷却 60 秒；`5xx`/超时累计 5 次后冷却 5 分钟。

### 查询缓存

`[cache]` 默认开启：相同 `query + 模式/渠道 + limit` 在 `ttl_secs`（默认 300 秒）内直接返回缓存结果，响应里带 `"cached": true`、`elapsed_ms: 0`。进程内缓存，最多 `max_entries`（默认 500）条，过期或超额自动淘汰，无需外部依赖。

请求体传 `"no_cache": true` 可跳过缓存强制重新搜索（结果仍会写回缓存），MCP `web_search` 工具同样支持该参数。

### 融合排序

多渠道结果按 **RRF（Reciprocal Rank Fusion，k=60）** 融合：每个渠道内部的顺序视为排名，同一页面跨渠道出现时分数累加，最终按分数排序；同一页面合并时保留最长正文与标题。SearXNG 只有短摘要，统一排在最后。

### 出站代理

`[proxy]` 可选；配置后所有渠道的请求都走该 HTTP 代理，`no_proxy` 里的主机（内网自建 SearXNG / agent2api / AxonHub 等）保持直连。支持 IP、域名和 CIDR（如 `192.168.0.0/16`），逗号分隔。修改后 `POST /reload` 即可生效。

## 路由模式

| 模式 | 渠道 | 用途 |
|---|---|---|
| `fast` | SearXNG | 免费、最快（约 0.5–0.7s，单次约 20 条） |
| `balanced` | SearXNG + Exa + Tavily + WorkBuddy（国内版） | 默认编排，四渠道并发、每渠道 20 条；实测合并去重后约 50 条（约 0.5–4s） |
| `full` | 所有已启用渠道（再加 Firecrawl / TinyFish） | 最全，Firecrawl 抓正文慢，适合需要全文时 |

> SearXNG 的 JSON API 没有条数参数，结果数由可用引擎决定（当前实例启用 bing + google cse，单次 28–30 条，翻页不可用）；search-x 按 `limit` 截断。

请求体里的 `channels` 数组可覆盖 `mode`，只调用指定供应商（如 `["exa"]`）；指定后若无结果，仍会触发 grok 兜底。

### 去重

多渠道结果合并时按 URL 归一（忽略 scheme、`www.`、末尾斜杠、fragment 与 `utm_*`/`spm`/`fbclid` 等跟踪参数）去重；同一页面出现在多个渠道时保留**最长的正文和标题**、首个发布日期。顺序：非 SearXNG 渠道在前，SearXNG 只有短摘要、统一排在最后。

未知查询参数（如 `?id=1`、`?page=2`）**保留**在去重 key 中——无法判断语义，一律忽略会把不同内容误合并。

`grok` 配置 `fallback_only = true` 时只在其他渠道全部失败/无结果时调用。

## 接口

### REST

```http
POST /search
{"queries":["..."],"mode":"balanced","channels":["exa","tavily"],"limit":10}

POST /fetch
{"urls":["https://..."]}
```

### MCP

```http
POST /mcp
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"web_search","arguments":{"queries":["..."],"mode":"fast"}}}
```

工具：`web_search`、`web_fetch`。

### 健康检查

```http
GET /health
```
无需认证，只返回 `{"status":"ok","version":"..."}`（不暴露渠道清单）。

### 配置热重载

```http
POST /reload
Authorization: Bearer <api-key>
```
重新读取 `config.toml` 并替换渠道、key、路由默认值与认证配置，**无需重启进程**。返回 `{"status":"reloaded","channels":[...]}`。

## 搜索日志

每次搜索（REST 和 MCP）追加一行 JSONL：

```json
{"ts":"2026-11-22T11:20:00Z","query":"...","mode":"balanced","elapsed_ms":1832,
 "channels":["exa","tavily"],"result_count":19,"results":[...],"errors":[]}
```

```toml
[log]
enabled = true
path = "logs/search.jsonl"
max_size_mb = 100      # 超过上限把当前文件轮转为 logs/search.jsonl.1
content_chars = 200    # 每条结果正文截断长度，0 = 不记正文
```

## grok key 自动导入

`grok.keys` 为空时，启动/reload 会尝试读取 Claude Code 的 MCP 配置：

```
$CLAUDE_CONFIG_PATH 或 ~/.claude.json
→ mcpServers["grok-search-rs"].headers["X-Grok-Api-Key"]
```

`base_url` 仍需在 `config.toml` 填写（MCP 配置里没有 grok2api/AxonHub 的地址）。

## 抓取行为

- 多个 URL **并行**抓取；单个 URL 内部按渠道顺序降级（Exa → Tavily → Firecrawl → TinyFish）
- 如果某渠道返回的内容过短（低于 `[fetch] min_content_chars`，默认 500 字符，典型是 SPA 只抓到导航文本），会继续尝试下一个渠道，最终采用内容最长的结果
- 返回正文按 `[fetch] max_content_chars`（默认 10000 字符）按字符截断（不会切坏中文）

## 渠道说明

| 渠道 | 搜索 | 抓取 | 备注 |
|---|---|---|---|
| SearXNG | ✅ | ❌ | 免费、无 key；`fast` 模式 |
| Exa | ✅ | ✅ | 单次最多 100 条；PDF/技术站可以，SPA/Reddit 不行 |
| Tavily | ✅ | ✅ | 10 条/查询，`include_raw_content` 带正文 |
| TinyFish | ✅ | ✅ | `X-API-Key`；默认未配置 |
| Firecrawl | ✅ | ✅ | SPA（小黑盒）最强；抓正文较慢 |
| grok | ✅ | ❌ | 直连 Responses API，仅兜底 |
| WorkBuddy | ✅ | ❌ | 走 agent2api `/api/web-search/search`，需把 `workbuddy` 加入其 `selection.search` |

## 开发

```bash
cargo test                                  # 18 个单元测试，无网络依赖
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # CI 同款检查
```

CI（GitHub Actions）在 push/PR 时运行上述检查；推送到 `main` 或打 `v*` 标签会构建并推送镜像到 GHCR：
`ghcr.io/<owner>/search-x:latest`、`:<tag>`、`:sha-<commit>`。

## 部署

> **务必挂载目录，不要挂单个配置文件**：单文件 bind mount 在宿主机编辑保存（rename 换 inode）后，容器内仍指向旧 inode，`POST /reload` 会静默读到旧配置。详见 [docs/PITFALLS.md](docs/PITFALLS.md)。

```bash
# 本地构建
docker build -t search-x .

# 或使用 CI 构建的镜像
docker pull ghcr.io/<owner>/search-x:latest

docker run -d --name search-x -p 127.0.0.1:3088:3088 \
  -v /path/to/config-dir:/config:ro \
  -v /path/to/logs:/logs \
  -e SEARCH_X_CONFIG=/config/config.toml \
  --restart unless-stopped \
  search-x
```

环境变量：

| 变量 | 说明 |
|---|---|
| `SEARCH_X_CONFIG` | 配置文件路径（默认 `config.toml`） |
| `CLAUDE_CONFIG_PATH` | 可选；grok key 自动导入时读取的 Claude 配置路径（默认 `~/.claude.json`） |
| `RUST_LOG` | 日志级别（默认 `search_x=info`） |
