# search-x

轻量级多渠道搜索聚合网关：一个 Rust 单二进制，同时提供 **MCP** 和 **REST** 接口。

- 渠道：SearXNG（免费）、Exa、Tavily、TinyFish、Firecrawl、grok（兜底）、WorkBuddy（agent2api / export）
- 每个渠道支持**多个 key 轮询**与失败冷却，**base URL 可自定义**（便于走反代）
- 三档路由：`fast`（串行兜底）/ `balanced`（并行）/ `full`（并行），独立超时、失败降级
- **RRF 融合排序** + URL 归一化去重（保留最长正文），SearXNG 摘要结果统一排最后
- **查询缓存**（TTL + 容量上限，请求可传 `no_cache` 绕过）
- **并行抓取**：多个 URL 同时抓，内容过短自动换渠道重试，正文按字符安全截断
- **GitHub 专项抓取**：issue / PR / release 链接走 GitHub API（可选 token 提高限额，失败回退普通渠道）
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
fast_limit = 10             # fast 每渠道条数
fast_channels = ["workbuddy", "tavily", "exa"]  # 串行兜底顺序
fast_timeout_secs = 5       # fast 每渠道超时上限

[mcp]
max_response_chars = 50000  # MCP 单次结果总字符上限
max_content_chars = 1500    # web_search 每条正文上限；全文用 web_fetch
max_fetch_content_chars = 10000 # web_fetch 单页正文上限（多页自动按 URL 数均分总预算）
max_queries = 3
default_limit = 8

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
| `fast` | WorkBuddy → Tavily → Exa（串行兜底） | 先用 WorkBuddy 出 10 条；报错/0 条才试 Tavily，再不行试 Exa；每次只用一个渠道，适合快速探索 |
| `balanced` | Exa + Tavily + WorkBuddy（国内版） | 默认编排，三渠道并发、每渠道 `default_limit`（默认 20 条；WorkBuddy 上游上限 10 条） |
| `full` | 所有已启用渠道（含 Firecrawl / TinyFish / 可选 SearXNG） | 最全，Firecrawl 抓正文慢，适合需要全文时 |

> `fast` 每个渠道的超时取 `min(channel.timeout_secs, fast_timeout_secs)`，默认 5 秒；WorkBuddy 卡住时会尽快切到 Tavily/Exa，而不是等满 10 秒。
>
> **SearXNG 默认不参与 `fast` / `balanced`**：当前实例的 Bing 引擎在没有正确 `cc` 参数时会返回与查询无关的结果，`google cse` 也常被限流；在修复前只建议通过 `full` 或显式 `channels` 使用。SearXNG 的 JSON API 没有条数参数，结果数由可用引擎决定。

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

MCP 客户端通常把一次工具结果整体放进上下文，因此 MCP 输出有独立预算：

```toml
[mcp]
max_response_chars = 50000      # 所有 query 合计；超出按 RRF 排名从尾部丢弃
max_content_chars = 1500        # web_search 每条正文上限，全文用 web_fetch
max_fetch_content_chars = 10000 # web_fetch 单页正文上限（多页按 URL 数均分总预算）
max_queries = 3                 # web_search 单次 query 数
default_limit = 8               # web_search 未传 limit 时每渠道条数（fast 仍使用 router.fast_limit）
```

超出预算时，结果里会带 `truncated: true`、`omitted_results`（搜索）或 `omitted_pages`（抓取）。REST `/search`、`/fetch` 不受这套预算影响，仍按 `[search]` / `[fetch]` 的配置返回。

### 健康检查

```http
GET /health
```
无需认证，只返回 `{"status":"ok"}`（不暴露版本号与渠道清单）。

### 配置热重载

```http
POST /reload
Authorization: Bearer <api-key>
```
重新读取 `config.toml` 并替换渠道、key、路由默认值与认证配置，**无需重启进程**。返回 `{"status":"reloaded","channels":[...]}`。

## 搜索日志

每次搜索（REST 和 MCP）追加一行 JSONL：

```json
{"ts":"2026-11-22T11:20:00Z","kind":"search","source":"rest","query":"...","mode":"balanced","limit":20,
 "no_cache":false,"requested_channels":["exa","tavily"],"elapsed_ms":1832,
 "channels":["exa","tavily"],"channel_elapsed_ms":{"exa":1292,"tavily":1820},
 "cached":false,"result_count":3,"result_content_chars":[3000,2860,1999],
 "errors":[]}
```

- `source`：`rest` 或 `mcp`，用于区分 REST 与 MCP 流量；测试/探针请求也可据此过滤。
- **默认只记元数据**：查询、模式、limit、是否绕过缓存、请求渠道、耗时、渠道、结果数、每条结果正文长度、错误，不记录返回内容，日志体积小。需要排查结果质量时把 `include_results` 设为 `true`，才会附加 `results`（正文按 `content_chars` 截断）。
- `result_content_chars`：按最终结果顺序记录每条正文的字符数，不记录正文本身。
- `channel_elapsed_ms`：每个渠道本次耗时（含失败渠道），用于定位慢渠道；缓存命中时为空。
- `cached: true` 表示命中查询缓存，用于统计命中率。
- `requested_channels` 只在调用方显式传 `channels` 时出现；`channels` 是本次实际返回了结果的渠道，两者可能不同。
- `limit` / `no_cache` 用于解释缓存 key 与缓存未命中：相同 query 但 limit 不同，或调用方传了 `no_cache: true`，都会绕过或更换缓存。

抓取（REST `/fetch` 与 MCP `web_fetch`）也会追加 `kind: "fetch"` 的元数据行：记录每个 URL 实际使用的渠道与正文长度（`chars`），不记录正文：

```json
{"ts":"2026-11-22T11:21:00Z","kind":"fetch","source":"rest","elapsed_ms":1137,"page_count":1,
 "pages":[{"url":"https://example.com/","channel":"tavily","chars":10000}],"errors":[]}
```

MCP 调用还会额外写一行最终输出预算日志（`kind: "mcp"`），用于观察是否触发 50K 字符/正文截断：

```json
{"ts":"2026-11-22T11:22:00Z","kind":"mcp","source":"mcp","tool":"web_search",
 "queries":["..."],"query_count":3,"mode":"balanced","limit":20,
 "response_chars":48582,"truncated":true,"omitted_results":107}
```

`web_fetch` 对应字段为 `tool:"web_fetch"`、`url_count`、`response_chars`、`truncated`、`omitted_pages`。

```toml
[log]
enabled = true
path = "logs/search.jsonl"
max_size_mb = 100        # 超过上限把当前文件轮转为 logs/search.jsonl.1
include_results = false  # true = 额外记录搜索结果明细（日志会明显变大）
content_chars = 200      # include_results=true 时每条结果正文截断长度，0 = 不记正文
```

## grok key 自动导入

`grok.keys` 为空时，启动/reload 会尝试读取 Claude Code 的 MCP 配置：

```
$CLAUDE_CONFIG_PATH 或 ~/.claude.json
→ mcpServers["grok-search-rs"].headers["X-Grok-Api-Key"]
```

`base_url` 仍需在 `config.toml` 填写（MCP 配置里没有 grok2api/AxonHub 的地址）。

## 搜索行为

- 单条搜索结果正文按 `[search] max_content_chars`（默认 8000 字符）截断，避免 Tavily PDF/长文等返回超大正文；截断按 Unicode 字符，不会切坏中文
- 正文为空的结果在进入 RRF 合并前丢弃；如果某渠道全部为空，等同于该渠道没有结果
- `limit` 是**每渠道**上限，最终结果数会随启用的渠道数增加；`fast` 模式按 `fast_channels` 串行兜底，条数由 `fast_limit` 控制（默认 10；显式传 `channels` 时不走串行兜底，`limit` 也不受 `fast_limit` 限制）
- MCP 另有一套更小的输出预算，见上文 `[mcp]`；REST 不受影响

```toml
[search]
max_content_chars = 8000
```

## 抓取行为

- 多个 URL **并行**抓取；单个 URL 内部默认按 `[fetch] channel_order` 降级，默认 **Firecrawl → Tavily → Exa → TinyFish**（Firecrawl 抓 SPA/长文最完整但较慢，Tavily 速度与正文较均衡，Exa 快但常只给 5000 字）；请求显式传 `channels` 时按调用方给出的顺序尝试，不受 `channel_order` 影响
- **GitHub issue / PR / release 链接优先走 GitHub API 专项抓取**：issue/PR 会合并评论与 PR review，release 直接取 release notes；失败或内容为空时才回退普通渠道
- `channels` 显式指定时，只有包含 `"github"` 才会启用 GitHub 专项抓取；例如 `{"urls":["https://github.com/o/r/issues/1"],"channels":["github"]}`
- GitHub token 可选：配置文件 `[fetch] github_token` 或环境变量 `GITHUB_TOKEN`（config 非空时优先）。匿名访问约 60 次/小时，配置 token 后限额更高且可读私有仓库
- 如果某渠道返回的内容过短（低于 `[fetch] min_content_chars`，默认 500 字符，典型是 SPA 只抓到导航文本），会继续尝试下一个渠道，最终采用内容最长的结果
- 返回正文按 `[fetch] max_content_chars`（默认 10000 字符）按字符截断（不会切坏中文）

```toml
[fetch]
channel_order = ["firecrawl", "tavily", "exa", "tinyfish"]  # 默认抓取顺序，可自行调整
github_token = "ghp_..."   # 可选；也可用 GITHUB_TOKEN 环境变量
```

## 渠道说明

| 渠道 | 搜索 | 抓取 | 备注 |
|---|---|---|---|
| SearXNG | ✅ | ❌ | 免费、无 key；当前 Bing 引擎结果质量不可用，默认不参与 `fast` / `balanced` |
| Exa | ✅ | ✅ | 单次最多 100 条；PDF/技术站可以，SPA/Reddit 不行 |
| Tavily | ✅ | ✅ | 10 条/查询；`include_raw_content` 默认 `false`（搜索只回摘要，正文交给 `/fetch`），需要时可在 `[channels.tavily]` 打开 |
| TinyFish | ✅ | ✅ | `X-API-Key`；默认未配置 |
| Firecrawl | ✅ | ✅ | SPA（小黑盒）最强；抓正文较慢 |
| grok | ✅ | ❌ | 直连 Responses API，仅兜底 |
| WorkBuddy | ✅ | ❌ | `mode = "agent2api"` 走 agent2api `/api/web-search/search`（上游固定 5 条）；`mode = "export"` 从 agent2api 导出接口取账号后直连（推荐，无需挂载数据库），最多 10 条，失败自动回退 agent2api 并记日志 |

### WorkBuddy export 模式（推荐）

`export` 模式不挂载 agent2api 数据库：它调用 agent2api 的账号导出接口 `POST /api/accounts/export-files`（`api_key` 需要管理权限），按 TTL 缓存返回的 `accessToken` 并直连 WorkBuddy 上游；刷新仍由 agent2api 负责，search-x 绝不调用 refresh 接口。

```toml
[channels.workbuddy]
mode = "export"
base_url = "http://agent2api.example.com:3065"
api_key = "YOUR_AGENT2API_KEY"
provider = "workbuddy"
max_results = 10
account_ttl_secs = 600
```

导出接口返回的账号包含明文 token，**只保存在进程内缓存，不写日志/不落盘**；`api_key` 等于 agent2api 管理权限，需妥善保管。

> 0.6.0 起已移除 `direct` 模式（不再支持旁路解密账号库）；旧配置请改为 `mode = "export"`，并删除 `db_path` / `key_path`。

## 开发

```bash
cargo test                                  # 67 个单元测试，无网络依赖
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
  --read-only --tmpfs /tmp \
  --cap-drop=ALL --security-opt=no-new-privileges \
  -v /path/to/config-dir:/config:ro \
  -v /path/to/logs:/logs \
  -e SEARCH_X_CONFIG=/config/config.toml \
  --restart unless-stopped \
  search-x
```

## 安全

- **认证默认开启**：支持 `bearer` / `x_api_key` / `both` / `custom` header；key 比较为常量时间；空 key 一律拒绝。
- **公开探活**只返回 `{"status":"ok"}`，不暴露版本号或渠道清单。
- **`/fetch` SSRF 防护**：只允许公网 http/https；拒绝内网/回环/链路本地/CGNAT/保留地址与 `.local` 等内网域名；并对域名做一次 DNS 解析，**所有解析结果必须是公网 IP**（阻止 `127.0.0.1.nip.io` 这类绕过）。
- **WorkBuddy export 模式**：`api_key` 需要 agent2api 管理权限；导出的明文 token 只保存在进程内缓存，不写日志、不落盘；导出失败只向客户端返回「账号来源不可用」。
- **GitHub 专项抓取**：token 只用于 `api.github.com` 的 `Authorization` 头，不写日志；其权限决定能读哪些私有仓库，公网部署请使用最小权限的只读 token。
- **公网部署建议**：
  - 用 nginx/Caddy 反代并启用 TLS，只暴露 `/search`、`/fetch`、`/mcp`；`/reload` 建议仅内网可达；
  - 反代层加请求频率限制与请求体上限；
  - 容器只绑定内网地址（`-p 127.0.0.1:3088:3088` 或内网 IP），不要 `-p 0.0.0.0`；
  - 容器使用 `--read-only`、`--cap-drop=ALL`、`--security-opt=no-new-privileges`；
  - export 模式不需要任何 agent2api 文件挂载；
  - 网关本身无文件读取接口：所有请求参数只能到达搜索/抓取渠道，不能指定文件路径。

环境变量：

| 变量 | 说明 |
|---|---|
| `SEARCH_X_CONFIG` | 配置文件路径（默认 `config.toml`） |
| `CLAUDE_CONFIG_PATH` | 可选；grok key 自动导入时读取的 Claude 配置路径（默认 `~/.claude.json`） |
| `GITHUB_TOKEN` | 可选；GitHub 专项抓取 token，`[fetch] github_token` 为空时使用 |
| `RUST_LOG` | 日志级别（默认 `search_x=info`） |
