# 踩坑记录

按"现象 → 根因 → 解决"记录开发、部署、渠道实测中踩过的坑。部分结论只对特定上游版本成立，升级后建议复测。

## 部署

### 1. 单文件 bind mount 导致热重载静默失效

- **现象**：修改宿主机 `config.toml` 后 `POST /reload` 返回 200，但新配置没生效（日志里的代理地址还是旧的）。
- **根因**：单文件 bind mount 绑定的是 inode；`sed -i`、vim 等编辑方式实际是"写临时文件 + rename"，inode 变了，容器内 `/config.toml` 仍指向旧 inode。
- **解决**：挂载**目录**（`-v /path/to/config-dir:/config:ro` + `SEARCH_X_CONFIG=/config/config.toml`）。容器内每次 reload 重新打开路径，永远读到新文件。

### 2. 本地 Docker Desktop 未启动

- **现象**：`docker build` 报 `open //./pipe/dockerDesktopLinuxEngine: The system cannot find the file specified`。
- **解决**：先启动 Docker Desktop 并等待 server 就绪（轮询 `docker version`）。

## 渠道 / 上游

### 3. Cloudflare Error 1010：非浏览器 UA 被拒

- **现象**：经 AxonHub 转发到 xAI 的 `/responses` 请求返回 403，响应体是 Cloudflare `error 1010: browser_signature_banned`；Python/reqwest 默认 UA 必现。
- **根因**：Cloudflare 按 User-Agent 拦截非浏览器客户端。
- **解决**：grok 渠道的 HTTP 客户端固定使用 Chrome UA。其它走 Cloudflare 的渠道如出现 1010，同样处理。

### 4. SearXNG 新版默认配置与旧版相反

- **现象**：切到 `use_default_settings: true` 后，唯一稳定的 `bing` 引擎消失了。
- **根因**：新版镜像的默认 `settings.yml` 里 `bing` 默认 `disabled: true`，`google cse` 默认启用。
- **解决**：在用户配置的 `engines:` 里显式 `- name: bing / disabled: false`，同时显式禁用真正不可用的引擎。

### 5. SearXNG 的 JSON API 没有条数参数

- **现象**：想要单次 20 条，但 `format=json` 没有 `count`/`limit` 参数，`pageno=2` 返回 0 条。
- **根因**：返回数量由各引擎决定（bing 10 条/页），该实例翻页不可用。
- **解决**：接受单页数量；要更多来源就启用更多可用引擎（bing + google cse 实测约 28–30 条）。

### 6. 上游引擎的已知问题（2026-11 实测）

| 引擎 | 问题 | 处理 |
|---|---|---|
| google | 403 风控，`SearxEngineAccessDenied` 默认 suspend 86400 秒 | 禁用（经代理仍被风控） |
| duckduckgo | 每次 CAPTCHA（上游 issue #6779） | 禁用 |
| sogou | `SXNG_Response` 无 `next_request`（curl_cffi 迁移遗留，上游 #6797） | 禁用 |
| baidu | 返回非 JSON（JSONDecodeError，反爬） | 禁用 |

### 7. Exa 对 SPA 返回短内容会"抢答"

- **现象**：小黑盒只返回 322 字导航文本，`/fetch` 第一个渠道成功即停止，Firecrawl 根本没机会抓正文。
- **解决**：内容长度低于 `fetch.min_content_chars`（默认 500）时继续尝试下一渠道，最终取最长结果；达到阈值才提前停止（避免不必要的付费调用）。

### 8. Tavily 单条正文可能极长

- **现象**：Reddit 页面单条 `raw_content` 实测 19082 字符，直接返回会撑爆调用方上下文。
- **解决**：`fetch.max_content_chars`（默认 10000）按字符截断。

### 9. WorkBuddy（agent2api）的硬限制

- 单次搜索固定最多 5 条（插件内 `max_results: 5` 硬编码），多 query 才能拿更多。
- 同一账号有约 10 秒的插件锁，并发过多可能返回 `plugin_busy`；默认编排下单查询无影响。

### 10. grok 慢且有尾部风险

- `/responses` + `web_search` 单次实测约 23 秒，且历史上 multi-agent 模型有 90 秒级超时。
- **解决**：grok 默认 `fallback_only = true`，只在前面的渠道全部无结果时调用。

## 代码 / 数据

### 11. URL 归一不能整串小写

- **现象**：早期实现把整个 URL 转小写去重，路径大小写敏感站点（如 `/Wiki/Foo` vs `/wiki/foo`）会被误合并。
- **解决**：host 小写（url crate 自动）+ 路径保留原始大小写；只忽略 scheme、`www.`、末尾斜杠、fragment。

### 12. IPv6 字面量的方括号

- **现象**：`http://[::1]/` 能绕过 SSRF 防护。
- **根因**：`url::Url::host_str()` 对 IPv6 返回带方括号的 `[::1]`，直接 `parse::<IpAddr>()` 失败，进入"域名"分支被放行。
- **解决**：解析 IP 前去掉首尾方括号。

### 13. reload 错误信息不能回显配置

- **现象**：配置解析失败的 `toml` 错误可能包含整行内容（含 key）。
- **解决**：reload 失败只返回固定文案，详细错误写服务日志。

### 14. 缓存命中时 `elapsed_ms` 的语义

- 缓存直接克隆原始响应会把首次搜索的耗时带出来（看起来像"命中也要 1.8 秒"）。
- **解决**：命中时把 `elapsed_ms` 置 0，并带 `cached: true`。

### 15. 中文 JSON 在 Git Bash 下发送

- **现象**：`curl --data-binary '{"queries":["中文"]}'` 在某些终端返回 `Request body must be a JSON-RPC 2.0 message`。
- **根因**：终端编码（GBK/cp936）把中文转成了非 UTF-8 字节。
- **解决**：把请求体写成 UTF-8 文件，再用 `--data-binary @file`；测试脚本统一用 Python 发请求。

### 16. SSRF 只查字面量不够：`nip.io` 这类域名要解析后再判

- **现象**：`http://127.0.0.1.nip.io/` 的主机名不是 IP 字面量，纯字符串检查会放行。
- **解决**：`url_guard::is_public_http_url_resolved` 在字面量检查后再做一次 DNS 解析，**所有**解析结果必须是公网 IP；解析失败一律拒绝。REST `/fetch` 与 MCP `web_fetch` 统一走这个函数。

### 17. WorkBuddy direct 模式：账号库挂载要收窄、WAL 要一起挂

- **背景**：direct 模式只读旁路读取 agent2api 的 `agent2api.db` 与 `account.key` 解密 token。
- **坑 1**：直接挂载整个 agent2api 数据目录会把 `update-token.key`、插件和 wasm 缓存一起暴露给 search-x。应建专用目录，只放 `agent2api.db`、`agent2api.db-wal`、`agent2api.db-shm`、`account.key` 四个文件的硬链接。
- **坑 2**：SQLite 处于 WAL 模式，只挂主库会读不到最新事务（token 刷新写回后可能看不到）。必须把 `-wal`/`-shm` 一起挂。
- **坑 3**：硬链接在 `-wal`/`-shm` 被重建（agent2api 重启）后会指向旧 inode；表现是读到的账号数据变旧。重新执行链接命令即可，search-x 侧有 TTL + 401 重读 + agent2api 回退兜底。
- **坑 4**：容器根文件系统设为 `--read-only` 后，SQLite 只读连接仍能工作（挂载目录本身是 ro），日志目录需单独 rw 挂载。
