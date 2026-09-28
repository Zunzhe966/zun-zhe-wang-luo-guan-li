# 05 · 脚本与工具链

> 对应源码：`scripts/`（约 800 行）、`tests/`
> **定位**：构建、发版、图标、特权控制器、MCP 守护、一致性校验的自动化工具链。脚本层不参与运行时业务，但 **发版流程是项目宪法级铁律**（见 `CONTRACT.md`）。

---

## 1. 脚本总览

```
scripts/
├── release.sh                    ★ 发版唯一入口（构建+签名+生成双通道 feed+可选发布）
├── make-icons.sh                 ★ 图标唯一入口（从母版生成全平台资源）
├── icon_build.py                 图标平台规范化后处理（macOS 圆角/iOS 去 alpha/Android 安全区）
├── mihomo-ctl.sh                 root 内核控制器（install_privileged_helper 装到 /usr/local/lib）
├── magic-agent-mcp.sh            MCP HTTP 桥接守护脚本（start/stop/status/restart）
├── com.magic-agent.mcp.plist     launchd 配置（登录时启动 MCP HTTP 桥接）
├── check_parity.py               ★ 双规则引擎一致性校验（Rust vs Python）
├── openrouter_free_models.py     OpenRouter 免费模型台账生成器
├── updater-feed/                 本地更新源（latest.json + 包 + 签名；http.server 7878 的根）
└── updater-feed-publish/         GitHub 发布源（ASCII 命名资产 + latest.json）
```

---

## 2. `release.sh` —— 发版唯一入口（9.3 KB）

`CONTRACT.md` **铁律**：禁止手工 `cp` 覆盖 `/Applications` 来"发版"，必须走本脚本。

```bash
./scripts/release.sh <version>              # 本地测试通道
./scripts/release.sh <version> --publish    # 同时发布到 GitHub Release
```

### 2.1 设计原则：一个源，两份 feed

包**只构建一次、只签名一次**，生成的 feed 只有一份内容，只把"包下载地址"换成两种写法：

| feed | 目录 | 包 URL |
|---|---|---|
| 本地测试 | `scripts/updater-feed/latest.json` | `http://127.0.0.1:7878/<包名>` |
| 公开发布 | `scripts/updater-feed-publish/latest.json` | `https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/download/v<ver>/<asset>` |

两边 **signature 完全相同**（同一把 `~/.tauri/magic-agent.key` 私钥签的），App 两个通道也用同一把公钥验签 → 本地与 GitHub 永远是同一份东西，不存在不同步。

### 2.2 执行步骤（含多处历史事故的防呆）

| 步骤 | 做什么 | 防的坑 |
|---|---|---|
| 1 | 校验版本号格式 `x.y.z`、`--publish` 参数合法 | 防手滑传错参数 |
| 2 | 同步三处版本号：`tauri.conf.json`(jq)、`Cargo.toml`(sed)、`ui/package.json`(sed) | — |
| 3 | **回读三处版本号断言一致**，不一致即退出 | jq/sed 静默失败时不至于把错版本发出去 |
| 4 | `bash scripts/make-icons.sh` 强制重建全部图标 | 防缓存旧图或缺失 512/1024 层 |
| 5 | `export TAURI_SIGNING_PRIVATE_KEY` → `cargo tauri build` | — |
| 6 | 断言 bundle 目录**恰好一个** `.app.tar.gz` | 防旧产物混入 |
| 7 | **对账**：用 PlistBuddy 读产物 `.app` 的 `CFBundleShortVersionString`，必须 == 发版参数 | 历史事故：手工把 0.2.6 的包配 0.2.7 的假 feed，验签能过（签名只管包没被篡改、不管版本号），用户"更新"完版本不变 |
| 8 | 架构对齐：`uname -m` → `aarch64`/`x86_64`；校验内置 mihomo 架构与产物一致 | — |
| 9 | 资产名统一 ASCII：`magic-agent_<ver>_<arch>.app.tar.gz` | GitHub Release 上传中文名会被静默改写（→ `app.tar.gz`），导致 latest.json 里的地址 404 |
| 10 | 拷贝包+sig 到两个 feed 目录；发布 feed 额外放 ASCII 命名副本 | — |
| 11 | `write_feed()` 生成两份 latest.json（version/notes/pub_date/platforms.signature/url） | — |
| 12 | **ad-hoc 重签名**（`codesign --force --deep --sign -`） | Tauri 默认 linker-signed 不密封 Resources，`codesign --verify` 会报错 |
| 13 | `codesign --verify` 校验；`spctl --assess` 检查公证（ad-hoc 未过属正常，用户右键打开即可） | — |
| 14 | `--publish` 时 `gh release create/upload`（资产 + sig + latest.json，`--clobber`） | — |

**注意**：`${GH_TAG}` 必须用 `{}` 定界——bash 3.2 对"变量名后紧跟全角括号"有多字节解析 bug，会误报 unbound variable。

### 2.3 完整闭环（`CONTRACT.md`）

```
改代码 → cargo test 全绿 → release.sh <新版本> → 部署验证 → git commit
```
三处版本号 + latest.json 版本必须指向同一版本。

---

## 3. `make-icons.sh` + `icon_build.py` —— 图标唯一入口

**铁律**：图标唯一母版必须是 `src-tauri/icons/source-icon.png`，全部尺寸只能经 `make-icons.sh` 生成；禁止手工替换单个尺寸后直接发版。

`make-icons.sh` 两步走：
1. 校验母版是**正方形 PNG 且 ≥1024px**，然后 `cargo tauri icon`（生成 Windows/Linux 方形的 PNG/ICO）。
2. `python3 scripts/icon_build.py` 做 Tauri CLI 做不到的平台规范化：
   - **macOS `icon.icns`**：按 Apple 图标网格嵌入松鼠形圆角矩形（1024 画布、内缩 82.4pt、圆角 185.4pt、4x 超采样抗锯齿）。
   - **iOS**：去掉 alpha 通道（App Store 强制），补 1024 营销图。
   - **Android**：自适应图标前景收进 72/108 安全区（任何遮罩都不切主体），背景取母版边缘主色，legacy/round 同风格合成。

`icon_build.py` 依赖 Pillow（`from PIL import Image...`）。

---

## 4. `mihomo-ctl.sh` —— root 内核控制器

安装到 `/usr/local/lib/magic-agent/`（root 所有），经 `/etc/sudoers.d/` 白名单免密调用，实现**零弹窗**启停。对应 MCP 的 `install_privileged_helper` 工具。

- 以 sudo 运行（root），`$HOME` 是 `/var/root`，**从 `SUDO_USER` 反推真实用户 home**（避免硬编码用户名）。
- 路径：`$USER_HOME/Library/Application Support/magic-agent/runtime/{bin/mihomo,mihomo.yaml,mihomo.log,mihomo.err.log}`。
- `PATTERN='magic-agent/runtime/bin/mihomo'`——精确匹配本 App 常驻副本。**绝不能用宽泛的 `resources/bin/mihomo`**，否则会误杀 FlClash/Clash Verge 等第三方内核。
- 命令：`start`（幂等；`ensure_bin` 探测 `/Applications/尊者网络管理.app`，历史名 `尊者魔法代理.app`/`魔法代理.app` 兜底；**umask 077 + 存量日志/轮转 .old chmod 600**（2026-09-23 隐私修复，root 默认 umask 会把全机连接记录落成世界可读）；日志超 10 MB 轮转 `.old`；后台启动并回显 PID）/ `stop`（pkill）/ `reload`（HUP）/ `status`（pgrep）。

---

## 5. MCP 守护：`magic-agent-mcp.sh` + `com.magic-agent.mcp.plist`

让 WorkBuddy 等 **HTTP MCP 客户端**接入：拉起 `mcp/server.py --http 19092`。

`magic-agent-mcp.sh {start|stop|status|restart}`：
- 自动定位 `mcp/server.py`（优先脚本上级目录；否则回退 `~/Desktop`、`~/Applications` 下的 `网络管理`/`尊者网络管理` 两种目录名——历史上曾写死导致找不到）。
- `PIDFILE=/tmp/magic-agent-mcp.pid`、`LOG=/tmp/magic-agent-mcp.log`。
- `start` 用 `nohup python3 … --http 19092 &`，并**轮询 `/health`（最多 20×0.25s）确认就绪**才算启动成功；`health()` 用 `curl --noproxy '*'`（避免被系统代理劫持到自身）。

`com.magic-agent.mcp.plist`（launchd）：
- Label `com.magic-agent.mcp`，`RunAtLoad=true`，**`KeepAlive=false`**。
- **2026-09-03 修复注释**：去掉 KeepAlive 常驻——MCP 服务随登录启动即可，崩溃不自动复活。KeepAlive 会造成"App 关了服务还在"的失控感，且掩盖崩溃问题。
- 需手动把 `/绝对路径/网络管理/mcp/server.py` 替换为真实路径。

---

## 6. `check_parity.py` —— 双规则引擎一致性校验（4.5 KB）

背景：网络管理有**两份规则生成逻辑**——Rust `mihomo.rs::build_conf`（冷启动）与 Python `server.py::generate_config`（MCP 热重载）。历史上漂移过一次（域名/进程规则顺序颠倒）。本脚本用同一份样例配置喂给两侧，对 `rules` 段**逐行 diff**，不一致则退出码 1。

```bash
cd src-tauri && cargo build          # 前置：生成 target/debug/dump_conf
python3 scripts/check_parity.py      # 0=一致，1=漂移，2=缺二进制
```

- 样例 `SAMPLE`：2 个节点（含多节点共用 server 验证防卷去重、三种 target 全覆盖、未确认条目应被忽略），app 条目全部用 `bin-` 前缀（不依赖实机 `/Applications` 扫描，两侧可确定性对比）。
- Rust 侧经 `dump_conf` 二进制（读 stdin JSON → 打印规则行）。
- **进程规则行只比结构**：Python `re.escape` 与 Rust `regex_escape_path` 转义字符集不同，`normalize()` 对 `PROCESS-PATH-REGEX,` 行只比"路径+目标"结构，其余行严格相等。
- `app-` 前缀条目依赖实机扫描，属非确定性行为，不纳入 parity 范围（分流表实际以 Rust 扫描为准）。

---

## 7. `openrouter_free_models.py` —— 免费模型台账生成器

拉 OpenRouter 全量模型 → 过滤免费（`:free` 后缀 或 prompt/completion 定价为 0）→ 生成两份产物：
- `docs/免费模型清单.md`（人看的分组台账）
- `docs/free_models.json`（**MCP `list_free_models` 工具的数据源**）

```bash
python3 scripts/openrouter_free_models.py          # 刷新台账
python3 scripts/openrouter_free_models.py --all    # 附带付费模型清单
```
分流说明：`openrouter.ai` 已有域名规则固定走节点，脚本无需任何代理配置。

---

## 8. `updater-feed/` 与 `updater-feed-publish/`

- **`updater-feed/`**：本地测试更新源。含 `latest.json` + `尊者网络管理.app.tar.gz` + `.sig`（保留原始文件名，本机 http.server 不改写）。
  启动：`cd scripts/updater-feed && python3 -m http.server 7878 --bind 127.0.0.1`。App 设置页切到"本地开发测试"即可检查更新。
- **`updater-feed-publish/`**：公开发布源。含历史版本资产（ASCII 名，如 `magic-agent_0.2.5/0.2.6/0.2.7_aarch64.app.tar.gz`）+ 各自 `.sig` + `latest.json`。`--publish` 时上传这些资产到 GitHub Release。

> 注意：这两个目录里的 `.app.tar.gz` 是 40+ MB 的构建产物，属**运行时生成物**，通过 `.gitignore` 管理（见 [06-run-and-build.md](./06-run-and-build.md)）。`latest.json` 里的下载地址是唯一需要与发布版本同源的部分。

---

## 9. `src-tauri/src/bin/` 下的两个 CLI 工具

| 工具 | 行数 | 作用 |
|---|---|---|
| `dump_conf.rs` | ~20 | 调试/校验：从 stdin 读 `config.json`，用 Rust 引擎打印规则行，供 `check_parity.py` diff。**仅暴露 `bin-` 前缀条目**（`app-` 跳过，因需实机扫描） |
| `magic_probe.rs` | ~14 | 冒烟测试：`start_proxy_standalone()` → sleep 8s → `stop_proxy_standalone()`，验证 mihomo 可启停 |

用法：`cargo run --bin magic_probe`（验证内核可用）；`echo '<json>' | cargo run --bin dump_conf`。

---

## 10. `tests/` —— 回归测试

`tests/test_regressions.py`（约 190 行，pytest）。以 `importlib` 直接加载 `mcp/server.py`，用 `monkeypatch` 隔离外部依赖。覆盖的回归点：

| 测试 | 钉死的契约 |
|---|---|
| `test_server_metrics_include_identity_like_rust_consumer_expects` | `server_metrics` 的解析结果字段名/类型必须与 Rust 消费方一致（`server.{name,host,user}`、`cpu_usage_pct`、`mem_usage_pct`、`disk_usage_pct`、`net_<if>_tx_bytes`） |
| `test_tools_expose_input_schema_for_mcp_clients` | 每个工具必须带 `inputSchema`；`switch_node`/`ssh_exec`/`server_metrics` 的 schema 精确匹配 |
| `test_rule_mutation_reports_hot_reload_failure` | 热重载失败时 `add_domain_rule` 必须返回 `ok=false` 且 message 含"热更新"（不能假装成功） |
| `test_deleted_node_domain_rule_falls_back_to_proxy_group` | 域名规则指向已删节点 → 降级为 `DOMAIN-SUFFIX,<domain>,PROXY`，不生成 `NODE-deleted` |
| `test_deleted_node_app_rule_falls_back_to_proxy_group` | 软件规则指向已删节点 → 降级为 `PROCESS-PATH-REGEX,^/opt/demo,PROXY` |
| `test_http_bridge_rejects_foreign_origin_and_requires_bearer_token` | 非法 Origin → 403；合法 Origin 但无 bearer → 401 |
| `test_http_bridge_allows_native_health_without_origin_or_token` | 无 Origin 的原生客户端 `/health` 必须免鉴权（启动器探测需要） |
| `test_http_bridge_allows_native_jsonrpc_without_origin_or_token` | 无 Origin 的原生 JSON-RPC 不能被 bearer 鉴权破坏兼容性 |

Rust 侧：`cd src-tauri && cargo test`（含 `generated_conf_is_valid_mihomo_yaml`，用 `mihomo -t` 真校验生成的配置）。

---

← [04-mcp-server.md](./04-mcp-server.md)　|　返回 [README](./README.md)　|　下一篇 → [06-run-and-build.md](./06-run-and-build.md)
