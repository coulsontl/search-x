# AGENTS.md

给 AI agent（以及新贡献者）的项目说明：架构、命令、约定、已知坑。面向使用者的说明见 [README.md](README.md)。

## 项目定位

轻量级多渠道搜索聚合网关（Rust 单二进制）：并行查询多个搜索渠道，RRF 融合排序、URL 去重合并，通过 REST 与 MCP 两种协议对外服务。除渠道 API 外不依赖任何外部服务（无数据库、无消息队列）。

## 架构

```
main.rs            进程入口：加载配置 → 构建 runtime → 监听端口
server.rs          HTTP 路由 + API key 认证 + 请求限制
mcp.rs             MCP JSON-RPC（initialize / tools/list / tools/call / ping）
router.rs          路由核心：模式选择渠道、RRF 合并、查询缓存、/fetch 降级
channels/          渠道适配器（每个文件一个渠道，实现 Channel trait）
github_fetch.rs    GitHub issue/PR/release 专项抓取（api.github.com，可选 token）
workbuddy_accounts.rs  WorkBuddy 账号来源：调 agent2api 导出接口（不刷新 token）
model.rs           数据模型 + URL 归一/去重/RRF 算法
cache.rs           进程内查询缓存（TTL + 容量）
config.rs          config.toml 结构与默认值
runtime.rs         装配层：配置 → 渠道实例 → Router
logger.rs          JSONL 搜索日志（轮转）
url_guard.rs       SSRF 防护（仅允许公网 http/https）
keypool.rs         多 key 轮询/冷却/禁用
```

数据流：`HTTP/MCP → 认证 → SearchRequest → Router::search → 并行渠道 → rrf_merge → move_searxng_last → 缓存/日志 → 响应`。

## 常用命令

```bash
cargo build --release        # 构建
cargo test                   # 单元测试（67 个，无网络依赖）
cargo fmt --check            # 格式检查
cargo clippy --all-targets -- -D warnings   # 静态检查（CI 同款）
docker build -t search-x:dev .            # 构建镜像
```

本地运行：

```bash
cp config.example.toml config.toml   # 填入真实 key（config.toml 已被 .gitignore）
cargo run
curl -s http://127.0.0.1:3088/health
```

## 代码约定

- 注释用中文，变量/函数用描述性英文命名
- 遵循 KISS：单文件职责单一，不做过度抽象；新增渠道 = 新增 `channels/<name>.rs` + 在 `runtime.rs` 装配
- 渠道错误统一走 `transport_error` / `status_error`，不把 URL、key、上游响应全文泄漏给客户端
- 认证比较必须常量时间（`server.rs::constant_time_eq`）
- 新增用户输入入口时，必须校验数量/长度上限；`/fetch` 的 URL 必须过 `url_guard::is_public_http_url_resolved`（字面量检查 + DNS 解析校验）
- 改行为必须补测试；改 URL 归一/去重/排序必须更新 `model.rs` 的测试

## 关键设计

| 主题 | 决策 |
|---|---|
| 融合排序 | RRF k=60；每个渠道内部顺序即排名；同页跨渠道分数累加 |
| 去重 | URL 归一（忽略 scheme/www/末尾斜杠/fragment/跟踪参数）+ 最长正文/标题合并；未知 query 参数保留 |
| 搜索正文 | 单条结果按 `[search] max_content_chars`（默认 8000）字符级截断；空正文结果在 RRF 前过滤 |
| 路由模式 | `fast`=WorkBuddy→Tavily→Exa **串行兜底**（每渠道 10 条，第一个有结果即停；每渠道超时取 `min(channel.timeout, fast_timeout_secs=5)`）；`balanced`=Exa+Tavily+WorkBuddy 并行；`full`=所有已启用渠道；显式 `channels` 覆盖 mode，仍走并行 |
| SearXNG | 结果只有摘要，统一排最后；JSON API 无条数参数；Bing 引擎当前结果质量不可用，默认不参与 `fast` / `balanced` |
| MCP 输出 | 独立预算：`max_response_chars`（所有 query 合计，超出按 RRF 尾部丢弃）、`max_content_chars`（搜索正文）、`max_fetch_content_chars`（抓取正文）、`max_queries`、`default_limit`；超限返回 `truncated` / `omitted_results` / `omitted_pages` |
| 缓存 | 进程内 TTL + 容量；key = query + 模式/渠道 + limit；`no_cache: true` 绕过读取但仍写回；命中时清空 `channel_elapsed_ms` |
| 日志 | JSONL；**默认只记元数据**（`include_results=false`，不记返回内容）；`source` 区分 `rest`/`mcp`；记录 `limit`、`no_cache`、显式 `requested_channels`、`result_content_chars`（每条结果正文长度），便于解释缓存和内容质量；`channel_elapsed_ms` 记录每渠道耗时（含失败渠道）；`cached: true` 可统计命中率；`kind: "fetch"` 记录每 URL 的渠道与正文长度（不记正文）；MCP 另写 `kind:"mcp"` 行，记录 `response_chars`、`truncated`、`omitted_results`/`omitted_pages` |
| 抓取 | 多 URL 并行；单 URL 按 `[fetch] channel_order` 降级（默认 **Firecrawl → Tavily → Exa → TinyFish**），显式 `channels` 时按调用方顺序；内容 < `min_content_chars` 时继续尝试下一渠道取最长；GitHub issue/PR/release 先走 `api.github.com` 专项抽取，失败再回退 |
| 认证 | 默认开启；无 key 时全部拒绝；支持 bearer / x-api-key / both / custom header |
| 安全 | SSRF 白名单（拒绝私网/回环/链路本地/CGNAT/内网域名 + DNS 解析校验）、1MB body 上限、REST 查询 ≤10 条、MCP 查询 ≤`mcp.max_queries`（默认 3）、单条 ≤4000 字符、URL ≤10 个；`/health` 不返回版本；账号来源错误对客户端脱敏；GitHub 专项抓取 API 主机固定为 `api.github.com` |
| 凭据 | `WorkBuddyAccount` 刻意不实现 `Debug`；token 不进入错误响应、日志或缓存 key；export 模式 token 只在进程内缓存，不落盘；`github_token` 只进 `Authorization` 头，不写日志 |
| 代理 | 全局 `[proxy]`，`no_proxy` 默认覆盖全部私网段 |

## 已知坑（详见 docs/PITFALLS.md）

1. **Docker 部署必须挂目录，不要挂单文件**：宿主机 `sed -i`/vim 保存会换 inode，容器内路径仍指向旧 inode，`POST /reload` 会静默读到旧配置
2. **经 Cloudflare 的 OpenAI 兼容端点上调用 xAI，必须伪装浏览器 UA**（Error 1010），grok 渠道已内置 Chrome UA
3. **SearXNG 新版默认配置 `bing` 是 disabled**；用 `use_default_settings: true` 时需显式启用
4. **Exa 对 SPA 可能只返回几百字导航文本**：fetch 必须有"内容过短继续降级"逻辑，否则 Firecrawl 永远没机会
5. **Tavily 单条正文可达 10 万字符**：必须截断后再返回
6. **WorkBuddy 经 agent2api 单次固定 5 条**；export 模式直连上游最多 10 条（服务端硬上限），且同一账号有约 10 秒锁。两者都绝不 refresh（刷新权在 agent2api）；export 模式的 `api_key` 需要 agent2api 管理权限
7. **URL 归一不能整串小写**：路径大小写敏感站点会被误合并
8. **GitHub 匿名 API 限流约 60 次/小时**：`[fetch] github_token`（或 `GITHUB_TOKEN`）可提高限额并读私有仓库；token 失败/为空时专项抽取会回退普通渠道

## 配置与部署

- `config.toml` 含真实 key，**不进 git**；`config.example.toml` 是唯一入库模板
- 配置路径用 `SEARCH_X_CONFIG` 指定；容器内建议 `/config/config.toml`
- 容器部署（挂载目录）：

```bash
docker run -d --name search-x \
  -p 127.0.0.1:3088:3088 \
  --read-only --tmpfs /tmp \
  --cap-drop=ALL --security-opt=no-new-privileges \
  -v /path/to/config-dir:/config:ro \
  -v /path/to/logs:/logs \
  --restart unless-stopped \
  ghcr.io/<owner>/search-x:latest
```

- **export 模式不需要任何 agent2api 文件挂载**
- GitHub 专项抓取 token 放 `[fetch] github_token`（600 权限的 config.toml）或容器环境变量 `GITHUB_TOKEN`；公网实例使用最小权限只读 token
- 公网部署：反代 TLS + 限流，只暴露 `/search`、`/fetch`、`/mcp`；`/reload` 仅内网

- `POST /reload` 重读配置（需认证），修改渠道/key/代理后无需重启；reload 失败只写日志，不把配置片段返回给客户端

## 测试

- 单元测试全部在 `src/*.rs` 的 `#[cfg(test)]` 模块内，无网络依赖（渠道用 `MockChannel`）
- 覆盖：URL 归一/去重/RRF、缓存命中与 `no_cache`、fetch 降级与截断、认证提取、配置默认值、SSRF
- CI 会跑 fmt + clippy + test；提交前本地跑一遍
