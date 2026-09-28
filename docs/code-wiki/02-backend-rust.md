# 02 · Rust 后端模块详解

> 位置：`src-tauri/src/`。crate 名 `magic_agent_lib`（`lib`) + 三个 bin（`magic-agent`、`magic_probe`、`dump_conf`）。

## 模块依赖图

```
                    main.rs
                       │
                       ▼
┌──────────────────────────────────────────────────────────────┐
│                        lib.rs（组合根）                       │
│  AppStatus / ConflictInfo / AppState / 全部 #[tauri::command] │
└───┬────────┬────────┬────────┬─────────┬──────────┬──────────┘
    │        │        │        │         │          │
    ▼        ▼        ▼        ▼         ▼          ▼
 mihomo   config    apps     ssh   system_proxy  keychain
    │        │                 │                    ▲
    │        │                 └────────────────────┘
    │        └──►（AppConfig 被 mihomo / lib 消费）
    │
    └──► 生成 mihomo.yaml、调 mihomo-ctl.sh / osascript

  updater.rs ──► config::save + AppState + tauri_plugin_updater
  bin/dump_conf.rs ──► config::AppConfig + lib::settings_to_app_rules + mihomo::MihomoManager
  bin/magic_probe.rs ──► lib::start_proxy_standalone / stop_proxy_standalone
```

可见性：`config` 与 `mihomo` 是 `pub mod`（给 bin 工具用）；`apps` / `ssh` / `system_proxy` / `updater` / `keychain` 是私有 `mod`。

---

## 2.1 `lib.rs`（1516 行）—— 组合根

### 职责
Tauri command 注册表、应用状态管理、代理启停编排、SSH 命令转发、退出收尾钩子、看门狗。

### 数据结构

| 类型 | 字段 | 说明 |
|---|---|---|
| `AppStatus` | `proxy_running: bool` / `proxy_pid: Option<u32>` / `proxy_port: u16` / `system_proxy: bool` / `apps_count: usize` / `nodes_count: usize` / `ssh: Option<SshSession>` | 前端轮询的状态快照（camelCase 序列化） |
| `ConflictInfo` | `has_conflict: bool` / `messages: Vec<String>` | 端口/第三方代理冲突检测结果 |
| `AppState`（`struct`，非 pub） | `config: AppConfig` / `mihomo: MihomoManager` / `apps_cache: Mutex<Vec<AppEntry>>` / `should_run: Arc<AtomicBool>` | 由 `Arc<Mutex<AppState>>` 托管 |

### 常量

| 常量 | 内容 |
|---|---|
| `FOREIGN_PROXY_APPS: &[(&str, &str)]` | 24 组「进程名关键字 → 显示名」，如 `("flclash", "FlClash")`、`("clash verge", "Clash Verge")`、`("surge", "Surge")` |
| `FOREIGN_PROXY_CORES: &[&str]` | 12 个第三方**内核**可执行名，如 `flclashcore`、`verge-mihomo`、`clash-meta`、`sing-box`、`xray-core`。注释强调：**绝不放入宽泛的 `mihomo`**（会误杀自己的内核） |

### 函数清单

#### 第三方代理清理

| 函数 | 签名 | 作用 |
|---|---|---|
| `find_foreign_proxies` | `fn() -> Vec<(u32, String)>` | 扫 `ps -axo pid=,args=`，返回 (pid, 描述)。跳过自身 PID、自身可执行文件（`magic-agent`/`magic_probe`/`dump_conf`）、runtime 目录下的内核。**只取首个空白 token（可执行文件路径）做匹配**，绝不匹配整条命令行 |
| `cleanup_foreign_proxies` | `fn() -> Vec<String>` | 两轮清理：SIGTERM → sleep 1.5s → 仍存活者 SIGKILL，最后关系统代理。返回被清理项描述 |
| `check_conflicts` / `check_conflicts_blocking` | `async fn() -> ConflictInfo` | 检测第三方代理进程 + 混合端口 7891 是否被非自身程序占用 |
| `kill_foreign_proxies` | `async fn() -> Vec<String>` | 前端「立即清理」按钮（`spawn_blocking` 包装） |
| `list_foreign_proxies` | `async fn() -> Vec<String>` | 只查询不清理 |

#### 状态与配置

| 函数 | 签名 | 关键点 |
|---|---|---|
| `get_status` | `async fn(State<Arc<Mutex<AppState>>>, State<SshManager>) -> Result<AppStatus, String>` | 锁内只取轻量数据（`apps_cache.len()`、`config.nodes.len()`），探测挪进阻塞线程池。`SshManager` 用 `ssh.inner().clone()` |
| `get_config` | `fn(State<...>) -> AppConfig` | **把 `api_secret` 置 `None`** 再下发前端 |
| `save_config` | `async fn(State<...>, AppConfig) -> Result<AppConfig, String>` | 保留后端原 secret（`None` 或空串都视为未提供）；`sshPassword` 置 `None`、多行私钥内容置 `None`；若代理在跑则热更新规则；返回的 config 同样清空 secret |

#### mihomo 控制 API 代理

| 函数 | 签名 | 作用 |
|---|---|---|
| `proxy_api` | `async fn(State<...>, path: String, method: Option<String>, body: Option<String>) -> Result<(u16, String), String>` | 前端访问 mihomo API 的唯一通道，secret 由后端注入 |
| `proxy_api_blocking` | `fn(secret, path, method, body) -> Result<(u16, String), String>` | 路径校验（须 `/` 开头、问号前不含 `://`、不含 `\r\n`）；method 只允许 GET/PUT；`TcpStream` 直连 `127.0.0.1:19091`，read timeout 15s |
| `parse_http_response` | `fn(&[u8]) -> (u16, String)` | **按字节解析**，支持 `Content-Length` 与 `Transfer-Encoding: chunked`（mihomo `/connections` 大响应必走 chunked）。仅在最后一步 `from_utf8_lossy`，避免 chunk 长度切片错位 |
| `find_subslice` | `fn(haystack, needle) -> Option<usize>` | 字节子切片查找 |

#### 软件分流规则

| 函数 | 签名 | 作用 |
|---|---|---|
| `effective_app_rules` | `fn(&AppConfig) -> Vec<(Vec<String>, String)>` | 便捷入口（无缓存，可能触发全盘扫描） |
| `effective_app_rules_with` | `fn(&AppConfig, Option<&[AppEntry]>) -> Vec<(Vec<String>, String)>` | 带缓存版本；`Some` 且非空时直接用，否则真扫描 |
| `settings_to_app_rules` | `pub fn(settings, path_lookup, valid_nodes) -> Vec<(Vec<String>, String)>` | **纯函数**，GUI 与 `dump_conf` 共用。跳过未确认项；`bin-` 前缀直接用 id 后半段；`proxy` 模式且节点存在 → `NODE-<sanitize>`，否则降级 `PROXY` |

#### 代理启停

| 函数 | 签名 | 关键点 |
|---|---|---|
| `start_proxy` | `async fn(State<...>) -> Result<MihomoStatus, String>` | 已在跑则直接返回；否则 `spawn_blocking`：`cleanup_foreign_proxies()` → 空节点提前拦截 → 复用 apps_cache 生成规则 → `mihomo.start()` → 按 config 设系统代理。成功后置 `should_run = true` |
| `stop_proxy` | `async fn(State<...>) -> Result<(), String>` | 先置 `should_run = false`，阻塞线程池里 `mihomo.stop()` + 关系统代理，最后清 PID |
| `set_system_proxy` | `async fn(State<...>, enabled: bool) -> Result<SystemProxyStatus, String>` | 调 `networksetup` 并**同步更新 + 持久化 `config.system_proxy`**（旧实现不改 config，导致重启后状态脱节） |
| `self_test` | `fn(State<...>) -> Result<String, String>` | 报告内核与 geo 目录路径/是否存在 |
| `start_proxy_standalone` / `stop_proxy_standalone` | `pub fn` | 供 `magic_probe` 用；stop 时先试 `ctl("stop")`，否则按 `runtime/mihomo.yaml` 路径 `ps` 匹配后 `osascript` 提权 kill |

#### SSH 命令

| 函数 | 签名 | 关键点 |
|---|---|---|
| `ssh_connect` | `async fn(State<AppState>, State<SshManager>, host, port, user, auth, password, key) -> Result<SshSession, String>` | 先 `spawn_blocking` 建连并通过认证；**Keychain 写入是副作用，失败只标 `password_saved=false`，不让整个命令返 Err**（否则用户看到"连接失败"但会话已 live） |
| `select_ssh_server` | `fn(State<...>, server_id) -> Result<ServerInfo, String>` | 切换激活服务器并同步旧字段（`sshPassword` 置 None、`sshPrivateKey` 取 `keyPath`） |
| `delete_ssh_server` | `fn(State<AppState>, State<SshManager>, server_id) -> Result<(), String>` | 删服务器 + 删 Keychain 凭据 + 若删的是当前激活项且会话连着它则 `disconnect()`；随后切到剩余第一个或重置默认 |
| `ssh_write` / `ssh_read` / `ssh_disconnect` | `fn(State<SshManager>, ...)` | 终端读写与断开（轻薄命令，无需 async） |
| `ssh_exec` | `async fn(State<...>, command, timeout_secs) -> Result<(String, String, i32), String>` | 在激活服务器非交互执行；timeout 默认 15，clamp 到 5~60 |
| `server_metrics` | `async fn(State<...>) -> Result<serde_json::Value, String>` | 一键探针（CPU/内存/磁盘/负载/在线时长/网卡），timeout 20s |
| `parse_server_metrics` | `fn(&str) -> serde_json::Value` | 解析 `---CPU---` / `---MEM---` / ... 分段输出 |
| `attach_server_identity` | `fn(Value, name, host, user) -> Value` | 补 `server: {name, host, user}` 对象（前端契约要求） |
| `extract_pct` | `fn(line, key) -> f64` | 从 `top` 的 `%Cpu(s): 5.2 us, 3.1 sy, ..., 89.8 id` 抓 id 值 |

#### 组合根

| 函数 | 作用 |
|---|---|
| `run()` | `pub fn`，Tauri 入口。创建 `AppState` → 启看门狗线程 → 启启动清理线程 → `Builder`（manage 两个 State、装 3 个 plugin、注册 **24 个命令**）→ `build()` + `run(callback)`，callback 里处理 `RunEvent::Exit` |

### 注册的 Tauri 命令（24 个）

```
get_status, get_config, save_config, scan_apps,
start_proxy, stop_proxy, set_system_proxy,
ssh_connect, ssh_write, ssh_read, ssh_disconnect, ssh_exec, server_metrics,
select_ssh_server, delete_ssh_server,
self_test, check_conflicts, kill_foreign_proxies, list_foreign_proxies,
fetch_subscription, proxy_api,
updater::get_update_channel, updater::set_update_channel,
updater::check_channel_update, updater::install_channel_update
```
（`fetch_subscription` 定义在 `lib.rs`，含 SSRF 校验 + `curl --noproxy '*'` 拉取 + 解析）

### 单元测试（`mod tests` + `mod server_metrics_tests`）

| 测试 | 验证点 |
|---|---|
| `parse_http_response_chunked` | chunked 的终止块被剥离，body 是纯 JSON |
| `parse_http_response_content_length` | 401 响应按长度精确截取 |
| `parse_http_response_multi_chunk` | 多 chunk 拼接 = `"hello world"` |
| `parse_http_response_chunked_with_utf8` | 中文（多字节 UTF-8）按字节切分不错位 |
| `parse_metrics_linux` | 完整 Linux 探针输出解析为预期字段 |
| `parse_metrics_empty_sections` | `n/a` 分段不产生垃圾字段 |
| `metrics_include_server_identity_for_dashboard` | 身份对象存在 |

---

## 2.2 `mihomo.rs`（1038 行）—— 内核管理

### 常量

| 常量 | 值 | 说明 |
|---|---|---|
| `API_PORT` | `19091` | `external-controller` |
| `PROXY_PORT` | `7893` | 无条件 PROXY 的「路」 |
| `DIRECT_PORT` | `7892` | 无条件 DIRECT 的「路」 |
| `PROTECTED_DIRECT_DOMAINS` | `["203.0.113.74"]` | 保命直连名单。注释说明：AI 助手中转站的模型请求是超长流式 JSON，被 TUN/应用层代理"拆-组"会损坏请求体（400 Invalid JSON body） |

### `MihomoManager`

```rust
#[derive(Clone)]
pub struct MihomoManager {
    pub pid: Arc<Mutex<Option<u32>>>,   // Arc 包裹 → clone 出的是同一份共享 PID 状态
    pub port: u16,                       // 默认 7891
    pub runtime_dir: PathBuf,            // <config_dir>/magic-agent/runtime
}
```

`#[derive(Clone)]` + `Arc<Mutex<..>>` 的用意：实例可 move 进阻塞线程池，主线程持有的实例仍能读到 PID。

### 方法清单

| 方法 | 签名 | 作用 |
|---|---|---|
| `new` | `fn() -> Self` | 初始化，`port = 7891` |
| `status` | `fn(&self) -> MihomoStatus` | 三重判定：内存 PID 存活 → `pgrep` 重新发现 → 控制 API 可连（500ms 超时） |
| `start` | `fn(&self, cfg, rules, app_rules) -> Result<MihomoStatus, String>` | 完整启动流程（见下） |
| `reload_rules` | `fn(&self, cfg, app_rules) -> Result<(), String>` | 写完整 YAML → `ctl("reload")` 优先 → 否则 `pgrep` 找 PID → `osascript` 提权 `kill -HUP` |
| `build_rules_for` / `build_rules` | `fn(&self, cfg, [rules], app_rules) -> Vec<String>` | 三层漏斗规则生成 |
| `find_running_pid` | `fn(&self) -> Option<u32>` | `pgrep -f "magic-agent/runtime/bin/mihomo"`（**不用 `ps`，macOS 的 ps 会截断长命令行**） |
| `stop` | `fn(&self)` | `ctl("stop")` 优先 → 内存 PID 或重新发现 → `kill` → 等 10s → 仍存活则 `osascript` 提权 kill |
| `ctl` | `fn(action: &str) -> Option<String>` | `sudo -n /usr/local/lib/magic-agent/mihomo-ctl.sh <action>`，脚本不存在返回 `None` |
| `copy_geo_files` | `fn(&self) -> Result<(), String>` | 复制 4 个 geo 文件到 runtime |
| `wait_api` | `fn(&self) -> bool` | 等 **API_PORT 19091** 就绪，最多 15s（不能等 `self.port`——混合端口可能先于控制 API 开放） |
| `bin_path` / `bin_path_for_test` / `resource_root` / `geo_dir_for_test` | `fn(&self) -> PathBuf` | 路径解析：优先 runtime 常驻副本 → `resources/bin/mihomo` → `/usr/local/bin/mihomo` |
| `ensure_runtime_bin` | `fn(&self) -> Result<(), String>` | 幂等复制内核到 `runtime/bin`（内容一致则跳过），设 0755 |
| `write_conf` | `fn(&self, conf: &str) -> Result<(), String>` | 写 `mihomo.yaml` 并 chmod 0600 |
| `build_conf` | `fn(&self, cfg, rules, app_rules) -> String` | 生成完整 YAML（见下） |

### `start()` 流程图

```
1. stop()                          ← 先停旧实例
2. create_dir_all(runtime_dir)
3. ensure_runtime_bin()            ← 复制内核常驻（App 升级不影响运行中内核）
4. copy_geo_files()
5. build_conf() → write_conf()     ← 生成并写 0600
6. 首选 ctl("start")               ← 已装 sudoers 白名单则零弹窗
     ├─ 返回 PID 且 wait_api() → Ok
     ├─ 返回 "already-running" 且 wait_api() → Ok（接管现有实例）
     └─ 失败 → 继续走 7
7. osascript 提权启动（回退路径）
     · umask 077 + 对存量日志/轮转 .old chmod 600（root 默认 umask 020 会让全机连接记录世界可读，2026-09-23 修复）
     · 日志轮转：>10MB 改名 .log.old
     · shell_cmd = "umask 077; <轮转+chmod>; '<bin>' -f '<conf>' -d '<dir>' > '<log>' 2> '<err>' & echo $!"
     · 用 & 后台化取 $!，**不能用 nohup**（osascript 的 shell 没有 TTY，nohup 会报错）
     · 解析 PID；失败时区分"用户取消授权"
8. 等混合端口 self.port 就绪，最多 30s（150 × 200ms）
     失败 → kill 进程 + 清 PID + 附加 stderr 末 600 字符后 Err
```

### `build_conf()` 生成的 YAML 结构

```yaml
mixed-port: 7891
mode: rule
interface-name: <detect_default_interface()>   # 钉死出网接口（治 DIRECT i/o timeout）
tun:
  enable: true
  stack: system
  auto-route: false        # ★ 禁区：改 true 会接管系统默认路由，重演劫持事故
  strict-route: true       # 只把明确要代理的流量拉进 TUN
  auto-detect-interface: true
  dns-hijack: [any:53]
listeners:                                    # ★ 每条必须显式 listen（缺省会绑 0.0.0.0，见 CONTRACT 监听红线）
  - {name: proxy-only,  type: mixed, port: 7893, listen: 127.0.0.1, proxy: PROXY}
  - {name: direct-only, type: mixed, port: 7892, listen: 127.0.0.1, proxy: DIRECT}
log-level: info
allow-lan: false
ipv6: false
find-process-mode: always                      # 进程规则的前提
external-controller: 127.0.0.1:19091
secret: "<yaml_quote(api_secret)>"             # 有则写
geo-auto-update: false
geodata-mode: false
geodata-loader: memconservative
dns:
  enable: true
  listen: 127.0.0.1:1054
  enhanced-mode: redir-host                    # 不开 fake-ip
  nameserver: [https://dns.alidns.com/dns-query, https://doh.pub/dns-query]   # 国内 DoH
  fallback:   [tls://8.8.8.8, tls://1.1.1.1]                                  # 境外 DoT
  fallback-filter: {geoip: true, geoip-code: CN}
proxies:                                       # 每个节点：vless + (servername) + (reality-opts)
proxy-groups:
  - name: PROXY, type: fallback, url: http://www.gstatic.com/generate_204, interval: 60
    proxies: [<选中节点排第一>, ...]            # 自动故障转移
  - name: "NODE-<sanitize_node_name(名)>", type: select, proxies: ["<原名>"]
rules: [<build_rules 输出>]
```

> 为什么不用 `PATCH /configs` 热更新规则：实测 mihomo 的 PATCH 对多条 `PROCESS-PATH-REGEX` **只保留第一条**。
> 因此改为写完整 YAML + SIGHUP 重载。

### 辅助函数

| 函数 | 作用 |
|---|---|
| `detect_default_interface()` | `/sbin/route -n get default` 解析 `interface:`，失败回退 `en0`。背景：`auto-detect-interface` 在 macOS 偶发抓错（虚拟网卡/桥接/utun）→ DIRECT 出站一天 1500+ 次 i/o timeout |
| `process_alive(pid)` | `/bin/kill -0 <pid>` |
| `yaml_quote(s)` | 转义 `\ " \n \r \t` 及 `<0x20` 控制字符为 `\uXXXX` |
| `sanitize_rule_field(s)` | 只保留 ASCII 字母数字与 `. - _ : * / # [ ]` |
| `sanitize_node_name(s)` | `pub(crate)`：保留中文/空格，只剔除 `, \n \r` 与控制字符 |
| `regex_escape_path(s)` | 转义 `\ . + ( ) [ ] { } ^ $ \| ? *` |

### 单元测试

`resource_root_exists`、`rules_follow_three_layer_funnel_order`（验证规则顺序与各层内容），
另有 `generated_conf_is_valid_mihomo_yaml`（`CONTRACT.md` 提到，用 `mihomo -t` **真校验**生成的配置）。

---

## 2.3 `config.rs`（953 行）—— 配置与订阅

### 数据结构

| 类型 | 字段 |
|---|---|
| `ProxyNode` | `name, server, port: u16, uuid, flow, network, tls: bool, udp: bool, fingerprint, public_key, short_id, sni, source, region` |
| `AppSetting` | `id, mode`（`"direct"`/`"proxy"`）, `node: Option<String>`, `reason, confirmed: bool` |
| `ServerInfo` | `id, name, host, port, user, auth`（`"password"`/`"key"`）, `password_saved, private_key_saved, key_path: Option<String>` |
| `DomainRule` | `domain, target`（`"proxy"`/`"direct"`/节点名）, `reason` |
| `AppConfig` | `nodes, selected_node, apps, system_proxy, auto_global, subscription_url, servers, active_server_id, domain_rules, api_secret, update_channel` + 6 个旧 SSH 字段（`ssh_host` / `ssh_port` / `ssh_user` / `ssh_auth` / `ssh_password` / `ssh_private_key`） |

**兼容性设计**：所有字段带 `#[serde(alias = "...")]`（snake_case 别名），`Default` 里 `update_channel = Some("github")`、
`nodes = vec![]`（**不硬编码任何真实节点信息**，隐私考虑）。

### 函数清单

| 函数 | 签名 | 作用 |
|---|---|---|
| `AppConfig::active_server` | `fn(&self) -> Option<ServerInfo>` | 优先 `active_server_id` → `servers.first()` → 旧字段。**旧字段 host 缺失时会从 `selected_node` 推导服务器地址**（代理与 SSH 是同一台机器，不让用户重复填） |
| `config_path` | `fn() -> PathBuf` | `<config_dir>/magic-agent/config.json` |
| `normalize_keys_to_camel` | `fn(&mut serde_json::Value)` | 递归把 object 的键 snake_case → camelCase（只动 key，不动 value） |
| `snake_to_camel` | `fn(&str) -> String` | 无下划线原样返回；否则首段小写、后续段首字母大写 |
| `parse_config_str` | `fn(&str) -> Result<AppConfig, String>` | Value 解析（重复键取最后）→ 键规整 → `from_value` |
| `backup_once` | `fn(&Path)` | 备份为 `.json.corrupt`，**只在备份不存在时写** |
| `recover_from_backup` | `fn(&Path) -> Option<AppConfig>` | 从 `.corrupt` 尝试恢复 |
| `load` | `pub fn() -> AppConfig` | 三层健壮性读取（见 01 文档 §5.2） |
| `generate_api_secret` | `fn() -> String` | `/dev/urandom` 读 16 字节转 hex；失败用时间戳+PID+地址 xorshift 兜底 |
| `save` | `pub fn(&AppConfig) -> Result<(), String>` | 原子写 + chmod 0600 在 rename 之前 |
| `validate_public_host` | `pub fn(&str) -> Result<(), String>` | SSRF 防护：去端口（含 IPv6 方括号）→ 字面 IP 直接判 → 域名做 DNS 解析，任一结果内网即拦截 |
| `is_private_or_reserved` | `pub fn(IpAddr) -> bool` | v4：loopback/private/link-local/multicast/unspecified/broadcast/documentation/CGNAT `100.64/10`；v6：loopback/unspecified/multicast/IPv4-mapped/ULA `fc00::/7`/link-local `fe80::/10` |
| `parse_vless_subscription` | `pub fn(&str) -> Result<Vec<ProxyNode>, String>` | 不含 `vless://` 时先 `base64 -D` 解码；按行找 `vless://`；按 `server:port` 去重 |
| `parse_vless_uri` | `fn(&str) -> Result<ProxyNode, String>` | 解析 `vless://uuid@host:port?params#fragment`，映射 `flow`/`type`/`security`/`fp`/`pbk`/`sid`/`sni` |
| `guess_region` | `pub fn(&str) -> String` | 名称关键词 → 美国/日本/香港/台湾/新加坡/韩国/英国/德国 |
| `url_decode` | `fn(&str) -> String` | 百分号解码。**只按字节判断，绝不切片 str** —— 旧实现 `&s[i+1..i+2]` 遇到 `%中` 会按字节切进字符内部直接 panic |

### 单元测试

`snake_to_camel_works`、`normalize_keys_handles_duplicate_snake_and_camel`（回归：旧 snake_case 与新
camelCase 键共存不再导致整份配置被清空）、`load_recovers_real_corrupt_file`（用本机真实 `.corrupt`
文件端到端验证，文件不存在则跳过）。

---

## 2.4 `apps.rs`（939 行）—— App/进程扫描

### 数据结构

```rust
pub struct AppEntry {
    pub id: String,               // "app-<名>" | "bin-<可执行文件路径>"
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: Option<String>,
    pub running: bool,
    pub mode: String,
    pub category: String,
    pub confirmed: bool,
    pub node: Option<String>,
    pub rule_paths: Vec<String>,  // 规则匹配用路径前缀列表
    pub online: bool,             // 有非回环活跃连接
    pub remote_ips: Vec<String>,
}
struct RunningProcess { app_dir, app_path, bundle_id }   // 私有
```

### 函数清单

| 函数 | 签名 | 作用 |
|---|---|---|
| `scan_macos_apps` | `pub fn() -> Vec<AppEntry>` | **主入口**：装 App → 运行进程 → 网络连接 → 回填 online/remote_ips → 脚本进程 → 按 name 排序 |
| `classify` | `pub fn(&str) -> String` | 名称关键词 → `浏览器/通讯/会议/开发工具/AI 工具/其他` |
| `scan_installed_apps` | 私有 | 扫 `/Applications`、`/System/Applications`、`/System/Applications/Utilities`、`~/Applications`；`canonicalize` 去重；优先 `Contents/MacOS/<名>` |
| `scan_running_processes` | 私有 | `ps -axo pid=,comm=,args=`；用 `args=`（非 `comm=`，避免截断）；只保留含 `.app/` 的行 |
| `scan_network_connections` | 私有 | `lsof -nP -iTCP -sTCP:ESTABLISHED`，**硬超时 2500ms**（超时 kill 并返回空 map）；只取 ESTABLISHED 且排除回环 |
| `scan_script_processes` | 私有 | 解释器白名单 `python/python3/node/ffmpeg/java/ruby/perl/go/deno/bun/php`；记录 argv[0] 可执行路径；id = `bin-<路径>` |
| `mark_running_apps` | 私有 | 先按可执行文件路径、再按 `.app` 目录、最后按 bundle id 匹配 |
| `process_name_matches` | 私有 | 精确相等或**边界匹配**（后跟 `  .( / : - _`）；候选名 < 2 字符只允许精确相等（避免 `Go` 误配 `Google Chrome`） |
| `app_ids` | 私有 | 无重名用历史 id `app-<名>`；重名追加 `--<bundleId>` 或 `--<路径>` |
| `rule_paths_for` | 私有 | 默认 `<app>/Contents/`；`com.apple.Safari` 额外加 `/System/Library/Frameworks/WebKit.framework/`（Safari 实际联网进程是系统级 XPC 服务） |
| `infer_script_project_dir` | 私有 | 按 shell 引号规则切分 argv，优先找含脚本后缀的参数取所在目录 |
| `shell_split_argv` | 私有 | 自实现 shell 引号解析（单引号字面、双引号简化不展开 `$`、反斜杠转义） |

### 外部命令依赖

`/usr/sbin/lsof`、`/bin/ps`、`/usr/bin/plutil -extract CFBundleIdentifier raw -o - <Info.plist>`。

---

## 2.5 `ssh.rs`（670 行）—— SSH 会话

### 数据结构

```rust
pub struct SshSession { pub id: String, pub host: String, pub port: u16, pub user: String, pub status: String }
// id 格式："ssh-<user>@<host>"（与 config 的 ServerInfo.id 同形，可直接比对）

#[derive(Clone)]
pub struct SshManager {
    pub child:   Arc<Mutex<Option<Child>>>,
    pub stdin:   Arc<Mutex<Option<ChildStdin>>>,
    pub buffer:  Arc<Mutex<Vec<u8>>>,
    pub session: Arc<Mutex<Option<SshSession>>>,
}
```
全部字段 `Arc<Mutex<..>>` + `derive(Clone)` → 可跨线程共享同一会话（`ssh_connect` 要把实例 move 进线程池）。

### 函数清单

| 函数 | 签名 | 作用 |
|---|---|---|
| `new` | `pub fn() -> Self` | 初始化 4 个 Arc |
| `password_account` | `pub fn(host, user) -> String` | `ssh-password-<user>@<host>` |
| `key_account` | `pub fn(host, user) -> String` | `ssh-key-<user>@<host>` |
| `connect` | `pub fn(&self, host, port, user, auth, password, key) -> Result<SshSession, String>` | 建立交互会话 + **认证结果验证** |
| `disconnect` | `pub fn(&self)` | kill 子进程、清空 stdin/session/buffer |
| `write` | `pub fn(&self, data: Vec<u8>) -> Result<(), String>` | 写 stdin；失败清 child/session |
| `read` | `pub fn(&self) -> Result<Vec<u8>, String>` | `mem::take` 取出累积 buffer |
| `status` | `pub fn(&self) -> Option<SshSession>` | 返回会话快照 |
| `exec` | `pub fn(&self, host, port, user, auth, command, timeout_secs, key_path) -> Result<(String, String, i32), String>` | 非交互执行，返回 `(stdout, stderr, exit_code)` |
| `expand_ssh_key` | `pub fn(&str) -> PathBuf` | `~` 展开 |

### 内部实现要点

**`connect` 认证验证（最多 12 秒）**

```
1. 先 disconnect()
2. 密码来源：传入明文（去空白）→ 否则 Keychain
3. ssh -tt -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=15
       -o ServerAliveCountMax=3 -o ConnectTimeout=10 [-p 端口] [-i 私钥] user@host
4. 密码认证走 expect -f -，脚本经 **stdin 喂入**（明文密码永不落盘）
5. 轮询 buffer 指纹：
   · 命中 ssh_failure_reason → kill + Err（错误密码不写 Keychain）
   · 命中 ssh_login_confirmed（"Last login:" / "Welcome to" / 末尾 # 或 $）→ 通过
   · 进程提前退出 → Err（含末 200 字符）
   · 超时且输出非空 → "软通过"（自定义提示符无特征）
   · 超时且输出为空 → Err("SSH 连接超时（12 秒内无任何输出）")
```

**`expect` 脚本（connect）逻辑**：等 `(?i)password:\s*` → 发密码 → 若再现密码提示或 `Permission denied`
→ `puts MAGIC_AUTH_FAILED; exit 3`；`Are you sure.*` → 发 `yes` + `exp_continue`；timeout →
`MAGIC_AUTH_TIMEOUT; exit 4`；eof → `exit 1`；最后 `interact`。`set timeout 20`。

**`exec` 脚本差异**：`set timeout <timeout_secs>`；`expect eof` 后
`set rc [lindex [wait] 3]; exit $rc` —— **取回 spawn 子进程真实退出码**，否则远端失败也会报成功。

**缓冲区保护**：两个分支都用 `spawn` + drain 线程 + `try_wait` 轮询（50ms）+ 超时 `kill`（超时 = `timeout_secs + 10`）。
drain 是为防止远端输出超 64KB（macOS 管道缓冲）导致子进程写阻塞。密钥分支用 `Stdio::null()` stdin 且
**刻意不用 `.output()`**（会永久阻塞 Tauri 命令线程）。

**临时私钥生命周期**：`looks_like_private_key`（只认 `-----BEGIN ... PRIVATE KEY-----`）校验后写入
`$TMPDIR/magic-ssh-key-<pid>-<nanos>`，0600；成功/失败各路径均 `remove_file`。

**锁顺序**：`write` 失败时先 `drop(stdin)` 锁再锁 `child`，避免与 `disconnect` 的 `child→stdin`
反向顺序造成死锁。

**`ssh_failure_reason` 指纹表**：`MAGIC_AUTH_FAILED`、`MAGIC_AUTH_TIMEOUT`、`Permission denied`、
`Enter passphrase for key`、`Connection refused`、`Operation timed out`、`Connection timed out`、
`No route to host`、`Could not resolve hostname`、`nodename nor servname provided`、
`Host key verification failed`、`no matching host key type`、`no matching cipher`。

---

## 2.6 `system_proxy.rs`（115 行）—— 系统代理

| 函数 | 签名 | 作用 |
|---|---|---|
| `set_system_proxy` | `pub fn(enable: bool, port: u16) -> Result<SystemProxyStatus, String>` | 遍历网络服务，`enable` 时设 web/secure-web/socks 代理为 `127.0.0.1:<port>` 并打开；否则只关。任一成功即视为成功，全失败返回首个错误。**⚠️ 已知缺陷（CONTRACT「待修」登记）**：any_success 会在多服务部分失败时报假成功，重构计划 P0-1 改为逐服务读回对账——改此函数前先看 docs/重构计划.md |
| `status` | `pub fn() -> SystemProxyStatus` | **读 `scutil --proxy` 的真实值**（`HTTPEnable : 1` / `SOCKSEnable : 1`），不是读 config 意图 |
| `list_services` | 私有 | `networksetup -listallnetworkservices`，跳过 bluetooth / iphone / thunderbolt / bridge / asterisk 等非真实服务 |
| `parse_port` | 私有 | 从 `scutil` 输出解析 `HTTPPort` / `SOCKSPort` |

`SystemProxyStatus { enabled: bool, http_port: u16, socks_port: u16 }`

> `CONTRACT.md` 反面教材：`status.systemProxy` 曾读 `config.json` 字段（意图），App 重启后 config 仍 true
> 但实际已关 → 误导智能体。改为读 `scutil --proxy`。

---

## 2.7 `keychain.rs`（69 行）—— 凭据存储

常量 `SERVICE = "com.magic.agent"`。

| 函数 | 签名 | 关键点 |
|---|---|---|
| `store` | `pub fn(account, secret) -> Result<String, String>` | `security add-generic-password -s <SERVICE> -a <account> -w -U`，**`-w` 不带值，secret 走 stdin**（不进 `ps` 可见的 argv） |
| `get` | `pub fn(account) -> Result<String, String>` | `security find-generic-password ... -w`，返回前 trim 换行 |
| `delete` | `pub fn(account)` | `security delete-generic-password`，忽略错误 |
| `exists` | `pub fn(account) -> bool` | 以 `get` 是否成功判断 |

---

## 2.8 `updater.rs`（296 行）—— 双通道更新

### 常量

| 常量 | 值 |
|---|---|
| `ENDPOINT_LOCAL` | `http://127.0.0.1:7878/latest.json` |
| `ENDPOINT_GITHUB` | `https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json` |
| `DEFAULT_CHANNEL` | `"github"` |

> 为什么需要运行时切端点：Tauri updater 的 endpoint 在**编译期固定**，运行时只能用
> `app.updater_builder().endpoints(vec![url]).build()` 重建 Updater。两通道共用同一份签名的
> 更新包/feed，切换只改"去哪取 latest.json"，Ed25519 验签不变。

### 数据结构

| 类型 | 字段 |
|---|---|
| `UpdateChannelInfo` | `channel, endpoint, label`；`from_channel()` 构造 |
| `UpdateCheckResult` | `current_version, available: bool, version: Option<String>, notes: Option<String>, endpoint, channel` |
| `ProxyEnvGuard`（私有） | `saved: Vec<(&'static str, Option<String>)>`，`Drop` 时恢复 env |
| `ConfigState`（私有别名） | `Arc<Mutex<crate::AppState>>` —— 注释强调必须是 `AppState` 而非 `AppConfig`，否则 `state()` 会 panic |

### 函数清单

| 函数 | 签名 | 作用 |
|---|---|---|
| `normalize_channel` | `pub fn(&str) -> String` | `local/dev/development` → `local`；**其余一律回 `github`**（面向用户更安全） |
| `endpoint_for` | `pub fn(&str) -> &'static str` | 通道 → 端点 |
| `channel_label` | `pub fn(&str) -> &'static str` | `本地开发测试` / `GitHub 公开发布` |
| `read_channel` | 私有 | 加锁读 `config.update_channel`，None 用默认；锁中毒用 `into_inner()` 恢复 |
| `get_update_channel` | `#[tauri::command] fn(State) -> UpdateChannelInfo` | 纯读，不触网 |
| `set_update_channel` | `#[tauri::command] fn(String, State) -> Result<..>` | 写 config + 持久化 |
| `check_channel_update` | `#[tauri::command] async fn(AppHandle) -> Result<UpdateCheckResult, String>` | 重建 Updater + `check()`，只检查不安装 |
| `install_channel_update` | `#[tauri::command] async fn(AppHandle) -> Result<(), String>` | 下载 + 安装 |
| `scope_update_proxy_env` | 私有 | 内核运行时设 `HTTP_PROXY/HTTPS_PROXY/ALL_PROXY`（含小写共 6 个 key）为 `http://127.0.0.1:<port>`，`NO_PROXY=127.0.0.1,localhost,::1`，返回 Guard 供析构恢复 |

**`install_channel_update` 细节**：先 `check()`，无更新返回 `"已是最新版本，无需安装"`；
下载走本机 mihomo（否则直连 GitHub 慢）；回调 emit `update-progress` 事件
（`{event:"progress", chunkLength, downloaded, contentLength}` / `{event:"finished"}`）；
macOS/Linux 安装后需用 `tauri-plugin-process` 的 `relaunch()` 重启。

---

## 2.9 `bin/` —— 命令行工具

| 文件 | 作用 |
|---|---|
| `main.rs`（4 行） | `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` + `magic_agent_lib::run()` |
| `bin/magic_probe.rs`（15 行） | 启代理 → `println!("STARTED port=... node=...")` → sleep 8s → 停代理 → `println!("STOPPED")`；失败 `eprintln!` + `exit(1)` |
| `bin/dump_conf.rs`（22 行） | stdin 读 `config.json` → 解析为 `AppConfig` → 用**空 `path_lookup`** 调 `settings_to_app_rules`（`bin-` 前缀项可直接对比，`app-` 项因需实机扫描而跳过）→ `build_rules_for` → 逐行打印规则。供 `scripts/check_parity.py` 与 Python 引擎做 diff |
