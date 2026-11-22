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
workbuddy_accounts.rs  WorkBuddy direct 模式：只读旁路解密 agent2api 账号库（不刷新 token）
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
cargo test                   # 单元测试（18 个，无网络依赖）
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
| SearXNG | 结果只有摘要，统一排最后；JSON API 无条数参数 |
| 缓存 | 进程内 TTL + 容量；key = query + 模式/渠道 + limit；`no_cache: true` 绕过读取但仍写回；命中时清空 `channel_elapsed_ms` |
| 日志 | JSONL；`channel_elapsed_ms` 记录每渠道耗时（含失败渠道）；`cached: true` 命中写轻量行（results 为空），可统计命中率 |
| 抓取 | 多 URL 并行；单 URL 按渠道顺序降级；内容 < `min_content_chars` 时继续尝试下一渠道取最长 |
| 认证 | 默认开启；无 key 时全部拒绝；支持 bearer / x-api-key / both / custom header |
| 安全 | SSRF 白名单（拒绝私网/回环/链路本地/CGNAT/内网域名 + DNS 解析校验）、1MB body 上限、查询 ≤10 条且 ≤4000 字符（REST 与 MCP 一致）、URL ≤10 个；`/health` 不返回版本；direct 模式错误对客户端脱敏 |
| 凭据 | `WorkBuddyAccount` 刻意不实现 `Debug`；token 不进入错误响应、日志或缓存 key；direct 只读挂载 4 个文件（db/wal/shm/account.key） |
| 代理 | 全局 `[proxy]`，`no_proxy` 默认覆盖全部私网段 |

## 已知坑（详见 docs/PITFALLS.md）

1. **Docker 部署必须挂目录，不要挂单文件**：宿主机 `sed -i`/vim 保存会换 inode，容器内路径仍指向旧 inode，`POST /reload` 会静默读到旧配置
2. **经 Cloudflare 的 OpenAI 兼容端点上调用 xAI，必须伪装浏览器 UA**（Error 1010），grok 渠道已内置 Chrome UA
3. **SearXNG 新版默认配置 `bing` 是 disabled**；用 `use_default_settings: true` 时需显式启用
4. **Exa 对 SPA 可能只返回几百字导航文本**：fetch 必须有"内容过短继续降级"逻辑，否则 Firecrawl 永远没机会
5. **Tavily 单条正文可达 10 万字符**：必须截断后再返回
6. **WorkBuddy 经 agent2api 单次固定 5 条**；direct 模式直连上游最多 10 条（服务端硬上限），且同一账号有约 10 秒锁。direct 只读账号库、绝不 refresh（刷新权在 agent2api）
7. **URL 归一不能整串小写**：路径大小写敏感站点会被误合并

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

- direct 模式额外挂载**专用目录**（只含 `agent2api.db`、`agent2api.db-wal`、`agent2api.db-shm`、`account.key` 四个文件的硬链接），不要挂载整个 agent2api 数据目录
- 公网部署：反代 TLS + 限流，只暴露 `/search`、`/fetch`、`/mcp`；`/reload` 仅内网

- `POST /reload` 重读配置（需认证），修改渠道/key/代理后无需重启；reload 失败只写日志，不把配置片段返回给客户端

## 测试

- 单元测试全部在 `src/*.rs` 的 `#[cfg(test)]` 模块内，无网络依赖（渠道用 `MockChannel`）
- 覆盖：URL 归一/去重/RRF、缓存命中与 `no_cache`、fetch 降级与截断、认证提取、配置默认值、SSRF
- CI 会跑 fmt + clippy + test；提交前本地跑一遍
