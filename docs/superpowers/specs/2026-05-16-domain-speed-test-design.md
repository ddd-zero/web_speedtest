# 多域名测速网站设计

## 背景与目标

当前项目已有一个单页网络测速页面，包含暖色卡片式视觉、用户网络信息、下载测速曲线、进度条和测速记录弹窗。新网站需要复用这套交互和视觉基调，但核心目标从“单线路测速”调整为“帮助用户比较多个域名哪一个更快”。

首批测速域名为：

- `https://a1.steinsgate.eu.org`
- `https://a2.steinsgate.eu.org`

用户进入页面后，页面应自动并发测试每个域名的 HTTPS 延迟，多测几次并给出推荐线路。用户点击某个域名后，再对该域名执行单线程下载测速。测速记录使用 SQLite 保存，支持历史查询。

## 范围

本次实现包含：

- Rust 后端服务，负责静态页面、SQLite 初始化、测速记录保存、更新和查询。
- 前端单页应用，复用现有 `index.html` 的卡片、按钮、曲线、进度和弹窗风格。
- 多域名 HTTPS 延迟自动测试。
- 单域名单线程下载测速。
- 测速进行到中段先保存一次记录，测速结束后更新同一条记录。
- 历史记录查询，并对返回 IP 做隐私脱敏。
- IP 查询信息先预留字段，API 未确定前显示“待接入”。

本次不包含：

- 真实 IP 归属地或运营商 API 接入。
- 用户登录、权限系统或复杂后台管理。
- 后端代理下载测速。浏览器端测速才代表用户到域名的速度。

## 方案取舍

推荐方案是 Rust 后端 + SQLite + 浏览器端测速。

Rust 后端负责稳定保存和查询数据，SQLite 满足轻量部署需求。前端直接请求用户要测试的域名，下载速度来自用户浏览器到目标域名的真实路径。保存逻辑由后端校验，避免前端随意覆盖记录。

备选一是纯静态页面加 `localStorage`。实现更快，但无法做多用户历史查询，也不满足 SQLite 要求。

备选二是后端代理测速文件。这样可以规避跨域限制，但测到的是服务器到目标域名的速度，不是用户到目标域名的速度，因此不采用。

## 前端信息架构

页面采用“域名对比列表优先”的结构：

1. 顶部展示页面标题和 IP 查询占位信息。
2. 域名列表展示每个域名的 HTTPS 延迟、状态、推荐标记和测速按钮。
3. 用户点击某个域名后，下方显示当前测速速度、曲线和进度。
4. 历史记录通过弹窗查询，延续现有表格风格。

每个域名配置包含：

- 展示名称，例如 `a1`、`a2`。
- 域名，例如 `a1.steinsgate.eu.org`。
- 延迟测试 URL，例如 `https://a1.steinsgate.eu.org/cdn-cgi/trace`。
- 下载测试 URL，例如 `https://a1.steinsgate.eu.org/200mb.test`。

## HTTPS 延迟测试

网页无法直接获取真实 TCP 握手耗时，也不能发起裸 TCP ping。因此延迟指标定义为“浏览器可测的 HTTPS 请求耗时”。

页面加载后，对每个域名并发执行多轮 `GET /cdn-cgi/trace`。每次请求追加随机参数并设置 `cache: "no-store"`，降低缓存对结果的影响。

统计方式：

- 每个域名默认测试 5 轮。
- 记录每轮耗时。
- 使用中位数作为主要排序指标。
- 同时保留最小值、最大值和样本列表，便于后续分析。

第一轮结果可能包含 DNS、TCP、TLS、HTTP 请求等冷启动成本；后续结果更接近稳定 HTTPS 请求往返耗时。页面文案只称为“HTTPS 延迟”，避免误导成真实 TCP 延迟。

## 下载测速

用户点击某个域名后，仅对该域名执行单线程下载测速：

- `a1` 使用 `https://a1.steinsgate.eu.org/200mb.test`
- `a2` 使用 `https://a2.steinsgate.eu.org/200mb.test`

前端使用 Fetch 读取 `ReadableStream`，按时间窗口计算当前 Mbps，并绘制速度曲线。测速以固定时长和文件读取完成两者中的先到者结束，结束时取消未完成的下载流。

测速文件必须允许跨域读取，例如返回 `Access-Control-Allow-Origin: *`。否则浏览器无法读取响应流，页面会显示跨域错误提示。`/cdn-cgi/trace` 已验证支持跨域读取；`200mb.test` 也需要配置同样的跨域头。

## 保存策略

保存策略沿用原页面逻辑，但后端增加更新令牌：

1. 测速进行到约 50% 或超过半段时间时，前端调用保存接口插入一条记录。
2. 后端返回 `id` 和 `update_token`。
3. 测速完成后，前端携带 `id` 和 `update_token` 更新同一条记录。
4. 如果中段保存失败，完成时创建一条最终记录。
5. 如果完成更新失败，页面提示保存失败，但测速结果仍显示给用户。

`update_token` 用于避免其他客户端猜测记录 ID 后覆盖数据。

## 后端接口

### `GET /`

返回测速页面。

### `GET /api/config`

返回域名配置，便于后续新增域名时不必改前端逻辑。

### `POST /api/results`

创建或更新测速记录。

创建请求包含：

- 域名标识
- HTTPS 延迟统计
- 中段下载速度
- 状态，例如 `running`
- 用户 IP 信息占位字段

更新请求额外包含：

- `id`
- `update_token`
- 最终下载速度
- 状态，例如 `completed` 或 `failed`

用户 IP 由后端从请求连接信息或转发头中解析，前端不能伪造最终入库 IP。部署在反向代理后时，需要配置可信代理头来源，避免把任意客户端传入的 `X-Forwarded-For` 当真。

### `GET /api/results`

查询历史记录。

支持参数：

- `domain`：按域名筛选。
- `status`：按状态筛选。
- `q`：模糊搜索域名、位置、运营商等预留字段。
- `limit`：限制返回数量，后端设置上限。

响应只返回脱敏 IP。

## SQLite 数据模型

表名：`speed_results`

核心字段：

- `id INTEGER PRIMARY KEY AUTOINCREMENT`
- `update_token TEXT NOT NULL`
- `domain_key TEXT NOT NULL`
- `domain_host TEXT NOT NULL`
- `trace_url TEXT NOT NULL`
- `download_url TEXT NOT NULL`
- `https_latency_median_ms REAL`
- `https_latency_min_ms REAL`
- `https_latency_max_ms REAL`
- `https_latency_samples_json TEXT`
- `partial_download_mbps REAL`
- `final_download_mbps REAL`
- `status TEXT NOT NULL`
- `client_ip TEXT NOT NULL`
- `location TEXT`
- `isp TEXT`
- `colo TEXT`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`

后续如果接入 IP API，只需要填充 `location`、`isp`、`colo` 等字段，不影响测速主流程。

## IP 隐私

数据库保存完整用户 IP，用于后续排查和统计。历史查询接口返回时必须脱敏：

- IPv4 保留 `/24`，例如 `1.2.3.4` 返回 `1.2.3.*`。
- IPv6 保留 `/48`，例如 `2001:db8:abcd:1234::1` 返回 `2001:db8:abcd::*`。
- 无法解析的 IP 返回空字符串或 `未知`，不原样暴露。

IP 脱敏逻辑必须有单元测试覆盖 IPv4、IPv6、IPv4-mapped IPv6、空值和非法输入。

## 错误处理

前端需要区分：

- 延迟测试失败：单个域名显示不可用，不影响其他域名。
- 下载跨域失败：提示需要为测速文件配置跨域头。
- 下载中断：保留已测速度，状态保存为 `failed`。
- 保存失败：测速结果仍显示，但提示记录未保存。
- 历史查询失败：弹窗显示加载失败。

后端需要：

- 校验请求体大小和字段范围。
- 限制查询 `limit` 上限。
- 使用参数化 SQL，避免 SQL 注入。
- 对数据库错误返回统一 JSON 错误。

## 测试策略

后端测试：

- IP 脱敏函数。
- 域名配置校验。
- SQLite 创建记录和更新记录。
- `update_token` 不匹配时拒绝更新。
- 查询接口只返回脱敏 IP。

前端验证：

- 页面加载后自动显示两个域名并开始多轮 HTTPS 延迟测试。
- 延迟测试完成后能标记推荐域名。
- 点击域名后触发下载测速、曲线更新和中段保存。
- 历史记录弹窗可查询并显示脱敏 IP。

最小充分验证：

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- 浏览器手动验证页面主要流程。

## 风险与技术债

- 浏览器 HTTPS 延迟不等于真实 TCP 延迟，只能作为网页环境下的近似指标。
- `200mb.test` 必须支持 CORS，否则前端无法读取下载流。
- 反向代理部署时必须明确可信 IP 头策略，否则用户 IP 可能被伪造。
- IP 查询 API 未确定，当前仅保留字段和 UI 占位。
- 域名数量增加后，延迟并发轮数需要避免过高，防止页面加载时产生过多请求。
