# 04 · MCP Server（Python）

> 对应源码：`mcp/server.py`（约 2 173 行，纯标准库，无第三方依赖）、`mcp/README.md`
> **定位**：把网络管理的能力暴露给 AI 客户端（Claude Desktop / Codex / WorkBuddy 等）。它是与 Tauri GUI、CLI bin 并列的**第三个入口**，与 Rust 后端共享同一份 `config.json` 和同一套 mihomo 内核运行时。
> **红线（CONTRACT.md）**：MCP 与 Rust 是同一软件的两个入口，**同一功能的行为（含副作用）必须一致**。为此有专门的 `scripts/check_parity.py` 做规则引擎逐行 diff（见 [05-scripts-and-tooling.md](./05-scripts-and-tooling.md)）。

---

## 1. 两种接入方式

`main()` 根据命令行参数分派：

| 启动方式 | 命令 | 传输 | 用途 |
|---|---|---|---|
| **stdio**（默认） | `python3 mcp/server.py` | 行分隔 JSON-RPC over stdin/stdout | Claude Desktop / Codex 等原生 MCP 客户端 |
| **HTTP 桥接** | `python3 mcp/server.py --http [port]`（默认 19092） | HTTP POST → JSON-RPC | WorkBuddy 等只能走 HTTP 的客户端 |

```
stdio 主循环（main）
  for line in sys.stdin:
      msg = json.loads(line)          # 解析失败的行静默跳过
      resp = handle_message(msg)
      if resp is not None:            # 通知类消息返回 None，不响应
          print(json.dumps(resp, ensure_ascii=False), flush=True)
```

---

## 2. 协议层

### 2.1 JSON-RPC 方法（`handle_message`）

| method | 处理 | 返回 |
|---|---|---|
| `initialize` | 返回协议版本与能力 | `protocolVersion: "2025-06-18"`、`capabilities: {tools:{}, instructions:{}}`、**`instructions: SERVER_INSTRUCTIONS`**、`serverInfo: {name:"magic-agent", version:"0.1.0"}` |
| `tools/list` | 返回整个 `TOOLS` 数组 | `{tools: TOOLS}` |
| `tools/call` | 取 `params.name` / `params.arguments`，调 `call_tool`，把结果包成 text content | `{content:[{type:"text", text: <JSON字符串>}]}` |
| `notifications/initialized` | 无需响应 | `None`（主循环不打印） |
| 其他 | — | `error: -32601 method not found` |

**健壮性兜底**：`tools/call` 里 `arguments` 可能是 `null`（客户端 bug），代码强制 `if not isinstance(tool_args, dict): tool_args = {}`，避免 `args.get` 崩溃。`call_tool` 内部异常也被 try/except 包成 `{'error': str(e)}` 返回，**永不因单个工具报错而断开连接**。

### 2.2 `SERVER_INSTRUCTIONS`（给 LLM 的"整机用户手册"）

MCP 协议 2025-06-18 引入 `instructions` 能力：客户端会把它注入系统提示词，让智能体"一连上就天然知道"网络管理是什么、何时用、两条路怎么选。这是"让智能体自主决策使用网络管理"的核心机制，写作遵循四条纪律：①讲何时该用/不该用；②讲两条路怎么选；③讲与其他工具的配合；④简洁功能导向、不要营销话术。

要点内容：
- **两条物理分开的路**：节点代理 `127.0.0.1:7893`（国外）、本机直连 `127.0.0.1:7892`（国内）。
- **核心原则**：网络管理**不替 AI 自动判断**走哪条路，决策权在 AI。
- **何时不该用**：访问 `127.0.0.1/localhost` 回环服务时不要套代理；访问 WorkBuddy 自身中转站/模型接口时不要套代理（超长流式 JSON 经代理会损坏请求体）。

---

## 3. 工具清单（**28 个**）

> `TOOLS` 数组实际 **28** 个（与 `_TOOL_SCHEMAS` 一一对应）。历史上 README 长期误写"23 个"，2026-09-23 修正为 27；同日 P1-1 新增 `audit_network` 后为 28。

| # | 工具 | 参数（required 加粗） | 说明 |
|---|---|---|---|
| 1 | `status` | — | 运行状态、选中节点、**真实**系统代理状态（读 `scutil`，非 config 意图）、节点数 |
| 2 | `start_proxy` | — | 启动 TUN 代理（需管理员授权）；内核已在运行时按 `config.systemProxy` 对齐系统代理 |
| 3 | `stop_proxy` | — | 停内核**并强制关系统代理**（否则系统代理指向已关的 7891，用户断网） |
| 4 | `list_nodes` | — | 节点列表（含 `current` 标记） |
| 5 | `switch_node` | **name** | 切节点：写 config + mihomo `PUT /proxies/PROXY` 秒切，不重启不弹框 |
| 6 | `list_apps` | — | 软件分流配置（id/mode/confirmed） |
| 7 | `set_app_mode` | **id**、**mode**（enum proxy/direct）、node | 设某 App 走代理/直连，置 `confirmed=true`，随后热重载规则 |
| 8 | `check_network` | — | 百度直连 + Google 经节点延迟探测 |
| 9 | `check_update` | — | 只读检查新版本（按 `updateChannel`），安装需用户在设置页操作 |
| 10 | `list_domain_rules` | — | 域名分流规则列表 |
| 11 | `add_domain_rule` | **domain**、**target**、reason | target 支持 proxy/direct/**节点名** |
| 12 | `remove_domain_rule` | **domain** | 删域名规则 |
| 13 | `fetch_subscription` | **url** | 拉 VLESS 订阅 |
| 14 | `test_node_delay` | **name** | 单节点延迟 |
| 15 | `list_free_models` | vendor | 读 `docs/free_models.json`，按厂商分组 |
| 16 | `list_connections` | limit | 实时连接快照（主机/规则/出口/进程/上下行，按下载排序） |
| 17 | `node_health` | — | 全部节点延迟 + fallback 组当前实际使用节点 |
| 18 | `download_proxy` | url | 返回两条路入口地址（+可选建议）**让 AI 自己决定走哪条** |
| 19 | `doctor` | — | 一键自检（见 §5） |
| 20 | `install_privileged_helper` | — | 一次性装特权控制器（root 控制脚本 + sudoers 白名单） |
| 21 | `probe_route` | **url**、timeout、read_bytes | 双路实测：7892 vs 7893 的延迟 + 吞吐对比 + 结论 |
| 22 | `server_metrics` | — | 远程采集激活服务器 CPU/内存/磁盘/带宽/负载/在线时长 |
| 23 | `list_servers` | — | 列云服务器，标出激活的一台 |
| 24 | `select_server` | **id** | 切换激活服务器（`server_metrics`/`ssh_exec` 作用目标随之变） |
| 25 | `set_system_proxy` | **enabled** | 开关 macOS 系统代理（指向 7891） |
| 26 | `ssh_exec` | **command**、timeout_secs | 在激活服务器非交互执行命令，返回 (stdout, stderr, exit_code) |
| 27 | `guide` | — | 返回完整使用手册 |
| 28 | `audit_network` | — | **网络体检（P1-1）**：只读采集第三方代理进程/端口冲突/死端口残留/路由/DNS/PAC/环境变量代理，返回 summary + degraded；与 Rust `auditor.rs` 同口径，零副作用 |

### 3.1 参数加严（`call_tool` 头部）

工具调用**多传未知参数会直接报错**，不静默吞掉。教训：曾误传 `policy='direct'`（正确是 `target`），旧逻辑静默忽略并存成默认值 proxy，**规则方向整个反了还不报错**。现在实现：拿 `_TOOL_SCHEMAS[name]` 的 `properties` 键集合，`args` 里多出的键直接返回 `{'error': '未知参数: [...]'}`。

---

## 4. 关键实现模块

### 4.1 配置读写与键名兼容

- `CONFIG_PATH = ~/Library/Application Support/magic-agent/config.json`（与 Rust `config.rs` 同一文件）。
- `_snake_to_camel` / `_normalize_keys`：把配置里的 snake_case 键规范成 camelCase，兼容旧版本写入的键名。
- `read_config` / `write_config`：读写 JSON；损坏时（key 不存在）返回 `{'error': ...}` 供上层透出。
- **状态真实性**：`status` 的 `systemProxy` 字段读 `scutil --proxy` 实际值，**不读 config 意图**——否则 App 重启后 config 仍 true 但代理实际关了，会误导 AI。

### 4.2 内核管理（与 Rust 端逻辑一致）

- `MIHOMO_BIN`：优先 runtime 常驻副本 `~/Library/Application Support/magic-agent/runtime/bin/mihomo`（App 升级替换 .app 不影响运行中的内核），否则 `_project_resource_bin()` 依次尝试环境变量 `MAGIC_AGENT_RESOURCES` → 项目 `src-tauri/resources/bin/mihomo` → 打包 `.app/Contents/Resources/bin/mihomo`。
- `MIHOMO_PGREP_PATTERN = 'magic-agent/runtime/bin/mihomo'`：**精确匹配本 App 的常驻副本**。绝不能用宽泛的 `resources/bin/mihomo`——FlClash/Clash Verge 等第三方内核也放在各自 `.app/Contents/Resources/` 下，用宽泛关键词会误杀。
- `generate_config(cfg)`：Python 侧的规则生成逻辑（对应 Rust `mihomo.rs::build_conf`）——这是 parity 校验的对象。listeners 段与 Rust 侧同步钉死 `listen: 127.0.0.1`（2026-09-23 安全修复；重构计划 P2-1 将把本函数退役为"调 App API"的瘦客户端，消灭双引擎漂移面）。
- `hot_reload_rules` / `_rule_reload_result`：走 mihomo PATCH `/configs` 热更新，不弹框。
- `ensure_runtime_bin` / `start_mihomo` / `stop_mihomo` / `mihomo_running`。
- 特权控制器：`CTL_PATH = /usr/local/lib/magic-agent/mihomo-ctl.sh`，`install_privileged_helper()` 安装 root 控制脚本 + sudoers 白名单（`SUDOERS_LINE`），装后启停/重载零弹窗。与 `scripts/mihomo-ctl.sh` 对应。

### 4.3 规则与安全

- `_yaml_quote` / `_sanitize_rule_field` / `_sanitize_node_name`：YAML 注入防护与字段净化（与 Rust 端同名函数对应）。
- `_is_private_or_reserved_ip` / `validate_public_host_py`：**服务器端请求伪造（SSRF）防护**——订阅/下载类工具拒绝解析到私网/保留地址的主机。
- `PROTECTED_DIRECT_DOMAINS = ['203.0.113.74']`：受保护的直连域名清单。
- `app_paths_for(app_id)`：把 App id 映射到可执行路径，供进程规则使用。

### 4.4 两条路的实测（`download_proxy` / `probe_route`）

- `download_proxy(args)`：返回 7892/7893 两条路入口；带 `url` 时附国内/国外建议（`_is_cn_ip` + `CN_DOMAIN_SUFFIXES` + `_http_get_via` 实测）。**只给建议，最终由 AI 拍板**。
- `probe_route(args)`：对同一 url 分别经 7892 与 7893 实测延迟 + 下载吞吐（读 `read_bytes` 字节），返回对比数据与明确结论。设计意图是"不再凭国内/国外规则猜，而是实测路况后拍板"。

### 4.5 SSH 与云服务器

- Keychain 服务名 `KEYCHAIN_SERVICE = 'com.magic.agent'`；`_keychain_password` / `_keychain_key` 读凭据（密码**不落盘**）。
- `_active_server()`：取"当前激活"服务器（`list_servers`/`select_server` 切换的目标）。
- `ssh_exec(command, timeout_secs)`：
  - 密钥认证：显式 `-i` 指定密钥文件（不依赖 `~/.ssh/config` 别名匹配）；Keychain 里的密钥内容写临时文件（0600）用后删除。
  - 密码认证：用 `/usr/bin/expect` 喂密码（密码不进 argv、不落盘）；**spawn 的每个参数都做 shell 引号包裹**，否则命令里的 `;`/`|`/空格会被 expect 错误拆分。
  - ssh 选项：`StrictHostKeyChecking=accept-new`、`ConnectTimeout=10`、`BatchMode=no`。
- `server_metrics()`：拼一条探针命令（CPU/MEM/DISK/LOAD/NET），经 `ssh_exec` 执行，`_parse_server_metrics` + `_extract_pct` 解析成结构化 JSON。

### 4.6 自检（`doctor`）

返回各检查项 OK/FAIL，是 AI 排查问题的第一入口：

| 检查项 | 判据 |
|---|---|
| `config` | 节点数 / 选中节点 / 域名规则数 / apiSecret 是否存在 |
| `process` | `mihomo_running()` |
| `auth` | 未运行时 `SKIP`；否则 `GET /version` 带鉴权是否成功 |
| `failover_group` | 读 `runtime/mihomo.yaml`，必须含 `- name: PROXY\n    type: fallback`——**防回退成 select 丢掉故障转移** |
| `secret_line` | YAML 里必须有 `secret:` 行 |
| `funnel_order` | `GEOSITE,cn,DIRECT` 必须出现在 `MATCH,DIRECT` 兜底之前 |
| `nodes_health` | `node_health().nodes` |

### 4.7 更新检查（`check_update`）

- 端点：`local` → `http://127.0.0.1:7878/latest.json`（直连）；`github` → `https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json`（**必须经节点代理端口 7893**，GitHub 直连不通）。
- 当前版本读 `src-tauri/tauri.conf.json` 的 `version`。
- 版本比较 `_v()` 把 `x.y.z` 拆成元组比较；返回 `available = latest > current`。
- **只读**：不下载、不安装；安装交由用户在 App 设置页操作。

---

## 5. HTTP 桥接的鉴权与 CSRF 防护

`create_http_server(port=19092)` / `serve_http` 用标准库 `ThreadingHTTPServer`：

- **令牌**：`load_http_bridge_token()` 读取或生成 `~/Library/Application Support/magic-agent/http-bridge.token`（**0600**）。首次启动自动生成并持久化，避免任意网页通过 localhost CSRF 直接调用代理控制工具。
- **来源白名单**（`_origin_allowed`）：带 `Origin` 头的请求，Origin 必须是 `http://127.0.0.1:{port}` / `http://localhost:{port}` / `http://[::1]:{port}` 之一。
- **双层校验**（`_request_authorized`）：
  - **无 Origin** = 本机原生客户端（launchd/脚本/CLI）→ 放行。
  - **有 Origin** = 浏览器请求 → 必须同时通过来源白名单 **且** `Authorization: Bearer <token>`。
- **路由**：
  - `GET /`、`/health`、`/extension` → `{ok:true, server:"magic-agent"}`。
  - `POST /extension`、`/rpc` → 解析 JSON 调 `handle_message`；响应 `None`（通知）返回 202，否则 200 + JSON。
  - `OPTIONS` → 204（CORS 预检）。
  - 其余 → 404。
- `server_version = 'magic-agent-extension/0.1'`（历史命名遗留，非 `mcp`）。
- 绑定失败（端口占用）时打印 stderr 并 `sys.exit(1)`。

---

## 6. 与 Rust 端的一致性约束（改代码必读）

| 约束 | 原因 |
|---|---|
| **规则生成必须与 Rust 逐行一致** | 历史漂移过一次（域名/进程规则顺序颠倒）。改 `generate_config` 后必须跑 `scripts/check_parity.py` |
| **启动/停止的副作用一致** | Rust 与 Python 都遵守：`start` 按 `config.systemProxy` 决定是否开系统代理；`stop` 必须关系统代理 |
| **状态查询报真实状态** | MCP `status` 与 Rust `get_status` 都读 `scutil`，不读 config 意图 |
| **进程匹配用精确路径** | 两侧都只匹配 `magic-agent/runtime/bin/mihomo`，避免误杀第三方内核 |
| **config 键名 camelCase** | 两侧都有 snake→camel 规范化，防止旧数据 break |
| **密码/密钥不进 config 明文** | 只进 Keychain（MCP README 亦强调） |

---

## 7. 调试

```bash
# 查看工具列表
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | python3 mcp/server.py

# 调单个工具（status）
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{}}}' | python3 mcp/server.py

# 启动 HTTP 桥接
python3 mcp/server.py --http 19092
curl -s http://127.0.0.1:19092/health
```

---

← [03-frontend-ui.md](./03-frontend-ui.md)　|　返回 [README](./README.md)　|　下一篇 → [05-scripts-and-tooling.md](./05-scripts-and-tooling.md)
