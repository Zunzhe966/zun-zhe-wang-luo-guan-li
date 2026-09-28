# 03 · 前端（Vue 3 + Vite）

> 对应源码：`ui/`（入口 `ui/index.html` → `ui/src/main.js` → `ui/src/App.vue`）
> 技术栈：Vue 3（Composition API + `<script setup>`）、Vite 5、@tauri-apps/api 2、@xterm/xterm 5
> 规模：`ui/src/**` 约 1 670 行（含样式）。
> **定位**：本页只讲"界面怎么组织、状态怎么流、点一下按钮调了哪些 Rust 命令"。所有真正的业务逻辑都在 Rust 后端（见 [02-backend-rust.md](./02-backend-rust.md)），前端是**薄壳 + 展示层**。

---

## 1. 全局结构

```
ui/
├── index.html              # 挂载点 <div id="app">，标题"尊者网络管理"
├── vite.config.js          # 端口 5173 strictPort，target safari13（macOS WebView）
├── package.json            # 0.2.10，devDep: vite 5 + @vitejs/plugin-vue
└── src/
    ├── main.js             # createApp(App).mount('#app')，仅 4 行
    ├── App.vue             # 唯一有"状态"的组件：壳层 + 路由 + 全局数据
    ├── toast.js            # 全局轻量提示（替代 alert）
    ├── updater.js          # 全局唯一的更新流程模块（跨页共享）
    ├── styles/main.css     # 全局样式 + CSS 变量 + toast 样式（111 行）
    └── components/
        ├── Dashboard.vue          # 总览：启停、系统代理、第三方代理清理
        ├── ConnectionsView.vue    # 实时连接：轮询 mihomo /connections
        ├── AppsView.vue           # 软件分流：扫描 /Applications，逐软件选模式与节点
        ├── DomainRulesView.vue    # 域名分流：有序规则表
        ├── ServersView.vue        # 云服务器：节点 CRUD、订阅拉取、延迟测速
        ├── ServerDashboard.vue    # 服务器仪表盘：SSH 探测远端 CPU/内存/磁盘/网络
        ├── SshView.vue            # 控制台：xterm.js 终端 + 快捷命令
        └── SettingsView.vue       # 设置：版本、更新检查、更新通道
```

**没有引入 Vue Router / Pinia。** 页面切换靠 `App.vue` 里的 `view` 字符串 + `v-if` 并列渲染；跨页共享状态只有两处：`App.vue` 顶层的 `ref`（status/config/apps），和从 `updater.js` 导出的全局 `reactive` 单例 `updaterState`。

---

## 2. 组件树与数据流

```
main.js
└─ App.vue  ★状态中枢
   ├─ 持有：view, status, config, apps, proxyBusy, proxyError
   ├─ 定时器：每 5s 调 refresh()（get_status + get_config）；仅当停在"软件分流"页时
   │           额外调 refreshApps()（scan_apps，重操作）
   ├─ <Dashboard>          props: status/config/proxyBusy/proxyError
   │                       emit: start / stop / toggle-system-proxy
   ├─ <AppsView>           props: apps / nodes
   │                       emit: change(保存) / refresh(重扫)
   ├─ <ServersView>        props: config
   │                       emit: update / select-server / delete-server / nav
   ├─ <ServerDashboard>    props: config        emit: goto-servers
   ├─ <DomainRulesView>    props: config        emit: update
   ├─ <ConnectionsView>    props: config        （自轮询，不走 App）
   ├─ <SshView>            props: config        emit: saved
   └─ <SettingsView>       props: 无            （直接用 updater.js 全局态）
```

### 2.1 单一数据源与"只升不降"的刷新约定

`App.vue` 是全应用唯一持有业务数据的组件，各子组件通过 `props` 拿数据、通过 `emit` 请求修改（典型单向上行）。这里沉淀了几条**踩过坑才有的约定**，改前端前务必理解：

| 约定 | 位置 | 原因（代码注释原文提炼） |
|---|---|---|
| **轻/重刷新分离** | `refresh()` vs `refreshApps()` | `scan_apps` 是秒级重操作，不能每 5 秒跑；`refresh()` 只拉 `get_status` + `get_config` |
| **扫描结果只覆盖"运行状态"** | `refreshApps()` | 5 秒轮询不能把用户正在编辑但未保存的 `mode/node/confirmed` 冲回旧值——按 `id` 保留旧值 |
| **dirty 门闩** | `ServersView`(`subDirty`)、`DomainRulesView`(`dirty`/`ignoreWatch`)、`SshView`(`dirty`) | 用户一旦开始编辑，就不再让 `props.config` 的轮询新值覆盖输入框 |
| **confirmed 只增不误标** | `applyApps()` | 只有用户明确选"代理"才置 `confirmed=true`，直连的保持原状——否则一次保存把全部软件标 confirmed，规则表爆炸 |
| **保存失败必须回滚可见** | `saveConfig()` | 后端返回 `Err`（如规则热更新失败）时，`config.value` 回滚到 `previous` 并 `toast`，绝不让用户误以为已保存 |

### 2.2 为什么 `App.vue` 用 `view` 字符串而不是路由

应用是单窗口桌面工具，页面间**无 URL 语义、无前进后退需求**，且所有页面共享同一份 `config`。用 `v-if` 并列渲染最直接，也避免为 8 个页面向 WebView 注入 rrouter 的额外体积与打包复杂度。

---

## 3. 子组件逐一说明

### 3.1 Dashboard.vue（总览，116 行）

- **props**：`status`、`config`、`proxyBusy`、`proxyError`；**emit**：`start`、`stop`、`toggle-system-proxy`。
- 顶部按 `status.proxyRunning` 切换"启动代理/停止代理"按钮；`proxyBusy` 期间禁用并显示"正在…"。
- 四张统计卡：代理内核状态（文案写死 `mihomo v1.19.29`）、混合端口、系统代理（`macOS networksetup`）、已识别软件数。
- **第三方代理面板**：`onMounted` 调 `list_foreign_proxies`，若有占用则列出并提供"立即清理"→ `kill_foreign_proxies`，成功后提示"已关闭 N 个第三方代理进程"。这是"启动前让系统回到干净状态"的前端入口。
- "当前分流策略"是**静态说明卡**（三种模式图例），不反映实时数据。

### 3.2 AppsView.vue（软件分流，91 行）

- **props**：`apps`、`nodes`；**emit**：`change`（带整个 apps 数组）、`refresh`。
- 搜索框 + 分段过滤器（全部 / 联网中 / 走代理 / 直连），过滤在前端 `computed` 完成；排序把 `running` 的排前面。
- 每行：应用名 + 类别 tag + 模式下拉（代理/直连）+ 节点下拉（仅"代理"时出现）+ 连接状态徽标 + 路径。
- **防连点**：`save()` 用 `saving` 锁 1.2 s。因为 `applyApps` 会触发 `save_config`→`reload_rules`（走 osascript 提权），连点会重复 IPC + 重复热更新。
- 状态文案由 `statusText()` / `connText()` / `connClass()` 三个纯函数派生，逻辑：未运行→"未运行"；未 confirmed→"默认直连"；proxy 有 node→"代理：节点名"。

### 3.3 DomainRulesView.vue（域名分流，94 行）

- **props**：`config`；**emit**：`update`（返回 Promise，`ok===false` 表示保存失败）。
- 规则**有序**（从上到下首条命中），每行：域名 / 目标（代理·直连·具体节点）/ 备注 / 删除。
- `save()` 做两件前端侧的规范化：
  1. `normalizeDomain()`：去协议头、去路径、去端口、去首尾点与 `*.`。
  2. 同域名去重（去重后同域名的后续规则永远是死规则）。
- **`ignoreWatch` 机制**：`save()` 用规范化后的 list 替换本地 `rules` 会触发 deep watch，而 watch 回调晚于 `dirty=false` 执行、会把 dirty 又置回 true，导致保存失败时 config watch 被 `dirty` 挡住、不回滚。所以 `save()` 内 `ignoreWatch=true` 拦掉这一次回调，`nextTick` 后恢复。

### 3.4 ServersView.vue（云服务器，246 行，前端最复杂）

- **props**：`config`、`status`；**emit**：`nav`、`update`、`select-server`、`delete-server`。
- **三块功能**：
  1. **订阅**：保存 `subscriptionUrl`（`saveSub` → `update`）；"拉取节点"调 `fetch_subscription`，按 `server:port` 去重后合并进 `nodes`。
  2. **节点 CRUD**：手工表单含 name/server/port/uuid/flow(默认 `xtls-rprx-vision`)/sni/publicKey/shortId/fingerprint(默认 `chrome`)。`editNode` / `submitForm` / `removeNode`。
  3. **测延迟**：`testDelay` 调 `proxy_api` 访问 `/proxies/{name}/delay?...`（**不再直接 fetch 19091**，secret 由后端持有）；`delayClass` 分档 good<800ms / mid<2000ms / bad。
- **改名的"连带迁移"**（关键业务逻辑，`submitForm`）：节点名被修改时，必须同步把 `selectedNode`、所有 `apps[].node`、所有 `domainRules[].target` 里引用旧名的值改成新名。否则后端把旧名当"节点已删除"，静默降级到 PROXY(fallback) 组，用户看到"设了走节点 A 却走了当前节点"且无任何提示。
- **删除当前节点自动回退**（`removeNode`）：删掉的正是 `selectedNode` 时，回退到剩余第一个，避免悬空。
- **`subDirty`** 阻止轮询覆盖正在输入的订阅 URL。

### 3.5 ServerDashboard.vue（服务器仪表盘，201 行）

- **props**：`config`；**emit**：`goto-servers`。
- 核心命令：`invoke('server_metrics')`（**优先走 Rust 后端**，凭据由后端持有，前端不碰）。
- `hasServer` 推导：有 `servers` 或 `sshHost`，或**存在选中代理节点**（后端会自动从节点推导 SSH 主机）即为真。
- 每 10 秒自动探测；`watch(hasServer)` 补偿一个坑：`config` 初始为 `null`，`onMounted` 时 `hasServer=false` 不启动定时器，config 异步加载后 `onMounted` 已过——watch 负责在 `hasServer` 变 true 时补启动。
- `netInterfaces` 用正则 `^net_(.+)_rx_bytes$` 从 metrics 扁平字段里抽出网卡列表。
- 进度条 `barClass`：≥85 危险、≥60 警告、其余正常。

### 3.6 SshView.vue（控制台，171 行）

- **props**：`config`；**emit**：`saved`。
- 依赖 `@xterm/xterm` + `@xterm/addon-fit`（深色主题 `#0c111b`）。
- **输入合并**：`term.onData` 把 30 ms 内的按键合并成一次 `ssh_write`，避免每次击键都发 IPC。
- **输出轮询**：每 200 ms 调 `ssh_read`，把返回的字节数组用 `TextDecoder` 写进终端。
- 连接：`ssh_connect`（注释提示"最多约 12 秒"，注释写 `connecting` 但模板用 `connected` 控制按钮）；断开 `ssh_disconnect`。
- **密码绝不进 config**：`emit('saved', …)` 时 `sshPassword: null`，密码只经后端验证后进 Keychain。
- 快捷命令：系统信息 / 内存 / 磁盘 / 进程 / 监听端口 / 最近日志，直接 `ssh_write` 对应命令。
- **清理**：`onUnmounted` 里 `clearInterval` + `clearTimeout` + 若已连则 `ssh_disconnect` + **`term.dispose()`**（否则切页后 xterm 内部定时器与 DOM 监听泄漏）。
- `nodeServer` computed：从选中代理节点推导 SSH 主机（代理与 SSH 是同一台机器），自动填主机并在顶部提示。
- `dirty` 门闩同上。

### 3.7 SettingsView.vue（设置，181 行）

- 无 props。版本号 `getVersion()`（Tauri app API）。
- 更新状态直接读全局 `updaterState`，`statusText` 把状态机映射成中文；`downloading/installing` 时显示进度条。
- **更新通道单选**：`channelModel` 是**可写 computed**（get 读 `updaterState.channel`，set 调 `selectChannel`）——旧实现用 `:checked + @change` 会出现"点了没反应但界面已选中"的状态不同步。
- 通道列表（写死）：`local`（`http://127.0.0.1:7878/latest.json`）与 `github`（`https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json`）。
- `channelHint`：选了 local 时提示需先在终端 `cd scripts/updater-feed && python3 -m http.server 7878 --bind 127.0.0.1`。

---

## 4. 跨组件模块

### 4.1 `toast.js`（26 行）

极简全局提示：懒创建 `.toast-container`，每次 `toast(message, type)` 生成一个 div，`requestAnimationFrame` 触发进入动画，3 s 后淡出移除。`type` 支持 `info`/`success`/`error`（对应 CSS 左边框颜色）。**替代原生 `alert`**，避免阻塞与体验不一致。

### 4.2 `updater.js`（124 行，全局更新流程单例）

这是前端**唯一**的更新逻辑入口，原因是历史教训：启动静默更新与设置页手动更新曾各写一套，互相抢状态。现在统一为：

- 导出全局 `reactive(updaterState)`：`state`（`idle|checking|available|downloading|installing|latest|error`）、`busy`、`latestVersion`、`notes`、`progress`、`channel`、`channelLabel`。
- `checkUpdate({ manual })`：
  - `manual=false`（启动静默）：失败只 `console.error`；用户点"否"=`localStorage[SKIP_KEY]=version` 记住"跳过此版本"，之后每次启动不再打扰。
  - `manual=true`（设置页）：全程 toast 明示，`install_channel_update` 后 `relaunch()` 重启。
- `selectChannel(id)`：切通道后清掉旧检查结果（避免 A 源的"有新版本"被算到 B 源头上）。
- **进度监听只注册一次**（`ensureProgressListener`，`listening` 标志）：监听 Rust 广播事件 `update-progress`，`progress` 事件按 `downloaded/contentLength` 算百分比、封顶 99，`finished` 事件置 100% 并进入 `installing`。
- 依赖：`invoke`（`check_channel_update`/`install_channel_update`/`get_update_channel`/`set_update_channel`）、`listen`（事件）、`ask`（@tauri-apps/plugin-dialog）、`relaunch`（@tauri-apps/plugin-process）。
- `SKIP_KEY = 'magic-agent.update.skippedVersion'`。
- 注意：`App.vue` 注释明确"启动时不自动检查更新"，检查只在设置页手动触发。

### 4.3 `styles/main.css`（111 行）

- **CSS 变量（`:root`）**：`--bg #0e1320`、`--panel #161d2e`、`--panel2 #1b2438`、`--line #273248`、`--text #e5ebf6`、`--muted #8c98ad`、`--accent #4d7cfe`、`--green #3ecf8e`、`--orange #f5a524`、`--danger #ef5b6e`。
- 深色主题；侧边栏固定 230px，主区 `overflow-y:auto`。
- 覆盖所有页面的类：`.btn`(primary/danger/small)、`.panel`、`.stat-card`、`.apps-row` 网格、`.server-card`、`.terminal-wrap`、`.toast-*`、`.conn-status`、`.delay-*` 等。
- **符号不一致（实现细节）**：`SettingsView.vue` 的 scoped 样式用了 `var(--line)`/`var(--panel2)`/`var(--accent)`/`var(--muted)`（都定义了），但 `ConnectionsView.vue` 用的是 `--color-text-secondary`/`--color-border-secondary`/`--color-border-tertiary`/`--font-mono` 这套**未定义**的变量名；`ServerDashboard.vue` 的 `.bar` 用了 `var(--border, #e5e7eb)`、`.net-kv span` 用了 `var(--muted, #888)`——这些只靠 fallback 兜底。改样式时留意，别误以为它们是全局变量。

---

## 5. Tauri command 调用矩阵

前端所有后端交互都经 `invoke('<command>')`。下表按组件汇总（★ = 该组件核心命令）：

| 组件 | 调用的 command | 用途 |
|---|---|---|
| App.vue | `get_status`、`get_config` | 5 s 轮询 |
| App.vue | `scan_apps` | 后台/按需扫描应用 |
| App.vue | `start_proxy`、`stop_proxy` | 启停内核 |
| App.vue | `set_system_proxy` | 开关系统代理 |
| App.vue | `save_config` | 通用配置保存（含热更新规则） |
| App.vue | `select_ssh_server`、`delete_ssh_server` | SSH 服务器管理 |
| Dashboard | `list_foreign_proxies`、`kill_foreign_proxies` | 第三方代理检测/清理 |
| AppsView | —（经 emit 交由 App 调 `save_config`/`scan_apps`） | |
| DomainRulesView | —（经 emit 交由 App 调 `save_config`） | |
| ServersView | `fetch_subscription` ★、`proxy_api` ★（测延迟） | 订阅与测速 |
| ServerDashboard | `server_metrics` ★ | 远端指标 |
| SshView | `ssh_connect` ★、`ssh_write` ★、`ssh_read` ★、`ssh_disconnect` ★ | 终端会话 |
| SettingsView / updater.js | `get_update_channel`、`set_update_channel`、`check_channel_update`、`install_channel_update` | 更新流程 |

> **共同约定**：控制 API（19091）的请求一律走 Rust 的 `proxy_api` 命令，**前端永远拿不到 mihomo secret**（ConnectionsView、ServersView 的注释都强调了这点）。

---

## 6. 开发时注意事项（前端侧）

1. **不要新增页面级轮询**：全局 5 s 定时器已在 `App.vue`；新增轮询会与它打架，且 `scan_apps` 类重操作会造成卡顿。
2. **加新"可编辑字段"时同步加 dirty 门闩**：否则 5 s 轮询会把用户正在输入的内容冲掉。
3. **保存类操作必须处理 `Err`**：`save_config` 失败会回滚，UI 必须 `toast` 出来。
4. **节点改名必须连带迁移引用**（见 3.4），这是最容易漏的隐性契约。
5. **终端/长驻组件必须在 `onUnmounted` 释放资源**（定时器、IPC 连接、xterm 实例）。
6. 无路由、无状态库——保持这个简单性，别为 8 个页面引入 Router/Pinia。

---

← [02-backend-rust.md](./02-backend-rust.md)　|　返回 [README](./README.md)　|　下一篇 → [04-mcp-server.md](./04-mcp-server.md)
