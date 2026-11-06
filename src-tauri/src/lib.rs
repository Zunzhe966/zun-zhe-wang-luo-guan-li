mod apps;
mod auditor;
mod dns_guard;
mod pac;
// config / mihomo 对 bin 工具（dump_conf）公开
pub mod config;
mod keychain;
mod ledger;
pub mod mihomo;
mod ssh;
mod system_proxy;
mod updater;


/// P0-2 信号收尾标志（v0.4.1）：SIGTERM/SIGHUP 到达时置位，由 run() 内的
/// 监听线程轮询后执行与 RunEvent::Exit 相同的收尾（停内核+关系统代理+结账）。
/// SIGKILL 无法拦截，仍靠启动自检（startup_self_check）兜底。
static TERM_SIGNAL_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn handle_term_signal(_: libc::c_int) {
    TERM_SIGNAL_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
}

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Manager;

use crate::config::AppConfig;
use crate::mihomo::{MihomoManager, MihomoStatus};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub proxy_running: bool,
    pub proxy_pid: Option<u32>,
    pub proxy_port: u16,
    pub system_proxy: bool,
    pub apps_count: usize,
    pub nodes_count: usize,
    pub ssh: Option<crate::ssh::SshSession>,
    /// 崩溃自愈提示（P0-4）：启动后发现"内核没跑但系统代理指向本程序端口"
    /// 并已自动关闭时，一次性下发此文案，前端 toast 后调 clear 命令清空。
    pub self_heal_notice: Option<String>,
    /// P1-4 漂移巡检：接管期间（账本 open）外部把系统代理改离接管态时，
    /// 看门狗每 5 分钟抽验一次并置此字段。非一次性——get_status 读出但
    /// 不清空（黄条持续显示），直到用户 [重新归位]（reapply_takeover）或
    /// [接受]（accept_drift）。None = 无漂移或未接管。
    pub drift: Option<DriftNotice>,
    /// P1-4 未结接管账本是否存在（= 接管生效中）。与 MCP status 的 openLedger
    /// 同口径（CONTRACT 双入口一致性），前端据此决定「一键还原」按钮可见性。
    pub open_ledger: bool,
}

/// P1-4 漂移通知：services = 读回不达标（不再指向 127.0.0.1:接管端口）的服务清单。
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DriftNotice {
    pub services: Vec<String>,
    pub ts: u64,
}

/// 代理端口与现有 Clash/FlClash 冲突检测结果
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConflictInfo {
    pub has_conflict: bool,
    pub messages: Vec<String>,
}

/// 第三方代理应用的特征（进程名关键字 → 显示名）。
/// 用于启动网络管理时识别需要清理的"其他代理"。
/// 注意：只用足够具体的名字，绝不用宽泛的 "proxy"，避免误杀无关进程。
const FOREIGN_PROXY_APPS: &[(&str, &str)] = &[
    ("flclash", "FlClash"),
    ("clash verge", "Clash Verge"),
    ("clash-verge", "Clash Verge"),
    ("clashverge", "Clash Verge"),
    ("clash for windows", "Clash for Windows"),
    ("clashx", "ClashX"),
    ("clash-nyanpasu", "Clash Nyanpasu"),
    ("clash.meta", "Clash.Meta"),
    ("v2rayx", "V2RayX"),
    ("v2rayu", "V2RayU"),
    ("qv2ray", "Qv2ray"),
    ("shadowsocksx", "ShadowsocksX"),
    ("shadowsocks-ng", "Shadowsocks-NG"),
    ("surge", "Surge"),
    ("quantumult", "Quantumult"),
    ("stash", "Stash"),
    ("loon", "Loon"),
    ("sing-box", "sing-box"),
    ("singbox", "sing-box"),
    ("trojan", "Trojan"),
    ("naiveproxy", "NaiveProxy"),
    ("hysteria", "Hysteria"),
    ("xray", "Xray"),
    ("v2ray", "V2Ray"),
];

/// 第三方代理的子进程名（内核进程，通常父进程被杀了它们还活着）。
/// 这些是已知代理软件的"内核"可执行名，需要一并清理，否则代理仍在生效。
/// 绝不放入宽泛的 "mihomo"——那会误杀本程序自己的内核。
const FOREIGN_PROXY_CORES: &[&str] = &[
    "flclashcore",
    "clash-verge-service",
    "clash-verge-service-ipc",
    "verge-mihomo",
    "clash-meta",
    "clash-meta-core",
    "sing-box",
    "v2ray-core",
    "xray-core",
    "hysteria",
    "naive",
    "trojan-go",
];

/// 扫描并返回正在运行的第三方代理进程 (pid, 描述) 列表。
/// 跳过本程序自己的进程和 runtime 目录下的内核。
fn find_foreign_proxies() -> Vec<(u32, String)> {
    let mut found: Vec<(u32, String)> = Vec::new();
    let runtime = MihomoManager::new().runtime_dir;
    let runtime_str = runtime.to_string_lossy().to_string();
    let self_pid = std::process::id();

    let ps = match std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,args="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return found,
    };
    let text = String::from_utf8_lossy(&ps.stdout);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let pid: u32 = match parts.next().and_then(|s| s.trim().parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let args = match parts.next() {
            Some(a) => a.trim(),
            None => continue,
        };
        if pid == self_pid {
            continue;
        }
        // 跳过本程序自己的可执行文件
        if args.contains("magic-agent")
            || args.contains("magic_probe")
            || args.contains("dump_conf")
        {
            continue;
        }
        // 跳过本程序 runtime 目录下的内核（那是我们自己的）
        if args.contains(&runtime_str) {
            continue;
        }

        // 只取可执行文件路径部分做匹配（首个空格前的 token）。
        // 绝不用整个命令行匹配：否则任何在参数里提到 "clash"/"v2ray" 字样的进程
        // （grep、编辑器、脚本）都会被误杀。
        let exe_path = args
            .split_whitespace()
            .next()
            .unwrap_or(args)
            .to_lowercase();
        let exe_name = exe_path.rsplit('/').next().unwrap_or(&exe_path).to_string();

        // 匹配应用可执行名（如 /Applications/FlClash.app/Contents/MacOS/FlClash → flclash）
        for (key, label) in FOREIGN_PROXY_APPS {
            if exe_path.contains(key) || exe_name == *key {
                found.push((pid, (*label).to_string()));
                break;
            }
        }
        // 匹配内核进程名
        for core in FOREIGN_PROXY_CORES {
            if exe_name.contains(core) {
                if !found.iter().any(|(p, _)| *p == pid) {
                    found.push((pid, format!("{} 内核", core)));
                }
                break;
            }
        }
    }
    found
}

/// P0-1 统一对账日志：系统代理设置结果不再 `let _ =` 静默吞掉。
/// 全部达标记一行 ok；有失败服务则 WARN 点名到服务级，供排查
/// "显示已开启但某些网络其实没走代理"这类假账场景。
fn log_proxy_set_result(caller: &str, r: &system_proxy::SystemProxyStatus) {
    if r.all_ok {
        eprintln!("[system_proxy] {caller}: 逐服务对账通过（{} 个服务）", r.services.len());
    } else {
        eprintln!(
            "[system_proxy] WARN {caller}: 部分服务未达标: {}（共 {} 个服务，请检查这些网络的代理状态）",
            if r.mismatched.is_empty() { "原因见 services.errors".to_string() } else { r.mismatched.join("、") },
            r.services.len()
        );
    }
}

/// 把系统代理切成 **PAC 白名单模式**（v0.4.2 核心改造）。
///
/// 与旧 `set_system_proxy(true, port)` 的区别：不再把三条全局代理通道指向本机
/// 端口（那等于"劫持整个网络"——内核一死，连 GEOSITE,cn→DIRECT 都执行不到），
/// 而是下发一条 PAC：**名单内的域名走 127.0.0.1:port，且带 `; DIRECT` 回退**，
/// 其余流量物理直连。
///
/// 失败策略：PAC 服务起不来就**不接管**（返回 Err），绝不降级成全局代理——
/// 降级等于把这次事故的根因重新埋回去。
fn apply_pac_mode(
    cfg: &AppConfig,
    port: u16,
    caller: &str,
) -> Result<system_proxy::PacApplyResult, String> {
    let domains = pac::proxy_domains_from_config(cfg);
    let body = pac::build_pac(port, &domains);
    let url = pac::ensure_running(body)?;
    let r = system_proxy::apply_pac(&url)?;
    eprintln!(
        "[pac] {caller}: 白名单 {} 个域名走代理，{} 个网络服务已下发（allOk={}）",
        domains.len(),
        r.services.len(),
        r.all_ok
    );
    if !r.all_ok {
        eprintln!(
            "[pac] WARN {caller}: 未达标服务 {}（原因见 services.errors）",
            if r.mismatched.is_empty() { "见明细".to_string() } else { r.mismatched.join("、") }
        );
    }
    Ok(r)
}

/// 唯一收尾入口：关 PAC + 关三条全局代理 + 清脏地址（`Server: off` 那类）。
/// 自愈、退出、信号、看门狗失败分支都走这里，保证只有一套口径。
fn disable_all_proxy(caller: &str) {
    let errs = system_proxy::disable_everything();
    if errs.is_empty() {
        eprintln!("[system_proxy] {caller}: PAC 与全局代理已全部关闭，系统回到纯直连");
    } else {
        eprintln!(
            "[system_proxy] WARN {caller}: 部分关闭失败（用户可能仍无网，重启 App 可自愈）: {}",
            errs.join("；")
        );
    }
    pac::stop();
}

/// 启动网络管理前，清理所有第三方代理：
/// 1) 杀掉第三方代理进程（先温和 TERM，1.5 秒后仍在则 KILL）
/// 2) 关闭系统代理设置，让网络回到"未设代理"的干净状态
///
/// **P1-2 账本立场（CONTRACT：启动清理 ≠ 接管）**：本函数自身不记账。
/// 记账由【显式接管入口】包裹：start_proxy / kill_foreign_proxies(UI 一键清理)
/// 在调用本函数前 begin_takeover 快照原值、调用后把被杀进程入账。
/// App 启动 800ms 后的自动清理不调账本入口（不是接管，不留 open 会话）。
///
/// 返回清理掉的进程描述列表（供 UI 提示 / 入账）。
fn cleanup_foreign_proxies() -> Vec<String> {
    let victims = find_foreign_proxies();
    let mut cleaned = Vec::new();
    if victims.is_empty() {
        // 没有第三方进程，但仍要确保系统代理是干净状态
        disable_all_proxy("cleanup(无第三方)");
        return cleaned;
    }

    // 第一轮：SIGTERM（温和退出，让代理软件自己清理系统代理设置和防火墙规则）
    for (pid, label) in &victims {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .output();
        cleaned.push(format!("{} (PID {})", label, pid));
    }

    // 等待进程退出，最多 1.5 秒
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // 第二轮：仍在运行的升级为 SIGKILL
    for (pid, _) in &victims {
        let alive = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| !o.stdout.is_empty() && String::from_utf8_lossy(&o.stdout).lines().count() > 1)
            .unwrap_or(false);
        if alive {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .output();
        }
    }

    // 关闭系统代理，回到干净状态（第三方软件可能残留了代理指向）
    disable_all_proxy("cleanup");

    cleaned
}

#[tauri::command]
async fn check_conflicts() -> ConflictInfo {
    // 内部跑 ps + TCP 连接探测，改 async 避免卡主线程
    tauri::async_runtime::spawn_blocking(check_conflicts_blocking)
        .await
        .unwrap_or(ConflictInfo {
            has_conflict: false,
            messages: vec![],
        })
}

fn check_conflicts_blocking() -> ConflictInfo {
    let mut messages = Vec::new();
    // 1) 检测正在运行的第三方代理程序（FlClash / Clash / 外部 mihomo）
    let foreign = find_foreign_proxies();
    if !foreign.is_empty() {
        let names: Vec<String> = foreign.iter().map(|(_, l)| l.clone()).collect();
        messages.push(format!(
            "检测到正在运行的第三方代理程序：{}",
            names.join("、")
        ));
    }
    // 2) 检测本程序要用的混合端口是否已被占用（排除自己的 runtime 内核）
    let port = MihomoManager::new().port;
    if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
        let runtime = MihomoManager::new().runtime_dir;
        let runtime_str = runtime.to_string_lossy().to_string();
        let own = std::process::Command::new("/bin/ps")
            .args(["-axo", "args="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if !own.contains(&runtime_str) {
            messages.push(format!("端口 {} 已被其他程序占用", port));
        }
    }
    ConflictInfo {
        has_conflict: !messages.is_empty(),
        messages,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeoverPlan {
    pub has_sources: bool,
    pub foreign: Vec<String>,
    pub stale_proxy: bool,
    pub summary: String,
}

/// P1-3 归序前体检：只读采集，供确认模式弹窗列示混乱源清单。
/// 不记账不清理，纯给 UI 看"接下来要动什么"。
/// our_pids 必须传真实内核 PID——空数组会让"内核在跑+系统代理指 7891"
/// 的正常状态被误报成死端口残留（kernel_up 判定口径，见 CONTRACT）。
#[tauri::command]
async fn takeover_plan() -> Result<TakeoverPlan, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let m = MihomoManager::new();
        let own_ports: Vec<u16> = vec![m.port, mihomo::PROXY_PORT, mihomo::DIRECT_PORT];
        let our_pids: Vec<u32> = m.find_running_pid().into_iter().collect();
        let report = auditor::audit(&own_ports, &our_pids);
        let foreign = report.foreign_procs.clone();
        let stale = report.stale_proxy.detected;
        Ok(TakeoverPlan {
            has_sources: !foreign.is_empty() || stale,
            foreign,
            stale_proxy: stale,
            summary: report.summary,
        })
    })
    .await
    .map_err(|e| format!("takeover_plan 线程异常: {e}"))?
}

/// P1-2 显式接管入口（带账清理）：用户主动发起的"把系统交给我管"才走这里。
/// 顺序即正确性：先 begin_takeover 快照【动手前】的逐服务原值，再执行清理，
/// 最后把被杀进程入账（不可逆项 reversible=false）。
/// App 启动 800ms 的自动清理不走此入口（启动清理 ≠ 接管，不留 open 会话）。
fn takeover_cleanup(reason: &str) -> Vec<String> {
    let ledger_file = ledger::LedgerFile::default();
    if let Err(e) = ledger_file.begin_takeover(reason, system_proxy::snapshot_system_proxy()) {
        // 记账失败不阻断接管（恢复网络秩序优先），但必须日志点名——静默=没有账
        eprintln!("[ledger] WARN begin_takeover 失败（本次接管不留账）: {e}");
    }
    let cleaned = cleanup_foreign_proxies();
    if let Err(e) = ledger_file.record_killed_procs(&cleaned) {
        eprintln!("[ledger] WARN record_killed_procs 失败: {e}");
    }
    cleaned
}

/// 供前端调用的「一键清理第三方代理」命令：
/// 杀掉所有第三方代理进程 + 关闭系统代理，把系统恢复到干净状态。
#[tauri::command]
async fn kill_foreign_proxies() -> Vec<String> {
    // 杀进程含 SIGTERM→等待→SIGKILL，最多约 1.5 秒，改 async 避免卡主线程
    tauri::async_runtime::spawn_blocking(|| takeover_cleanup("kill_foreign_proxies(UI 一键清理)"))
        .await
        .unwrap_or_default()
}

/// 查询当前有哪些第三方代理在运行（供 UI 展示）。
#[tauri::command]
async fn list_foreign_proxies() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(|| {
        find_foreign_proxies()
            .into_iter()
            .map(|(p, l)| format!("{} (PID {})", l, p))
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// P1-1 网络体检：只读采集本机网络秩序现状（第三方代理/端口冲突/残留/路由/DNS/PAC）。
/// 内部串行跑多个子进程（lsof 2.5s + route 2s + scutil 2s…），最坏 ~10 秒，
/// 必须 async + spawn_blocking（CONTRACT 主线程红线）。
/// MihomoManager::new() 是轻量构造（与 check_conflicts 同款用法），不依赖 managed state。
#[tauri::command]
async fn audit_network() -> Result<auditor::NetworkAuditReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mgr = MihomoManager::new();
        let own_ports: Vec<u16> = vec![mgr.port, mihomo::PROXY_PORT, mihomo::DIRECT_PORT];
        let our_pids: Vec<u32> = mgr.find_running_pid().into_iter().collect();
        auditor::audit(&own_ports, &our_pids)
    })
    .await
    .map_err(|e| format!("audit_network 线程异常: {e}"))
}

#[tauri::command]
async fn fetch_subscription(url: String) -> Result<Vec<crate::config::ProxyNode>, String> {
    // 必须是 async：内部用 curl 拉订阅（最长 15 秒），同步命令会卡死主线程。
    tauri::async_runtime::spawn_blocking(move || fetch_subscription_blocking(url))
        .await
        .map_err(|e| format!("fetch_subscription 线程异常: {e}"))?
}

fn fetch_subscription_blocking(url: String) -> Result<Vec<crate::config::ProxyNode>, String> {
    // 只允许 http/https，防止 curl 访问 file:// 等本地协议造成敏感信息外泄
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("订阅地址必须是 http:// 或 https:// 链接".to_string());
    }
    // SSRF 防护：解析出 host 并拒绝回环/内网/链路本地/组播地址，
    // 防止恶意前端借本命令拉取内网内容（如 127.0.0.1 服务、192.168.x 设备）。
    if let Some(rest) = trimmed.split_once("://").map(|(_, r)| r) {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority
            .rsplit_once('@') // 去掉 userinfo（user:pass@host）
            .map(|(_, h)| h)
            .unwrap_or(authority);
        if let Err(e) = crate::config::validate_public_host(host) {
            return Err(e);
        }
    }
    // 用系统 curl 拉取订阅内容
    let out = std::process::Command::new("/usr/bin/curl")
        .arg("-sL")
        .arg("--max-time")
        .arg("15")
        // 拉订阅必须走真实链路，不受环境代理变量（HTTP_PROXY 等）劫持
        .arg("--noproxy")
        .arg("*")
        .arg("-A")
        .arg("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)")
        .arg(trimmed)
        .output()
        .map_err(|e| format!("调用 curl 失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "拉取订阅失败: {}",
            if err.is_empty() {
                "HTTP 错误".to_string()
            } else {
                err
            }
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    crate::config::parse_vless_subscription(&text)
}

struct AppState {
    config: AppConfig,
    mihomo: MihomoManager,
    /// 已安装 App 列表缓存（scan_apps 时更新，get_status 只读长度，避免重扫描卡界面）
    apps_cache: Mutex<Vec<crate::apps::AppEntry>>,
    /// 用户是否期望代理在运行（start_proxy 置 true，stop_proxy 置 false）。
    /// 看门狗线程据此判断 mihomo 崩溃后是否需要自动重启。
    should_run: Arc<AtomicBool>,
    /// 崩溃自愈一次性提示（P0-4）：启动自检发现"死端口仍被系统代理指向"
    /// 并自动关闭后写入，get_status 读出置 None（前端 toast 一次即消费）。
    /// Arc 包装：启动自检线程与 AppState 共享同一槽位。
    self_heal_notice: Arc<Mutex<Option<String>>>,
    /// P1-4 漂移通知（看门狗写，get_status 读）：Some = 接管期间系统代理被
    /// 外部改离接管态。Arc 包装：看门狗线程与 AppState 共享同一槽位。
    drift: Arc<Mutex<Option<DriftNotice>>>,
    /// P1-4 用户已接受漂移（点 [接受现状]）：true 后看门狗停止巡检，
    /// 直到下次 start_proxy 接管重新归位为 false。
    drift_ack: Arc<AtomicBool>,
}

/// P0-4 启动自检（崩溃自愈）：App 被 kill -9 / 断电时 RunEvent::Exit 不执行，
/// 系统代理可能残留指向本程序死端口（7891/7892/7893）→ 整机断网且用户不明所以。
/// 启动时若发现"内核不在跑但系统代理仍指向这三个端口之一"，立即关闭系统代理
/// 恢复直连，并把情况写入一次性提示。
/// 端口集合来自常量而非配置，任何情况下不会误关"指向其他代理"的系统代理。
fn startup_self_check(state: &Arc<Mutex<AppState>>) {
    let (mihomo, heal_slot) = {
        let g = state.lock().unwrap();
        (g.mihomo.clone(), g.self_heal_notice.clone())
    };
    if mihomo.status().running {
        return; // 内核活着（自动启动马上会接管），不是残留场景
    }
    let sys = system_proxy::status();
    if !sys.enabled {
        return; // 系统代理本来就没开
    }
    let ours = [
        mihomo.port,
        crate::mihomo::PROXY_PORT,
        crate::mihomo::DIRECT_PORT,
    ];
    if !ours.contains(&sys.http_port) && !ours.contains(&sys.socks_port) {
        // 指向的是别人的端口——那是第三方代理的状态，不归本自检管
        // （cleanup_foreign_proxies 会按既有策略处理），绝不能误关他人配置。
        return;
    }
    eprintln!(
        "[self-check] 检测到系统代理指向本程序死端口（http={} socks={}）但内核未运行，自动关闭以恢复直连",
        sys.http_port, sys.socks_port
    );
    let errs = system_proxy::disable_everything();
    pac::stop();
    let notice = format!(
        "检测到上次异常退出残留的代理设置（指向已停止的端口 {}），已自动关闭恢复直连。",
        if sys.http_port != 0 { sys.http_port } else { sys.socks_port }
    );
    *heal_slot.lock().unwrap() = Some(if errs.is_empty() {
        notice
    } else {
        format!("{notice}但以下项目未能确认关闭：{}，请手动检查网络设置。", errs.join("；"))
    });
}

/// P1-4 漂移判定（纯函数，供单测直接喂假读回结果）：
/// 接管生效中（账本 open 且含 system_proxy 项）时，逐服务读回应当
/// "全部达标指向 127.0.0.1:接管端口"。读回不达标（含被关掉、被改指向、
/// 读写异常）的服务清单非空 = 外部改动 detected。
/// 返回漂移服务清单；无漂移返回 None。
/// 口径说明：expect_on 恒为 true——巡检只在"接管生效中"跑，此时系统代理
/// 应当开着并指向本程序端口；内核崩溃场景由既有看门狗（重启/关代理恢复直连）
/// 负责，本巡检绝不与其抢处置权。
fn judge_drift(mismatched: &[String], drift_ack: bool) -> Option<Vec<String>> {
    if drift_ack || mismatched.is_empty() {
        None
    } else {
        Some(mismatched.to_vec())
    }
}

/// P1-4 漂移抽验（看门狗每 5 分钟调用）：只跑一次逐服务读回对账，
/// 不写任何状态、不杀任何进程——发现漂移只记录，处置权在用户
/// （黄条 [重新归位] / [接受]）。
/// PAC 漂移抽验（看门狗每 5 分钟调用）：接管生效中，每个服务都应当
/// "PAC 已启用且 URL 指向本程序的 PAC 服务"。只读不写，发现漂移仅记录。
///
/// v0.4.2 口径变更：不再用 `verify_system_proxy(expect_on=true)`（那是"全局代理
/// 指向本程序端口"的旧口径）。PAC 模式下全局代理本来就应当是关的，
/// 继续用旧口径会把正常状态误报成漂移。
fn probe_drift_pac(drift_ack: bool) -> Option<Vec<String>> {
    let services = system_proxy::list_services();
    if services.is_empty() {
        return None; // 枚举不了服务 = 无从判定，宁可不报也不瞎报
    }
    let mut mismatched: Vec<String> = Vec::new();
    for svc in &services {
        let (enabled, url) = system_proxy::pac_state_for(svc);
        let ok = enabled && !url.is_empty();
        if !ok {
            mismatched.push(svc.clone());
        }
    }
    // 去重保序（防御性处理）
    mismatched.dedup();
    judge_drift(&mismatched, drift_ack)
}

/// 【DNS 体检】只读。回答"这台机器现在到底能不能把域名解析成 IP"，
/// 以及"是不是有服务被手工写死了 DNS"（015 事故的直接诱因）。
#[tauri::command]
async fn check_dns() -> Result<dns_guard::DnsHealth, String> {
    tauri::async_runtime::spawn_blocking(dns_guard::check)
        .await
        .map_err(|e| format!("DNS 体检线程异常: {e}"))
}

/// 【修复 DNS】把手工写死的 DNS 改回 `Empty`（重新继承 DHCP 下发）。
/// 只做这一件事、只改被写死的服务、并把原值回报给 UI——绝不静默改系统配置。
/// 改完立刻复检一次，把"修好了没有"如实告诉调用方。
#[tauri::command]
async fn repair_dns() -> Result<String, String> {
    let (done, before) = tauri::async_runtime::spawn_blocking(|| dns_guard::repair_to_dhcp(None))
        .await
        .map_err(|e| format!("修复 DNS 线程异常: {e}"))??;
    // 给 configd 一点时间重新下发（真机实测 300ms 足够，读回才有意义）
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = tauri::async_runtime::spawn_blocking(dns_guard::check)
        .await
        .map_err(|e| format!("复检线程异常: {e}"))?;
    if done.is_empty() {
        return Ok(format!(
            "没有检测到被手工写死的 DNS（{}）。当前解析状态：{}",
            "各服务均继承 DHCP",
            after.note
        ));
    }
    let detail = before
        .iter()
        .map(|(svc, old)| format!("{}（原值 {}）", svc, if old.is_empty() { "空".to_string() } else { old.join("/") }))
        .collect::<Vec<_>>()
        .join("、");
    Ok(format!(
        "已把 {} 的 DNS 改回 DHCP 自动获取。复检：{}",
        detail,
        if after.ok { format!("解析已恢复（{}）", after.servers.join("、")) } else { after.note }
    ))
}

#[tauri::command]
async fn get_status(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    ssh: tauri::State<'_, crate::ssh::SshManager>,
) -> Result<AppStatus, String> {
    // 同步命令跑主线程：内部 mihomo.status() 会 TcpStream::connect 探端口、
    // system_proxy::status() 会 fork 子进程跑 scutil。被前端每 5 秒轮询，
    // 任一环节慢（端口被防火墙 DROP / 子进程调度）都会让 UI 卡顿。
    // 改 async + spawn_blocking，探测挪到线程池。
    let (mihomo_state, apps_count, nodes_count, heal, drift) = {
        let g = state.lock().unwrap();
        let mihomo = g.mihomo.clone();
        let apps_count = g.apps_cache.lock().unwrap().len();
        let nodes_count = g.config.nodes.len();
        // 一次性消费崩溃自愈提示（读取即清空，前端 toast 一次）
        let heal = g.self_heal_notice.lock().unwrap().take();
        // P1-4 漂移：读出但【不清空】——黄条要持续显示直到用户处置
        let drift = g.drift.lock().unwrap().clone();
        (mihomo, apps_count, nodes_count, heal, drift)
    };
    // SshManager 内部全 Arc，直接 clone（clone 与 State 生命周期解耦）
    let ssh: crate::ssh::SshManager = ssh.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let m = mihomo_state.status();
        let sys = system_proxy::status();
        AppStatus {
            proxy_running: m.running,
            proxy_pid: m.pid,
            proxy_port: m.port,
            system_proxy: sys.enabled,
            apps_count,
            nodes_count,
            ssh: ssh.status(),
            self_heal_notice: heal,
            drift,
            // P1-4：接管是否生效中（读账本，与 MCP status.openLedger 同口径）。
            // 账本是个位 KB 的小文件，5 秒轮询一次可接受。
            open_ledger: ledger::LedgerFile::default().open_session().is_some(),
        }
    })
    .await
    .map_err(|e| format!("get_status 线程异常: {e}"))
}

#[tauri::command]
fn get_config(state: tauri::State<Arc<Mutex<AppState>>>) -> AppConfig {
    let mut cfg = state.lock().unwrap().config.clone();
    // 安全：apiSecret 绝不下发到前端 JS。前端不直接连 mihomo API，
    // 统一走 proxy_api 命令由 Rust 后端持有 secret 转发，防止 XSS 窃取控制密钥。
    cfg.api_secret = None;
    cfg
}

/// 前端通过本命令访问 mihomo 控制 API，secret 由后端持有并注入，
/// 前端 JS 永远拿不到 secret，也无法直接 fetch 19091（CSP 已限制）。
/// path 形如 "/connections" 或 "/proxies/<name>/delay?timeout=5000&url=..."。
/// method 目前支持 "GET"（默认）与 "PUT"。
/// 返回 (http_status, body_string)。
#[tauri::command]
async fn proxy_api(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    path: String,
    method: Option<String>,
    body: Option<String>,
) -> Result<(u16, String), String> {
    // 同步命令会卡主线程：内部是 TCP 直连 mihomo API（read timeout 15s），
    // 且被前端高频调用（延迟测试/连接列表轮询）。改 async + 线程池。
    let secret = {
        let g = state.lock().unwrap();
        g.config.api_secret.clone().unwrap_or_default()
    };
    tauri::async_runtime::spawn_blocking(move || proxy_api_blocking(secret, path, method, body))
        .await
        .map_err(|e| format!("proxy_api 线程异常: {e}"))?
}

fn proxy_api_blocking(
    secret: String,
    path: String,
    method: Option<String>,
    body: Option<String>,
) -> Result<(u16, String), String> {
    // 路径必须以 / 开头，防止被拼成完整 URL（如 http://attacker.com）
    // 但 mihomo delay 接口的 query 参数 url=http://... 含 ://，必须放行：
    // 只检查问号前的路径段不含 ://，query 段允许含 ://。
    if !path.starts_with('/') {
        return Err("非法的 API 路径".to_string());
    }
    let path_part = path.split('?').next().unwrap_or("");
    if path_part.contains("://") {
        return Err("非法的 API 路径".to_string());
    }
    // 拒绝换行：path 直接拼进 HTTP 请求行，含 \r\n 会注入额外请求头/请求走私
    if path.contains('\r') || path.contains('\n') {
        return Err("非法的 API 路径".to_string());
    }
    // secret 由 async 包装从 state 读出后传入（见 proxy_api）
    let method = method.unwrap_or_else(|| "GET".to_string()).to_uppercase();
    if method != "GET" && method != "PUT" {
        return Err("不支持的方法".to_string());
    }
    let body = body.unwrap_or_default();
    // 通过 TcpStream 直连 127.0.0.1:19091 转发，secret 只在后端内存/本地传递
    let addr = ("127.0.0.1", crate::mihomo::API_PORT);
    let mut stream =
        std::net::TcpStream::connect(addr).map_err(|e| format!("无法连接代理控制 API：{e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;

    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {secret}\r\nConnection: close\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
        method = method,
        path = path,
        port = crate::mihomo::API_PORT,
        secret = secret,
        len = body.len(),
        body = body,
    );
    use std::io::{Read, Write};
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).map_err(|e| e.to_string())?;

    // 解析状态码与 body（按字节解析，避免 from_utf8_lossy 对非法字节的替换
    // 导致 chunked 长度切片错位）
    let (status, body_str) = parse_http_response(&resp);
    Ok((status, body_str))
}

/// 极简 HTTP 响应解析：返回 (状态码, body)。
/// 按字节解析，正确处理两种编码：
///   - Content-Length：按长度截取 body；
///   - Transfer-Encoding: chunked：逐个 chunk 块解码（mihomo 控制 API 对大响应
///     如 /connections 必然走 chunked，旧实现用 split("\r\n\r\n") 会把 chunk
///     长度块 25ca\r\n 混进 body，导致前端 JSON.parse 失败）。
/// body 仅在最后一步才从字节转 String（lossy），因此 chunk 长度按字节切分不会
/// 因中文字符或非法字节替换而错位。
fn parse_http_response(raw: &[u8]) -> (u16, String) {
    let status = raw
        .split(|&b| b == b'\n')
        .next()
        .and_then(|l| {
            let l = String::from_utf8_lossy(l);
            l.split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
        })
        .unwrap_or(0);

    // 拆出头与体（HTTP 头以 \r\n\r\n 结束；容忍 \n\n 的变体）
    let (head, body_raw): (&[u8], &[u8]) = if let Some(i) = find_subslice(raw, b"\r\n\r\n") {
        (&raw[..i], &raw[i + 4..])
    } else if let Some(i) = find_subslice(raw, b"\n\n") {
        (&raw[..i], &raw[i + 2..])
    } else {
        (&raw[..0], &raw[..0])
    };
    let head_lower = String::from_utf8_lossy(head).to_ascii_lowercase();

    // chunked 编码：逐块解码，块格式为 "<hex长度>\r\n<数据>\r\n"，以 "0\r\n\r\n" 结束
    if head_lower.contains("transfer-encoding: chunked") {
        let mut out: Vec<u8> = Vec::new();
        let mut rest = body_raw;
        loop {
            // 取长度行（十六进制，可能带 chunk 扩展，用分号分隔）
            let Some(nl) = find_subslice(rest, b"\r\n") else {
                break;
            };
            let size_str = String::from_utf8_lossy(&rest[..nl]);
            let size_str = size_str.split(';').next().unwrap_or("").trim();
            let Ok(size) = usize::from_str_radix(size_str, 16) else {
                break;
            };
            rest = &rest[nl + 2..];
            if size == 0 {
                break; // 终止块
            }
            if rest.len() < size {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size..];
            // 跳过块尾的 \r\n
            if rest.starts_with(b"\r\n") {
                rest = &rest[2..];
            }
        }
        return (status, String::from_utf8_lossy(&out).to_string());
    }

    // Content-Length：按长度精确截取
    if let Some(cl) = head_lower
        .lines()
        .find(|l| l.trim_start().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        let n = cl.min(body_raw.len());
        return (status, String::from_utf8_lossy(&body_raw[..n]).to_string());
    }

    // 兜底：无显式长度，直接返回整个 body
    (status, String::from_utf8_lossy(body_raw).to_string())
}

/// 在字节切片中查找子切片，返回起始索引。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tauri::command]
async fn save_config(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    config: AppConfig,
) -> Result<AppConfig, String> {
    // 同步命令会卡主线程：热更新要调 mihomo API（秒级）。改 async + spawn_blocking。
    let (api_secret, running, apps_cache) = {
        let g = state.lock().unwrap();
        let secret = g.config.api_secret.clone();
        let running = g.mihomo.status().running;
        let cache = g.apps_cache.lock().unwrap().clone();
        (secret, running, cache)
    };
    let mut config = config;
    // 前端拿不到 apiSecret（get_config 已置空），这里必须保留后端持有的原 secret，
    // 否则每次保存都会把 secret 覆盖成 None，导致控制 API 鉴权失效。
    // 防御空串：前端若传 apiSecret: ""（空串而非 null），is_none 判 false 不会补回，
    // secret 会被空串覆盖导致鉴权失效。None 或空串都视为「未提供」，补回旧 secret。
    let secret_missing = config
        .api_secret
        .as_deref()
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if secret_missing {
        config.api_secret = api_secret;
    }
    // 安全：SSH 明文密码/私钥内容绝不落盘 config.json。
    // 密码只存 Keychain；私钥内容只存 Keychain，config 里至多保留私钥「路径」。
    config.ssh_password = None;
    if config
        .ssh_private_key
        .as_deref()
        .map(|k| k.contains('\n'))
        .unwrap_or(false)
    {
        config.ssh_private_key = None;
    }
    // 如果代理正在运行，热更新 rules，让新保存的分流/域名规则立即生效（不重启、不弹授权框）。
    // 慢操作（规则生成 + mihomo API 调用）放线程池，界面不卡。
    let reload_err = if running {
        let cfg_for_reload = config.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let app_rules = effective_app_rules_with(&cfg_for_reload, Some(&apps_cache));
            let m = MihomoManager::new();
            m.reload_rules(&cfg_for_reload, &app_rules).err()
        })
        .await
        .map_err(|e| format!("save_config 线程异常: {e}"))?
    } else {
        None
    };
    state.lock().unwrap().config = config.clone();
    config::save(&config)?;
    if let Some(e) = reload_err {
        // 热更新失败不阻塞保存，但要把错误返回给前端提示
        return Err(format!("配置已保存，但规则热更新失败：{}", e));
    }
    // 返回给前端的 config 同样不能带 secret
    config.api_secret = None;
    Ok(config)
}

#[tauri::command]
async fn scan_apps(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<Vec<crate::apps::AppEntry>, String> {
    // 同步命令会卡主线程：全盘扫描 App + lsof（秒级）。改 async + 线程池。
    let settings = {
        let g = state.lock().unwrap();
        g.config.apps.clone()
    };
    let list = tauri::async_runtime::spawn_blocking(move || {
        let mut list = crate::apps::scan_macos_apps();
        for app in list.iter_mut() {
            if let Some(setting) = settings.iter().find(|s| s.id == app.id) {
                app.mode = setting.mode.clone();
                app.confirmed = setting.confirmed;
                app.node = setting.node.clone();
            }
        }
        list
    })
    .await
    .map_err(|e| format!("scan_apps 线程异常: {e}"))?;
    // 更新缓存，供 get_status 轻量读取数量 / start_proxy 复用
    state
        .lock()
        .unwrap()
        .apps_cache
        .lock()
        .unwrap()
        .clone_from(&list);
    Ok(list)
}

/// 生成生效的软件分流规则：只处理用户已确认（confirmed）的条目。
/// 返回 (路径前缀列表, 目标) 列表；目标 = DIRECT | NODE-<节点名> | PROXY(当前选中节点)。
///
/// 性能：优先复用 AppState.apps_cache，避免每次保存配置/启动代理都全盘扫描 App
/// （scan_macos_apps 含全盘扫描 + lsof，秒级；缓存为空时才回退真扫描）。
fn effective_app_rules(config: &AppConfig) -> Vec<(Vec<String>, String)> {
    effective_app_rules_with(config, None)
}

/// 带可选缓存版本：cached 为 Some 时直接用（不再扫描）。
fn effective_app_rules_with(
    config: &AppConfig,
    cached: Option<&[crate::apps::AppEntry]>,
) -> Vec<(Vec<String>, String)> {
    let mut by_id = std::collections::HashMap::new();
    match cached {
        Some(list) if !list.is_empty() => {
            for a in list {
                by_id.insert(a.id.clone(), a.rule_paths.clone());
            }
        }
        _ => {
            for a in crate::apps::scan_macos_apps() {
                by_id.insert(a.id, a.rule_paths);
            }
        }
    }
    // 现存节点名集合：软件分流的 node 引用已删除节点时降级为 PROXY
    let valid_nodes: std::collections::HashSet<String> =
        config.nodes.iter().map(|n| n.name.clone()).collect();
    settings_to_app_rules(&config.apps, &by_id, &valid_nodes)
}

/// 纯函数版规则生成（供 effective_app_rules 和 dump_conf bin 共用）。
/// bin-<绝对路径> 直接用 id 后半段；app-<Name> 查 path_lookup（App 扫描结果）。
pub fn settings_to_app_rules(
    settings: &[crate::config::AppSetting],
    path_lookup: &std::collections::HashMap<String, Vec<String>>,
    valid_nodes: &std::collections::HashSet<String>,
) -> Vec<(Vec<String>, String)> {
    let mut out = Vec::new();
    for setting in settings {
        if !setting.confirmed {
            continue; // 未确认的软件不进规则表，由 MATCH,DIRECT 兜底
        }
        let paths = if let Some(bin) = setting.id.strip_prefix("bin-") {
            vec![bin.to_string()]
        } else {
            match path_lookup.get(&setting.id) {
                Some(p) => p.clone(),
                None => continue,
            }
        };
        let target = if setting.mode == "proxy" {
            match &setting.node {
                // 节点名已被删除则降级为 PROXY，避免引用不存在的 NODE-xxx 组
                // 用与组名定义处一致的 sanitize_node_name，保证组名与引用一致（中文保留、逗号剔除）
                Some(n) if !n.trim().is_empty() && valid_nodes.contains(n.trim()) => {
                    format!("NODE-{}", crate::mihomo::sanitize_node_name(n.trim()))
                }
                _ => "PROXY".to_string(),
            }
        } else {
            "DIRECT".to_string()
        };
        out.push((paths, target));
    }
    out
}

#[tauri::command]
async fn start_proxy(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<MihomoStatus, String> {
    // 同步命令会卡死主线程（启动可达数十秒：停旧进程 + cleanup + 等 API）。
    // 改为 async + spawn_blocking，主线程立即返回，界面不卡。
    let (cfg, already_running, apps_cache) = {
        let g = state.lock().unwrap();
        let cfg = g.config.clone();
        let running = g.mihomo.status().running;
        let cache = g.apps_cache.lock().unwrap().clone();
        (cfg, running, cache)
    };
    if already_running {
        // P1-A 接管必开入口：内核在跑（App 重启/看门狗重启）时，流量入口（系统代理）
        // 必须开着。TUN 不承载流量（auto-route=false 红线冻结），系统代理是引擎唯一
        // 流量入口——入口关着 = 内核空转，正是"点了启动却看不到真实连接"的根因。
        let m = MihomoManager::new();
        // P0-1：不吞失败——逐服务对账结果如实记日志。v0.4.2：改为 PAC 白名单下发。
        if let Err(e) = apply_pac_mode(&cfg, m.port, "start(已运行)") {
            eprintln!("[start_proxy] PAC 下发失败: {e}");
        }
        let status = m.status();
        // 意图对齐落盘：接管生效即入口在开，纠正历史 false 意图（与漂移巡检口径一致）；
        // 同时清 P2-1 双引擎停用仲裁标记（MCP 侧可能曾置 userStopped=true）
        let need_persist = {
            let mut g = state.lock().unwrap();
            if !g.config.system_proxy {
                g.config.system_proxy = true;
                true
            } else { false }
        };
        let us = {
            let mut g = state.lock().unwrap();
            if g.config.user_stopped {
                g.config.user_stopped = false;
                true
            } else { false }
        };
        if need_persist || us {
            let cfg_now = state.lock().unwrap().config.clone();
            if let Err(e) = config::save(&cfg_now) {
                eprintln!("[start_proxy] WARN 意图落盘失败: {e}");
            }
        }
        if need_persist {
            let cfg_now = state.lock().unwrap().config.clone();
            if let Err(e) = config::save(&cfg_now) {
                eprintln!("[start_proxy] WARN 意图落盘失败: {e}");
            }
        }
        return Ok(status);
    }
    // 启动的慢操作整体挪到线程池
    let status = tauri::async_runtime::spawn_blocking(move || -> Result<MihomoStatus, String> {
        // 前置检查放在接管之前：无节点时根本不该动系统（原顺序会先清屏再报"没节点"）。
        // 无节点时 mihomo 的 fallback 组 proxies 为空，配置校验会失败且错误晦涩。
        if cfg.nodes.is_empty() {
            return Err("尚未添加任何代理节点，请先到「云服务器」页添加节点或拉取订阅".to_string());
        }
        // ── v0.4.2 DNS 闸门（015 事故整改）──
        // 事故现场：Wi-Fi 的 DNS 被钉死在一个不应答的地址上，系统"已连接"但
        // 所有域名都解析不出来。此时若继续接管，会把一层故障叠成两层
        // （DNS 坏 + 代理指死端口），用户看到的就是"整机没网"。
        // 立场：DNS 不健康时**拒绝接管**，并如实说明原因与修复入口。
        let dns = crate::dns_guard::check();
        if !dns.ok {
            return Err(format!(
                "接管已中止：{}。请先在总览页点「修复 DNS」（把手工写死的 DNS 改回 DHCP 自动获取），或手动检查 系统设置 → 网络 → DNS。",
                dns.note
            ));
        }
        eprintln!(
            "[start_proxy] DNS 体检通过（{}）；手工钉死的服务: {}",
            dns.servers.join("、"),
            if dns.manual_services.is_empty() { "无".to_string() } else { dns.manual_services.join("、") }
        );

        // 用户点「启动代理」= 显式接管：先记账（快照逐服务原值）再清理第三方代理
        // （杀 FlClash/Clash 等 + 关系统代理），让系统回到「未设代理」的干净状态，
        // 再启动本程序的代理。
        takeover_cleanup("start_proxy");
        // 复用 App 扫描缓存，避免启动时全盘扫描
        let app_rules = effective_app_rules_with(&cfg, Some(&apps_cache));
        let mihomo = MihomoManager::new();
        let status = match mihomo.start(&cfg, &[], &app_rules) {
            Ok(s) => s,
            Err(e) => {
                // 清理已执行但内核没起来：结账并如实记录，绝不留"幽灵未结账本"
                // （未结账本的语义 = 接管生效中且系统状态已被改变待还原）
                if let Err(le) = ledger::LedgerFile::default().settle_open(&format!("start_proxy 失败：{e}")) {
                    eprintln!("[ledger] WARN 启动失败后结账失败: {le}");
                }
                return Err(e);
            }
        };
        // P1-A 接管必开入口：内核起来后，系统代理（唯一流量入口）必须打开并逐服务
        // 对账达标——不设入口的内核 = 空转摆设（v0.3.1 前按 cfg.system_proxy 可关，
        // 用户因此"点了启动却看不到任何真实连接"）。
        // 不达标 = 半套秩序（部分网络走代理部分裸奔），这正是"报假账"要消灭的
        // 状态——宁可不启：停内核 + 按账本回放系统代理原值 + 结账，并向用户报错。
        // 已知不可逆项如实声明：cleanup 杀掉的第三方进程不会自动复活。
        let verify = match apply_pac_mode(&cfg, mihomo.port, "start") {
            Ok(r) => {
                if r.all_ok {
                    None
                } else {
                    Some(format!(
                        "PAC 下发未全部达标：{} 等 {} 个服务",
                        if r.mismatched.is_empty() { "原因见对账明细".to_string() } else { r.mismatched.join("、") },
                        r.mismatched.len().max(1)
                    ))
                }
            }
            Err(e) => Some(format!("PAC 下发失败：{e}")),
        };
        {
            if let Some(why) = verify {
                eprintln!("[start_proxy] {why}，自动回滚本次接管（宁可不启，不留半套秩序）");
                mihomo.stop();
                // 回滚前先读账本：是否含不可逆 process 项决定提示口径（绝不空喊"进程
                // 不会复活"吓用户，也绝不隐瞒真杀了进程的事实）
                let had_procs = ledger::LedgerFile::default()
                    .open_session()
                    .map(|s| s.entries.iter().any(|e| e.kind == "process"))
                    .unwrap_or(false);
                let (restore_errs, had_ledger) = ledger::LedgerFile::default()
                    .rollback_session("start_proxy 对账不达标，自动回滚")
                    .unwrap_or_else(|e| (vec![format!("账本操作失败：{e}")], false));
                let mut msg = format!("{why}。已自动回滚：内核已停止");
                msg.push_str(if had_ledger {
                    "，系统代理已按账本还原为接管前原值"
                } else {
                    "，但账本无未结账目，系统代理未能还原原值（请手动检查网络设置）"
                });
                if !restore_errs.is_empty() {
                    msg.push_str(&format!("；还原存在失败项：{}", restore_errs.join("；")));
                }
                if had_procs {
                    msg.push_str("。注意：接管时关闭的第三方代理进程不会自动复活，需要的话请手动重开。");
                }
                return Err(msg);
            }
        }
        Ok(status)
    })
    .await
    .map_err(|e| format!("start_proxy 线程异常: {e}"))??;
    // 回到状态（这里只做极短的锁写入）
    if let Some(pid) = status.pid {
        *state.lock().unwrap().mihomo.pid.lock().unwrap() = Some(pid);
    }
    // P1-A：接管入口已开并对账达标 → 意图对齐落盘（纠正历史 false，
    // 与漂移巡检"接管生效中入口必须开"口径一致；失败不回滚，仅告警）
    let need_persist = {
        let mut g = state.lock().unwrap();
        if !g.config.system_proxy {
            g.config.system_proxy = true;
            true
        } else {
            false
        }
    };
    if need_persist {
        let cfg_now = state.lock().unwrap().config.clone();
        if let Err(e) = config::save(&cfg_now) {
            eprintln!("[start_proxy] WARN 意图落盘失败（不影响本次接管）: {e}");
        }
    }
    // 通知看门狗：用户期望代理在运行，mihomo 崩溃后应自动重启
    {
        let mut g = state.lock().unwrap();
        g.should_run.store(true, Ordering::Relaxed);
        // P1-4：新一次接管开始，恢复漂移巡检（清掉上一次的"已接受"与旧黄条）
        g.drift_ack.store(false, Ordering::Relaxed);
        *g.drift.lock().unwrap() = None;
        // P2-1：用户启动 = 明确期望运行，清双引擎停用仲裁标记
        g.config.user_stopped = false;
    }
    let cfg_now = state.lock().unwrap().config.clone();
    if let Err(e) = config::save(&cfg_now) {
        eprintln!("[start_proxy] WARN userStopped 落盘失败: {e}");
    }
    Ok(status)
}

#[tauri::command]
async fn stop_proxy(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> Result<(), String> {
    // 通知看门狗：用户主动停止，不要自动重启
    let port = {
        let g = state.lock().unwrap();
        g.should_run.store(false, Ordering::Relaxed);
        g.mihomo.port
    };
    // P2-1：双引擎停用仲裁——落盘 userStopped=true，MCP 侧 stop 也能阻止看门狗复活
    {
        let mut g = state.lock().unwrap();
        g.config.user_stopped = true;
        let cfg_now = g.config.clone();
        if let Err(e) = config::save(&cfg_now) {
            eprintln!("[stop_proxy] WARN userStopped 落盘失败: {e}");
        }
    }
    // 停进程 + 关系统代理（慢操作）放线程池
    tauri::async_runtime::spawn_blocking(move || {
        let mihomo = MihomoManager::new();
        mihomo.stop();
        let _ = port;
        disable_all_proxy("stop_proxy");
        // P1-2：正常停止 = 接管结束，结账。
        // 注意本步只结账不回放原值——一键还原是 P1-4 的 restore_network，
        // 停止的既有语义（关系统代理回直连）保持不变，不得偷偷扩权。
        match ledger::LedgerFile::default().settle_open("stop_proxy") {
            Ok(true) => eprintln!("[ledger] 接管已结账（stop_proxy）"),
            Ok(false) => {} // 无未结账本（如 MCP 侧已结），幂等正常
            Err(e) => eprintln!("[ledger] WARN settle 失败: {e}"),
        }
    })
    .await
    .map_err(|e| format!("stop_proxy 线程异常: {e}"))?;
    *state.lock().unwrap().mihomo.pid.lock().unwrap() = None;
    Ok(())
}

/// P1-4 一键还原结果（MCP 侧 restore_network 返回同结构，camelCase 契约）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    /// true = 账本有未结接管、系统代理已按 before 原值逐服务回放
    pub restored: bool,
    /// 回放失败项（逐条点名，绝不假装全成）
    pub errors: Vec<String>,
    /// 账本中的不可逆项（接管时关闭的第三方进程——还原救不回它们，如实声明）
    pub irreversible: Vec<String>,
    pub message: String,
}

/// P1-4 一键还原：把网络交还给接管之前的状态。
/// 顺序：通知看门狗停手（防它把内核拉回覆盖还原）→ 停自己内核 →
/// 按账本 before 逐服务回放系统代理原值并结账 → 落意图（config.systemProxy=false，
/// 防下次启动联动又指回 7891）→ 清漂移状态。
/// 无未结账本时不瞎写：退回"关系统代理回直连"的最小安全动作并如实说明。
#[tauri::command]
async fn restore_network(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> Result<RestoreResult, String> {
    // 先通知看门狗：用户要求还原 = 不再期望代理运行（与 stop_proxy 同语义）
    {
        let mut g = state.lock().unwrap();
        g.should_run.store(false, Ordering::Relaxed);
        // P2-1：还原 = 明确停用，落盘仲裁标记防看门狗复活
        g.config.user_stopped = true;
        let cfg_now = g.config.clone();
        if let Err(e) = config::save(&cfg_now) {
            eprintln!("[restore_network] WARN userStopped 落盘失败: {e}");
        }
    }
    let result = tauri::async_runtime::spawn_blocking(|| -> Result<RestoreResult, String> {
        let mihomo = MihomoManager::new();
        mihomo.stop();
        let ledger = ledger::LedgerFile::default();
        // 还原前先读账本：不可逆项清单要在回放（结账）前拿
        let irreversible: Vec<String> = ledger
            .open_session()
            .map(|s| {
                s.entries
                    .iter()
                    .filter(|e| e.kind == "process")
                    .map(|e| e.key.clone())
                    .collect()
            })
            .unwrap_or_default();
        let (errors, had_ledger) = ledger.rollback_session("用户一键还原")?;
        let message = if had_ledger {
            let mut m = String::from("代理内核已停止；系统代理已按接管前账本逐服务还原");
            if !errors.is_empty() {
                m.push_str(&format!("；{} 项还原失败：{}", errors.len(), errors.join("；")));
            }
            if !irreversible.is_empty() {
                m.push_str(&format!(
                    "；{} 个接管时关闭的第三方进程不会自动复活：{}",
                    irreversible.len(),
                    irreversible.join("、")
                ));
            }
            m
        } else {
            // 无账本 = 没有"接管前原值"可回。唯一诚实的动作是退回直连并说明局限。
            let mut notes = Vec::new();
            let errs = system_proxy::disable_everything();
            pac::stop();
            if !errs.is_empty() {
                notes.push(format!("部分服务未能确认关闭：{}", errs.join("；")));
            }
            let mut m = String::from("无未结接管账本（本机未发生过接管或账本已结），代理内核已停止，系统代理已关闭回直连");
            if !notes.is_empty() {
                m.push_str(&format!("；{}", notes.join("；")));
            }
            m.push_str("。若接管前的设置并非直连，请手动恢复");
            m
        };
        Ok(RestoreResult {
            restored: had_ledger,
            errors,
            irreversible,
            message,
        })
    })
    .await
    .map_err(|e| format!("restore_network 线程异常: {e}"))??;
    // 收尾（短临界区）：pid 清空、意图落盘（防启动联动又开系统代理覆盖还原结果）、清漂移
    {
        let mut g = state.lock().unwrap();
        *g.mihomo.pid.lock().unwrap() = None;
        g.config.system_proxy = false;
        if let Err(e) = config::save(&g.config) {
            eprintln!("[restore_network] WARN 意图落盘失败（下次启动可能自动开系统代理）: {e}");
        }
        *g.drift.lock().unwrap() = None;
        g.drift_ack.store(false, Ordering::Relaxed);
    }
    Ok(result)
}

/// P1-4 漂移处置 [重新归位]：接管仍在生效（账本 open）但系统代理被外部改离
/// 接管态时，用户点归位 = 把系统代理恢复到【本次接管声明的秩序】并回读对账。
/// 前提校验一：内核必须在跑——对着死端口设系统代理 = 亲手制造断网，绝不做。
/// 前提校验二（P1-A 更新）：接管生效中的达标态恒为"系统代理开着并指向本程序
/// 端口"——TUN 不承载流量（auto-route=false 红线冻结），系统代理是引擎唯一流量
/// 入口，入口被关 = 内核空转。归位一律回开，不再按历史意图分模式
/// （旧逻辑按 systemProxy=false 归位成"关闭"，把接管态归回了空转态，已废弃）。
#[tauri::command]
async fn reapply_takeover(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> Result<crate::system_proxy::SystemProxyStatus, String> {
    let (port, drift_slot, drift_ack) = {
        let g = state.lock().unwrap();
        let mihomo = g.mihomo.clone();
        if !mihomo.status().running {
            return Err("代理内核未在运行，无法归位（请直接点「启动代理」重新接管）".to_string());
        }
        if ledger::LedgerFile::default().open_session().is_none() {
            return Err("无未结接管账本，系统代理当前不归本程序管辖，拒绝改写".to_string());
        }
        (g.mihomo.port, g.drift.clone(), g.drift_ack.clone())
    };
    let cfg_now = state.lock().unwrap().config.clone();
    let pacres = tauri::async_runtime::spawn_blocking(move || apply_pac_mode(&cfg_now, port, "reapply"))
        .await
        .map_err(|e| format!("reapply_takeover 线程异常: {e}"))??;
    let status = pacres.to_status();
    if status.all_ok {
        *drift_slot.lock().unwrap() = None;
        drift_ack.store(false, Ordering::Relaxed);
    }
    Ok(status)
}

/// P1-4 漂移处置 [接受现状]：外部改动是用户有意为之（比如自己开了别的代理），
/// 停止巡检提示；下次 start_proxy 接管时自动恢复巡检。
#[tauri::command]
fn accept_drift(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> Result<(), String> {
    let g = state.lock().unwrap();
    g.drift_ack.store(true, Ordering::Relaxed);
    *g.drift.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
async fn set_system_proxy(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    enabled: bool,
) -> Result<crate::system_proxy::SystemProxyStatus, String> {
    // networksetup 可能耗时数秒，同步命令会卡主线程 → 改 async + spawn_blocking
    let port = state.lock().unwrap().mihomo.port;
    let cfg_now = state.lock().unwrap().config.clone();
    // v0.4.2：开关语义从「全局代理」改成「PAC 白名单」。
    // 开 = 下发名单（只有名单内域名走代理，其余直连，代理不可用时自动回退直连）；
    // 关 = 统一收尾（关 PAC + 关全局代理 + 清脏地址）。
    let status = tauri::async_runtime::spawn_blocking(move || -> Result<system_proxy::SystemProxyStatus, String> {
        if enabled {
            Ok(apply_pac_mode(&cfg_now, port, "set_system_proxy")?.to_status())
        } else {
            disable_all_proxy("set_system_proxy");
            Ok(system_proxy::SystemProxyStatus {
                enabled: false,
                all_ok: true,
                ..Default::default()
            })
        }
    })
    .await
    .map_err(|e| format!("set_system_proxy 线程异常: {e}"))??;
    // 同步更新并持久化 config.system_proxy，使 UI 开关与 config 一致。
    // 旧实现只调 networksetup 不改 config，导致 UI 开关后 start_proxy 仍按旧 config 判断，
    // 重启后系统代理状态与用户上次选择脱节。
    let cfg = {
        let mut g = state.lock().unwrap();
        g.config.system_proxy = enabled;
        g.config.clone()
    };
    if let Err(e) = config::save(&cfg) {
        eprintln!("[set_system_proxy] 持久化失败（不影响本次设置）: {e}");
    }
    Ok(status)
}

#[tauri::command]
async fn ssh_connect(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    ssh: tauri::State<'_, crate::ssh::SshManager>,
    host: String,
    port: u16,
    user: String,
    auth: String,
    password: Option<String>,
    key: Option<String>,
) -> Result<crate::ssh::SshSession, String> {
    // 连接+认证验证最长十余秒，同步命令会卡死主线程 → 改 async + spawn_blocking。
    // connect 内部已验证登录结果：认证失败会返回 Err，下面的 Keychain/配置写入不会执行，
    // 错误密码/私钥因此永远不会被存进 Keychain 污染后续连接。
    let session = {
        let (h, u, a, p, k) = (
            host.clone(),
            user.clone(),
            auth.clone(),
            password.clone(),
            key.clone(),
        );
        let ssh: crate::ssh::SshManager = ssh.inner().clone(); // clone 共享内部 Arc 状态
        tauri::async_runtime::spawn_blocking(move || ssh.connect(h, port, u, a, p, k))
            .await
            .map_err(|e| format!("ssh_connect 线程异常: {e}"))??
    };

    // 密码/私钥内容安全存入 Keychain，config 不落明文。
    // 注意：SSH 会话此时已经成功建立（ssh.connect 已通过认证）。
    // Keychain 存储是「持久化凭据」的副作用，不能让它把整个 ssh_connect 拉成 Err
    // ——否则用户会看到"连接失败"，但后台 session 已 live，下次 connect 才被 disconnect
    // 清理，期间会变成孤儿会话。Keychain 写失败时只标记 password_saved=false，
    // 让用户在 UI 上看到「已连接但凭据未保存」，而不是误报连接失败。
    let password_saved = if auth == "password" {
        match password {
            Some(p) if !p.trim().is_empty() => {
                match keychain::store(&crate::ssh::SshManager::password_account(&host, &user), &p) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("[ssh] 保存密码到 Keychain 失败（不影响本次连接）: {e}");
                        false
                    }
                }
            }
            _ => keychain::exists(&crate::ssh::SshManager::password_account(&host, &user)),
        }
    } else {
        false
    };
    let private_key_saved = if auth == "key" {
        match key.as_ref() {
            Some(k) if !k.trim().is_empty() => {
                // 如果是路径，不存内容；如果是私钥内容（多行），存 Keychain
                if k.contains('\n') {
                    match keychain::store(&crate::ssh::SshManager::key_account(&host, &user), k) {
                        Ok(_) => true,
                        Err(e) => {
                            eprintln!("[ssh] 保存私钥到 Keychain 失败（不影响本次连接）: {e}");
                            false
                        }
                    }
                } else {
                    false // 路径形式，直接引用路径
                }
            }
            _ => keychain::exists(&crate::ssh::SshManager::key_account(&host, &user)),
        }
    } else {
        false
    };

    // 更新 servers 列表
    let mut g = state.lock().unwrap();
    let id = format!("ssh-{}@{}", user, host);
    let info = crate::config::ServerInfo {
        id: id.clone(),
        name: host.clone(),
        host: host.clone(),
        port,
        user: user.clone(),
        auth: auth.clone(),
        password_saved,
        private_key_saved,
        key_path: key.as_ref().filter(|k| !k.contains('\n')).cloned(),
    };
    if let Some(existing) = g.config.servers.iter_mut().find(|s| s.id == id) {
        *existing = info.clone();
    } else {
        g.config.servers.push(info);
    }
    g.config.active_server_id = Some(id);

    // 兼容旧字段（不再存明文密码）
    g.config.ssh_host = Some(host);
    g.config.ssh_port = Some(port);
    g.config.ssh_user = Some(user);
    g.config.ssh_auth = Some(auth);
    g.config.ssh_password = None;
    g.config.ssh_private_key = None;
    // config 持久化失败不应让已成功的连接变成"失败"（session 已建立），
    // 但也不能静默吞掉——内存有服务器、磁盘没有，重启后消失且 Keychain 留孤儿凭据。
    if let Err(e) = config::save(&g.config) {
        eprintln!("[ssh_connect] config 持久化失败（不影响本次连接）: {e}");
    }
    Ok(session)
}

#[tauri::command]
fn select_ssh_server(
    state: tauri::State<Arc<Mutex<AppState>>>,
    server_id: String,
) -> Result<crate::config::ServerInfo, String> {
    let mut g = state.lock().unwrap();
    let server = g
        .config
        .servers
        .iter()
        .find(|s| s.id == server_id)
        .cloned()
        .ok_or("服务器不存在")?;
    g.config.active_server_id = Some(server.id.clone());
    g.config.ssh_host = Some(server.host.clone());
    g.config.ssh_port = Some(server.port);
    g.config.ssh_user = Some(server.user.clone());
    g.config.ssh_auth = Some(server.auth.clone());
    g.config.ssh_password = None;
    g.config.ssh_private_key = server.key_path.clone();
    config::save(&g.config)?;
    Ok(server)
}

#[tauri::command]
fn delete_ssh_server(
    state: tauri::State<Arc<Mutex<AppState>>>,
    ssh: tauri::State<crate::ssh::SshManager>,
    server_id: String,
) -> Result<(), String> {
    let mut g = state.lock().unwrap();
    let idx = g
        .config
        .servers
        .iter()
        .position(|s| s.id == server_id)
        .ok_or("服务器不存在")?;
    let server = g.config.servers.remove(idx);
    keychain::delete(&crate::ssh::SshManager::password_account(
        &server.host,
        &server.user,
    ));
    keychain::delete(&crate::ssh::SshManager::key_account(
        &server.host,
        &server.user,
    ));
    // 若删的正是当前激活服务器，且 SSH 交互式会话还连着它，必须同步断开——
    // 否则用户删完服务器在「控制台」页依然看到"已连接"，且底层 ssh 进程仍持有
    // 该服务器凭据的会话，与"已删除"语义矛盾，存在凭据残留风险。
    let is_active = g.config.active_server_id.as_deref() == Some(server.id.as_str());
    if is_active {
        // session.id 形如 "ssh-<user>@<host>"，与 server.id 同形，直接比对即可
        let sid = ssh.session.lock().unwrap().as_ref().map(|s| s.id.clone());
        if sid.as_deref() == Some(server.id.as_str()) {
            ssh.disconnect();
        }
    }
    if g.config.active_server_id.as_deref() == Some(server.id.as_str()) {
        if let Some(next) = g.config.servers.first().cloned() {
            g.config.active_server_id = Some(next.id.clone());
            g.config.ssh_host = Some(next.host);
            g.config.ssh_port = Some(next.port);
            g.config.ssh_user = Some(next.user);
            g.config.ssh_auth = Some(next.auth);
            g.config.ssh_password = None;
            g.config.ssh_private_key = next.key_path;
        } else {
            g.config.active_server_id = None;
            g.config.ssh_host = None;
            g.config.ssh_port = Some(22);
            g.config.ssh_user = Some("root".to_string());
            g.config.ssh_auth = Some("password".to_string());
            g.config.ssh_password = None;
            g.config.ssh_private_key = None;
        }
    }
    config::save(&g.config)?;
    Ok(())
}

#[tauri::command]
fn ssh_write(ssh: tauri::State<crate::ssh::SshManager>, data: Vec<u8>) -> Result<(), String> {
    ssh.write(data)
}

#[tauri::command]
fn ssh_read(ssh: tauri::State<crate::ssh::SshManager>) -> Result<Vec<u8>, String> {
    ssh.read()
}

#[tauri::command]
fn ssh_disconnect(ssh: tauri::State<crate::ssh::SshManager>) -> Result<(), String> {
    ssh.disconnect();
    Ok(())
}

/// 在「当前激活的云服务器」上非交互式执行一条命令，返回 (stdout, stderr, exit_code)。
/// 智能体 / 前端仪表盘用来远程探测服务器状态（CPU/内存/磁盘/带宽），不污染交互式终端。
///
/// 注意：必须是 async 命令。Tauri 的同步命令运行在主线程上，SSH 建立连接+执行
/// 最长可达数十秒，会卡死主线程 → macOS 显示"彩色转圈"（应用无响应）。
/// 这里用 spawn_blocking 把阻塞 IO 挪到线程池，主线程立即返回。
#[tauri::command]
async fn ssh_exec(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<(String, String, i32), String> {
    let (host, port, user, auth, key_path) = {
        let g = state.lock().unwrap();
        let s = g
            .config
            .active_server()
            .ok_or("尚未配置云服务器：请先在「云服务器」页添加 SSH 连接")?;
        (
            s.host.clone(),
            s.port,
            s.user.clone(),
            s.auth.clone(),
            s.key_path.clone(),
        )
    };
    let timeout = timeout_secs.unwrap_or(15).clamp(5, 60);
    // 阻塞 IO 放到独立线程，绝不占用主线程
    tauri::async_runtime::spawn_blocking(move || {
        let ssh = crate::ssh::SshManager::new();
        ssh.exec(host, port, user, auth, command, timeout, key_path)
    })
    .await
    .map_err(|e| format!("ssh_exec 线程异常: {e}"))?
}

/// 云服务器一键探针：采集 CPU、内存、磁盘、网络带宽、负载、在线时长。
/// 返回结构化 JSON 给前端仪表盘 / 智能体（MCP 也走同一逻辑）。
///
/// 注意：必须是 async 命令 + spawn_blocking。同步命令跑在主线程上，这个探针
/// 会 SSH 连服务器执行命令（最长 20 秒超时），期间主线程被占 → 点仪表盘时
/// macOS 显示"彩色转圈"（应用无响应）。挪到线程池后交互不再卡顿。
#[tauri::command]
async fn server_metrics(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<serde_json::Value, String> {
    let (server_name, host, port, user, auth, key_path) = {
        let g = state.lock().unwrap();
        let s = g.config.active_server().ok_or("尚未配置云服务器")?;
        (
            s.name.clone(),
            s.host.clone(),
            s.port,
            s.user.clone(),
            s.auth.clone(),
            s.key_path.clone(),
        )
    };
    let cmd = r#"
echo '---CPU---'; top -bn1 | grep 'Cpu(s)' || echo 'n/a'
echo '---MEM---'; free -m | grep -E 'Mem|内存' || echo 'n/a'
echo '---DISK---'; df -h / | tail -1 || echo 'n/a'
echo '---LOAD---'; cat /proc/loadavg 2>/dev/null || sysctl -n vm.loadavg 2>/dev/null || echo 'n/a'
echo '---UPTIME---'; uptime | sed 's/^ *//' || echo 'n/a'
echo '---NET---'; cat /proc/net/dev | grep -E 'eth0|ens|enp' | head -5 || echo 'n/a'
"#;
    tauri::async_runtime::spawn_blocking(move || {
        let ssh = crate::ssh::SshManager::new();
        let (out, err, code) = ssh.exec(
            host.clone(),
            port,
            user.clone(),
            auth,
            cmd.to_string(),
            20,
            key_path,
        )?;
        if code != 0 {
            return Err(format!("探针执行失败 (exit {code}): {err}"));
        }
        Ok(attach_server_identity(
            parse_server_metrics(&out),
            &server_name,
            &host,
            &user,
        ))
    })
    .await
    .map_err(|e| format!("server_metrics 线程异常: {e}"))?
}

/// 解析探针原始输出为结构化 JSON（前端/智能体直接消费）。
fn parse_server_metrics(raw: &str) -> serde_json::Value {
    use serde_json::json;
    let mut m = serde_json::Map::new();

    let mut section = "";
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with("---") && t.ends_with("---") {
            section = t.trim_matches('-').trim();
            continue;
        }
        match section {
            "CPU" => {
                // top: %Cpu(s):  us, sy, ni, id, wa...
                if t.starts_with("Cpu") || t.starts_with("%Cpu") || t.contains("us,") {
                    let id = extract_pct(t, "id");
                    m.insert("cpu_usage_pct".into(), json!(100.0 - id));
                }
            }
            "MEM" => {
                // free -m:  total used free shared buff/cache available
                let cols: Vec<&str> = t.split_whitespace().collect();
                if cols.len() >= 7 {
                    if let (Ok(total), Ok(used), Ok(avail)) = (
                        cols[1].parse::<f64>(),
                        cols[2].parse::<f64>(),
                        cols[6].parse::<f64>(),
                    ) {
                        m.insert("mem_total_mb".into(), json!(total));
                        m.insert("mem_used_mb".into(), json!(used));
                        m.insert("mem_avail_mb".into(), json!(avail));
                        m.insert(
                            "mem_usage_pct".into(),
                            json!((used / total * 100.0 * 10.0).round() / 10.0),
                        );
                    }
                }
            }
            "DISK" => {
                // df -h: Filesystem Size Used Avail Use% Mounted
                let cols: Vec<&str> = t.split_whitespace().collect();
                if cols.len() >= 5 {
                    m.insert("disk_size".into(), json!(cols[1]));
                    m.insert("disk_used".into(), json!(cols[2]));
                    m.insert("disk_avail".into(), json!(cols[3]));
                    m.insert(
                        "disk_usage_pct".into(),
                        json!(cols[4].trim_end_matches('%')),
                    );
                }
            }
            "LOAD" => {
                // loadavg: 0.12 0.09 0.08 1/123 456
                let cols: Vec<&str> = t.split_whitespace().collect();
                if cols.len() >= 3 {
                    m.insert("load_1m".into(), json!(cols[0]));
                    m.insert("load_5m".into(), json!(cols[1]));
                    m.insert("load_15m".into(), json!(cols[2]));
                }
            }
            "UPTIME" => {
                m.insert("uptime".into(), json!(t));
            }
            "NET" => {
                // eth0: 1234 5 0 0 0 0 0 0 5678 9 ...  (RX 累计字节在第1列，TX 在第9列)
                if let Some(colon) = t.find(':') {
                    let ifname = t[..colon].trim().to_string();
                    let nums: Vec<&str> = t[colon + 1..].split_whitespace().collect();
                    if nums.len() >= 10 {
                        let rx = nums[0].parse::<f64>().unwrap_or(0.0);
                        let tx = nums[8].parse::<f64>().unwrap_or(0.0);
                        m.insert(format!("net_{}_rx_bytes", ifname), json!(rx));
                        m.insert(format!("net_{}_tx_bytes", ifname), json!(tx));
                    }
                }
            }
            _ => {}
        }
    }
    m.insert("probe_ok".into(), json!(true));
    serde_json::Value::Object(m)
}

/// 前端/Python extension 的 server_metrics 契约都包含 server 身份对象。
/// 单独抽成纯函数，防止 Rust 侧只返回指标、仪表盘却永远进入不了服务器信息分支。
fn attach_server_identity(
    mut metrics: serde_json::Value,
    name: &str,
    host: &str,
    user: &str,
) -> serde_json::Value {
    if let Some(obj) = metrics.as_object_mut() {
        obj.insert(
            "server".to_string(),
            serde_json::json!({"name": name, "host": host, "user": user}),
        );
    }
    metrics
}

fn extract_pct(line: &str, key: &str) -> f64 {
    // top 的 CPU 行形如 "%Cpu(s):  5.2 us,  3.1 sy,  0.0 ni, 89.8 id,  1.9 wa"
    // 每段是 "<值> <字段名>"，字段名可能带尾逗号。抓 key 前紧邻的浮点数。
    for part in line.split(',') {
        let p = part.trim();
        let mut prev_val: Option<f64> = None;
        for w in p.split_whitespace() {
            let field = w.trim_end_matches(',');
            if field == key {
                if let Some(v) = prev_val {
                    return v;
                }
            }
            prev_val = w
                .trim_end_matches(',')
                .trim_end_matches('%')
                .parse::<f64>()
                .ok();
        }
    }
    0.0
}

#[tauri::command]
fn self_test(state: tauri::State<Arc<Mutex<AppState>>>) -> Result<String, String> {
    let g = state.lock().unwrap();
    let bin = g.mihomo.bin_path_for_test();
    let geo = g.mihomo.geo_dir_for_test();
    let mut lines = Vec::new();
    lines.push(format!("mihomo_bin: {}", bin.display()));
    lines.push(format!("mihomo_exists: {}", bin.exists()));
    lines.push(format!("geo_dir: {}", geo.display()));
    lines.push(format!("geo_dir_exists: {}", geo.exists()));
    Ok(lines.join("\n"))
}

pub fn start_proxy_standalone() -> Result<MihomoStatus, String> {
    let cfg = config::load();
    let mihomo = MihomoManager::new();
    let app_rules = effective_app_rules(&cfg);
    let status = mihomo.start(&cfg, &[], &app_rules)?;
    // P1-A：接管必开入口（与 start_proxy 同语义）——TUN 不承载流量，
    // 系统代理是引擎唯一流量入口，不设入口的内核 = 空转摆设。
    if let Err(e) = apply_pac_mode(&cfg, mihomo.port, "standalone") {
        eprintln!("[standalone] PAC 下发失败: {e}");
    }
    Ok(status)
}

pub fn stop_proxy_standalone() -> Result<(), String> {
    let mihomo = MihomoManager::new();
    // v0.4.1：用户态直接结束内核进程（stop() 内部含 PID 发现与 SIGKILL 兜底，
    // 无需特权控制器 sudoers、无需 osascript 提权——内核是普通用户进程）
    mihomo.stop();
    // v0.4.2：统一收尾（关 PAC + 关全局代理 + 清脏地址）
    disable_all_proxy("standalone");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 登录自愈（v0.4.2）：断电后**不依赖用户手动打开 App**
// ─────────────────────────────────────────────────────────────────────────────

/// 登录自愈 agent 的标签与路径
pub const SELFHEAL_LABEL: &str = "com.magic.agent.selfheal";

fn selfheal_plist_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{SELFHEAL_LABEL}.plist")))
}

/// 无界面自愈：登录后由 launchd 拉起，检查"内核不在但代理/PAC 还开着"，
/// 是则全部关掉。**这是断电重启后网络能自动恢复的唯一保证**——
/// v0.4.1 的自愈只写在 GUI 的启动路径里，用户不点开 App 就永远不执行。
pub fn selfheal() -> i32 {
    let kernel_up = MihomoManager::new().status().running;
    let pac_on = system_proxy::pac_enabled_globally();
    let legacy_on = system_proxy::status().enabled;
    eprintln!(
        "[selfheal] 启动自检：内核运行={kernel_up} PAC启用={pac_on} 全局代理启用={legacy_on}"
    );
    if kernel_up {
        eprintln!("[selfheal] 内核在跑，不动系统设置（接管状态由 App 负责）");
        return 0;
    }
    if !pac_on && !legacy_on {
        eprintln!("[selfheal] 系统无代理/PAC 残留，无需处理");
        return 0;
    }
    eprintln!("[selfheal] 检测到残留（内核已不在），清除以恢复直连");
    let errs = system_proxy::disable_everything();
    if errs.is_empty() {
        eprintln!("[selfheal] 已完成：PAC 与全局代理均已关闭");
        0
    } else {
        eprintln!("[selfheal] 部分失败：{}", errs.join("；"));
        1
    }
}

/// 无界面 DNS 体检（`--dns-check`）：输出 JSON，供脚本/支持排障直接消费。
pub fn dns_check_json() -> String {
    let h = dns_guard::check();
    serde_json::to_string_pretty(&h).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// 无界面对账（`--proxy-status`）：报告当前 PAC / 全局代理现状，只读。
pub fn proxy_status_text() -> String {
    let pac_on = system_proxy::pac_enabled_globally();
    let legacy = system_proxy::status();
    let mut out = vec![
        format!("PAC 启用: {pac_on}"),
        format!("全局代理启用: {}", legacy.enabled),
        format!("HTTP 端口: {} / SOCKS 端口: {}", legacy.http_port, legacy.socks_port),
    ];
    for svc in system_proxy::list_services() {
        let (en, url) = system_proxy::pac_state_for(&svc);
        out.push(format!(
            "  {svc}: PAC={} {}",
            if en { "on" } else { "off" },
            if url.is_empty() { String::new() } else { format!("({url})") }
        ));
    }
    out.join("\n")
}

/// 无界面修 DNS（`--repair-dns`）
pub fn dns_repair_text() -> String {
    match dns_guard::repair_to_dhcp(None) {
        Ok((done, before)) => {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let after = dns_guard::check();
            if done.is_empty() {
                format!("没有手工写死的 DNS。当前：{}", after.note)
            } else {
                format!(
                    "已把 {} 改回 DHCP（原值 {:?}）。复检：{}",
                    done.join("、"),
                    before,
                    after.note
                )
            }
        }
        Err(e) => format!("修复失败: {e}"),
    }
}

/// 安装（或刷新）登录自愈 LaunchAgent。幂等。
/// 只在本 App 自己的用户域运行，不写 /Library，不需要 root。
pub fn install_selfheal_agent() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("取可执行路径失败: {e}"))?;
    let path = selfheal_plist_path().ok_or("取不到用户 home 目录")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 目录失败: {e}"))?;
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{SELFHEAL_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--selfheal</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>/tmp/magic-agent-selfheal.log</string>
  <key>StandardErrorPath</key><string>/tmp/magic-agent-selfheal.log</string>
</dict>
</plist>
"#,
        exe.display()
    );
    std::fs::write(&path, plist).map_err(|e| format!("写 LaunchAgent 失败: {e}"))?;
    // 已加载则先卸载（内容变了需要重新读），忽略失败
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &domain, path.to_string_lossy().as_ref()])
        .output();
    let out = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", &domain, path.to_string_lossy().as_ref()])
        .output()
        .map_err(|e| format!("launchctl 调用失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // 已经加载过（EALREADY/5）不算失败
        if !err.contains("already") && !err.is_empty() {
            return Err(format!("launchctl bootstrap 失败: {err}"));
        }
    }
    Ok(path.display().to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let should_run = Arc::new(AtomicBool::new(false));

    // P0-2 生命周期（v0.4.1）：RunEvent::Exit 只覆盖 GUI 正常退出路径
    // （关窗/Cmd+Q/app.exit()/系统注销）；外部 `kill <pid>` / launchctl kill 发
    // SIGTERM 时 Tauri 不触发 Exit → mihomo 孤儿 + 系统代理残留指向死端口。
    // 注册 libc 信号 handler（handler 只置原子标志，信号安全），监听线程轮询
    // 标志后执行同一收尾逻辑再显式 exit(0)。SIGKILL 无法拦截，由启动自检兜底。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, handle_term_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handle_term_signal as *const () as libc::sighandler_t);
    }
    let state = Arc::new(Mutex::new(AppState {
        config: config::load(),
        mihomo: MihomoManager::new(),
        apps_cache: Mutex::new(Vec::new()),
        should_run: should_run.clone(),
        self_heal_notice: Arc::new(Mutex::new(None)),
        drift: Arc::new(Mutex::new(None)),
        drift_ack: Arc::new(AtomicBool::new(false)),
    }));

    // 信号监听线程：SIGTERM/SIGHUP 置位后执行与 RunEvent::Exit 相同的收尾
    // （先通知看门狗停止 → 停内核 → 关系统代理 → 结账）再显式退出。
    // 与 Exit 路径唯一差异：不做 SSH 断连（信号线程无 app handle；SSH 子进程
    // 随 App 进程终止自然回收，属可接受的极小残留）。
    let signal_state = state.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !TERM_SIGNAL_FLAG.load(Ordering::Relaxed) {
                continue;
            }
            eprintln!("[magic-agent] 收到 SIGTERM/SIGHUP：执行退出收尾");
            let g = signal_state.lock().unwrap_or_else(|e| e.into_inner());
            g.should_run.store(false, Ordering::Relaxed);
            g.mihomo.stop();
            disable_all_proxy("signal-exit");
            match ledger::LedgerFile::default().settle_open("signal_exit") {
                Ok(true) => eprintln!("[ledger] 接管已结账（signal_exit）"),
                Ok(false) => {}
                Err(e) => eprintln!("[ledger] WARN 信号退出结账失败: {e}"),
            }
            std::process::exit(0);
        }
    });

    // v0.4.2：装/刷新登录自愈 agent。断电重启后由它（而不是"用户记得打开 App"）
    // 把残留的 PAC/代理关掉——这是本次事故最重要的一处补齐。
    match install_selfheal_agent() {
        Ok(p) => eprintln!("[selfheal-agent] 已安装/刷新: {p}"),
        Err(e) => eprintln!("[selfheal-agent] WARN 安装失败（断电后需手动打开 App 才能自愈）: {e}"),
    }

    // P0-4 启动自检：崩溃残留的"系统代理指向死端口"会整机断网，
    // 必须第一时间恢复直连并告知用户。同步跑在 setup 前（百毫秒级，
    // 仅一次内核端口探测 + 条件性一次 networksetup 批量关），
    // 放后台线程会与 800ms 后的 cleanup_foreign_proxies 竞态。
    startup_self_check(&state);

    // P1-2 未结账本提示：正常退出/停止都会 settle，账本仍为 open = 上次
    // 接管未经正常结束（kill -9/断电/MCP 侧异常）。如实一次性提示。
    // ⚠️ 只提示不自动回放——回写系统设置属用户决策（P1-4 已交付「一键还原」，
    // 总览页可执行；异常残留场景下用户可能已手动调过网络，自动回放会覆盖之）。
    {
        if let Some(session) = ledger::LedgerFile::default().open_session() {
            let proxy_items = session.entries.iter().filter(|e| e.kind == "system_proxy").count();
            let killed = session.entries.iter().filter(|e| e.kind == "process").count();
            let notice = format!(
                "上次接管（{}，开始于 {}，{} 项系统代理原值在册{}）未正常结账。启动代理或清理时账本继续沿用最初原值；如需归还接管前设置，可在总览页点「一键还原」或手动检查系统代理设置。",
                session.reason,
                session.started_ts,
                proxy_items,
                if killed > 0 { format!("，关闭了 {killed} 个第三方进程（不可逆）") } else { String::new() },
            );
            let slot = state.lock().unwrap().self_heal_notice.clone();
            let mut g = slot.lock().unwrap();
            *g = Some(match g.take() {
                Some(prev) => format!("{prev} {notice}"),
                None => notice,
            });
        }
    }

    // mihomo 看门狗：每 30s 检查一次。用户启动代理后 should_run=true，
    // 若 mihomo 崩溃（端口探测失败）则自动拉起（v0.4.1：用户态直接 start，
    // 无需特权控制器/osascript）。重启失败则关掉系统代理，
    // 避免 mihomo 死了但系统代理仍指向 127.0.0.1:7891 导致全机断网。
    let watchdog_state = state.clone();
    // P1-4 漂移巡检也要读 should_run，先 clone 一份（下一条线程会 move 原件）
    let drift_should_run = should_run.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if !should_run.load(Ordering::Relaxed) {
                continue;
            }
            let mihomo = MihomoManager::new();
            if mihomo.status().running {
                continue;
            }
            // P2-1 双引擎停用仲裁（v0.4.1）：MCP stop_proxy/restore_network 落盘
            // userStopped=true 后，看门狗绝不 30 秒内复活内核——否则从 AI 入口视角
            // "我停了它又活了"= 失控（PID 34747→40591 实锤）。
            if config::load().user_stopped {
                eprintln!("[watchdog] userStopped=true（用户/MCP 已停止），不再自动重启；同步内存态");
                {
                    let g = watchdog_state.lock().unwrap_or_else(|e| e.into_inner());
                    g.should_run.store(false, Ordering::Relaxed);
                }
                continue;
            }
            eprintln!("[watchdog] mihomo 已停止但 should_run=true，尝试自动重启（用户态）...");
            let (cfg, apps_cache) = {
                let g = watchdog_state.lock().unwrap_or_else(|e| e.into_inner());
                let apps = g.apps_cache.lock().unwrap().clone();
                (g.config.clone(), apps)
            };
            let app_rules = effective_app_rules_with(&cfg, Some(&apps_cache));
            match mihomo.start(&cfg, &[], &app_rules) {
                Ok(s) => {
                    eprintln!("[watchdog] mihomo 自动重启成功（用户态）");
                    *watchdog_state.lock().unwrap().mihomo.pid.lock().unwrap() = s.pid;
                    // v0.3.2 修复（用户真机报障：内核在跑但实时连接全空）：
                    // 旧逻辑重启成功就 continue，系统代理无人恢复 →
                    // "内核活着但流量没进隧道"的半套秩序（内核统计 0 字节）。
                    // P1-A 红线口径：接管生效中达标态恒为"入口开着指向本程序端口"，
                    // 不看历史意图（should_run=true 本身即"接管生效中"）。
                    // 且【先记账后动手】（CONTRACT 接管账本红线；begin 幂等——
                    // 接管仍生效时绝不覆盖最初原值）。
                    let port = {
                        let g = watchdog_state.lock().unwrap_or_else(|e| e.into_inner());
                        g.mihomo.port
                    };
                    if let Err(e) = ledger::LedgerFile::default()
                        .begin_takeover("watchdog 恢复系统代理", system_proxy::snapshot_system_proxy())
                    {
                        eprintln!("[watchdog] WARN begin_takeover 失败（联动不设代理，宁可不写）: {e}");
                    } else {
                        let cfg_now = {
                            let g = watchdog_state.lock().unwrap_or_else(|e| e.into_inner());
                            g.config.clone()
                        };
                        match apply_pac_mode(&cfg_now, port, "watchdog 重启联动") {
                            Ok(r) => {
                                if !r.all_ok {
                                    eprintln!("[watchdog] 重启后 PAC 未全部达标，漂移巡检将接力提示");
                                }
                            }
                            Err(e) => eprintln!("[watchdog] WARN 重启后 PAC 下发失败: {e}"),
                        }
                    }
                }
                Err(e) => {
                    // 重启失败：关掉系统代理，避免死代理端口导致全机断网。
                    // P0-1：这里若没关成是真实危险（用户即将断网），必须点名；
                    // 即便本次失败，下次 App 重启时 startup_self_check（P0-4）仍会兜底。
                    eprintln!("[watchdog] mihomo 自动重启失败: {e}，关闭代理设置以恢复直连");
                    disable_all_proxy("watchdog");
                }
            }
            continue;
        }
    });

    // P1-4 漂移巡检：接管生效中（账本 open）每 5 分钟抽验一次系统代理现值。
    // 只读不写——发现外部改动仅置 drift 状态（UI 黄条 [重新归位]/[接受]），
    // 绝不自动改写（用户可能正手动调试网络，抢写比漂移本身更恶劣）。
    // 内核未运行不巡检：崩溃/重启的处置权在上一条看门狗，避免双线程抢方向。
    let drift_state = state.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(300));
            if !drift_should_run.load(Ordering::Relaxed) {
                continue;
            }
            let (port, drift_slot, drift_ack) = {
                let g = drift_state.lock().unwrap();
                (g.mihomo.port, g.drift.clone(), g.drift_ack.clone())
            };
            if drift_ack.load(Ordering::Relaxed) {
                continue; // 用户已接受现状，停止打扰
            }
            if ledger::LedgerFile::default().open_session().is_none() {
                // 未接管（无未结账本）：系统代理现状不归本巡检管。
                // P1-4 跨引擎细节：账本可能刚被 MCP 侧还原/结账，App 内旧黄条
                // 必须随之解除——接管已结束，警报不得续存扰民。
                let mut d = drift_slot.lock().unwrap();
                if d.take().is_some() {
                    eprintln!("[drift] 接管已结束（无未结账本），解除遗留漂移提示");
                }
                continue;
            }
            if !MihomoManager::new().status().running {
                continue;
            }
            let _ = port;
            if let Some(services) = probe_drift_pac(drift_ack.load(Ordering::Relaxed)) {
                let ts = ledger::now_ts();
                eprintln!("[drift] 接管期间 PAC 配置被外部改动：{} 个服务不达标", services.len());
                *drift_slot.lock().unwrap() = Some(DriftNotice { services, ts });
            } else {
                // 恢复达标（如用户调试完自己改回来）：清除旧的黄条，不留过期警报
                let mut d = drift_slot.lock().unwrap();
                if d.is_some() {
                    *d = None;
                    eprintln!("[drift] 系统代理已回到接管态，解除漂移提示");
                }
            }
        }
    });

    // 启动时自动清理第三方代理：App 一打开就把系统里其他代理（FlClash/Clash 等）
    // 全部关掉，并把系统代理恢复为「未设置」，让网络回到最初干净状态。
    // 这样用户打开本软件即成为系统唯一代理，不会有多个代理抢流量/抢端口。
    // 放在后台线程执行，避免阻塞窗口显示（清理含 1.5 秒等待）。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(800));
        let cleaned = cleanup_foreign_proxies();
        if !cleaned.is_empty() {
            eprintln!("[startup] 已清理第三方代理: {}", cleaned.join("、"));
        }
    });

    tauri::Builder::default()
        .manage(state)
        // SSH 会话单独管理：连接验证（最长十余秒）与终端读写不经过配置大锁，
        // 避免连接期间/终端高频 IO 卡住 get_status 轮询。
        // 用 Arc 包住：ssh_connect 需把共享实例 move 进 spawn_blocking 线程，
        // 若各自 new() 会导致会话存不到共享状态，后续 ssh_write/disconnect 找不到会话。
        .manage(crate::ssh::SshManager::new())
        // 自动更新：updater 下载 .app.tar.gz + Ed25519 验签 + 整体覆盖；
        // dialog 用于弹「发现新版本」原生对话框；process 用于安装后 relaunch 重启
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_status,
            check_dns,
            repair_dns,
            get_config,
            save_config,
            scan_apps,
            start_proxy,
            stop_proxy,
            restore_network,
            reapply_takeover,
            accept_drift,
            set_system_proxy,
            ssh_connect,
            ssh_write,
            ssh_read,
            ssh_disconnect,
            ssh_exec,
            server_metrics,
            select_ssh_server,
            delete_ssh_server,
            self_test,
            check_conflicts,
            kill_foreign_proxies,
            list_foreign_proxies,
            takeover_plan,
            audit_network,
            fetch_subscription,
            proxy_api,
            updater::get_update_channel,
            updater::set_update_channel,
            updater::check_channel_update,
            updater::install_channel_update
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // ── 退出收尾钩子（2026-09-03 事故根因修复）──
            // 铁律：App 生命周期必须完整覆盖内核生命周期——关 App = 关代理 + 关系统代理。
            // 此前 App 退出没有任何清理钩子，root mihomo 变成孤儿进程继续用 TUN
            // 接管全机流量，劫持其他应用（WorkBuddy 中转站请求被掐成 ECONNRESET）。
            // RunEvent::Exit 在所有退出路径（关窗、Cmd+Q、app.exit()、系统注销）必经。
            if let tauri::RunEvent::Exit = event {
                eprintln!("[magic-agent] RunEvent::Exit：开始收尾（停内核+断SSH+关系统代理）");
                let state = app.state::<Arc<Mutex<AppState>>>();
                // 锁可能被毒化（其他线程持锁 panic），退出路径绝不能再 panic
                let g = state.lock().unwrap_or_else(|e| e.into_inner());
                // 先通知看门狗停止，避免退出时它检测到 mihomo 已死又自动拉起
                g.should_run.store(false, Ordering::Relaxed);
                g.mihomo.stop();
                // 断开 SSH 会话：不断开则 ssh/expect 子进程变孤儿，继续占着远端连接
                app.state::<crate::ssh::SshManager>().disconnect();
                // 退出收尾也要对账：没关干净的话日志点名（下次启动 P0-4 自检兜底）
                disable_all_proxy("exit");
                // P1-2：正常退出 = 接管结束，结账（与 stop_proxy 同语义，只结账不回滚）
                match ledger::LedgerFile::default().settle_open("app_exit") {
                    Ok(true) => eprintln!("[ledger] 接管已结账（app_exit）"),
                    Ok(false) => {}
                    Err(e) => eprintln!("[ledger] WARN 退出结账失败: {e}"),
                }
                eprintln!("[magic-agent] RunEvent::Exit：收尾完成");
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_response_chunked() {
        // mihomo /connections 实际返回：Transfer-Encoding: chunked
        // 注意：chunk 长度必须与实际数据字节数一致（这里 body 就是完整内容）
        let body_json = "{\"downloadTotal\":18428227,\"connections\":[]}";
        let size = format!("{:x}", body_json.len());
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\n{body_json}\r\n0\r\n\r\n"
        );
        let (status, body) = parse_http_response(raw.as_bytes());
        assert_eq!(status, 200);
        // 关键：chunk 长度块与终止块都被剥掉，body 是纯 JSON
        assert!(!body.contains("\r\n0\r\n"), "终止块未被剥离");
        assert_eq!(body, body_json);
        // 可被 JSON 解析
        let _: serde_json::Value = serde_json::from_str(&body).expect("body 应是合法 JSON");
    }

    #[test]
    fn parse_http_response_content_length() {
        // mihomo 401 实际返回：body 含结尾换行，Content-Length 精确匹配
        let raw = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"message\":\"Unauthorized\"}\n";
        let (status, body) = parse_http_response(raw.as_bytes());
        assert_eq!(status, 401);
        assert_eq!(body, "{\"message\":\"Unauthorized\"}\n");
    }

    #[test]
    fn parse_http_response_multi_chunk() {
        // 多个 chunk 块拼接
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                   5\r\nhello\r\n\
                   6\r\n world\r\n\
                   0\r\n\r\n";
        let (_, body) = parse_http_response(raw.as_bytes());
        assert_eq!(body, "hello world");
    }

    #[test]
    fn parse_http_response_chunked_with_utf8() {
        // chunk 内含多字节 UTF-8（中文），按字节切分不能错位
        let body_json = "{\"chains\":[\"示例节点2\",\"NODE-示例节点2\"]}";
        let size = format!("{:x}", body_json.len());
        let raw = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\n{body_json}\r\n0\r\n\r\n"
        );
        let (_, body) = parse_http_response(raw.as_bytes());
        assert_eq!(body, body_json);
        let _: serde_json::Value = serde_json::from_str(&body).expect("UTF-8 body 应可解析");
    }
}

#[cfg(test)]
mod server_metrics_tests {
    use super::*;

    #[test]
    fn parse_metrics_linux() {
        let raw = r#"---CPU---
%Cpu(s):  5.2 us,  3.1 sy,  0.0 ni, 89.8 id,  1.9 wa,  0.0 hi,  0.0 si,  0.0 st
---MEM---
              total        used        free      shared  buff/cache   available
Mem:            1986         823         214          42         948        1038
---DISK---
/dev/vda1        59G   28G   29G  50% /
---LOAD---
0.12 0.09 0.08 1/123 456
---UPTIME---
12:34:56 up 10 days,  3:04,  1 user,  load average: 0.12, 0.09, 0.08
---NET---
eth0: 1234567890 1000 0 0 0 0 0 0 9876543210 2000 0 0 0 0 0 0
"#;
        let m = parse_server_metrics(raw);
        assert_eq!(m["probe_ok"], true);
        assert!((m["cpu_usage_pct"].as_f64().unwrap() - 10.2).abs() < 0.01);
        assert_eq!(m["mem_total_mb"], 1986.0);
        assert_eq!(m["mem_used_mb"], 823.0);
        assert_eq!(m["disk_usage_pct"], "50");
        assert_eq!(m["load_1m"], "0.12");
        assert_eq!(m["net_eth0_rx_bytes"], 1234567890.0);
        assert_eq!(m["net_eth0_tx_bytes"], 9876543210.0);
    }

    #[test]
    fn parse_metrics_empty_sections() {
        let raw = "---CPU---\nn/a\n---MEM---\nn/a\n---DISK---\nn/a\n";
        let m = parse_server_metrics(raw);
        assert_eq!(m["probe_ok"], true);
        assert!(m.get("cpu_usage_pct").is_none());
    }

    #[test]
    fn metrics_include_server_identity_for_dashboard() {
        let m = attach_server_identity(
            serde_json::json!({"probe_ok": true}),
            "prod-1",
            "203.0.113.10",
            "root",
        );
        assert_eq!(m["server"]["name"], "prod-1");
        assert_eq!(m["server"]["host"], "203.0.113.10");
        assert_eq!(m["server"]["user"], "root");
    }

    /// P1-4 漂移判定：不达标清单非空且未接受 → 报漂移（原样带出服务清单）
    #[test]
    fn drift_judged_when_mismatched_and_not_acked() {
        let d = judge_drift(&["Wi-Fi".to_string(), "USB 10/100 LAN".to_string()], false);
        assert_eq!(d.unwrap(), vec!["Wi-Fi".to_string(), "USB 10/100 LAN".to_string()]);
    }

    /// 读回全部达标 → 无漂移（绝不在正常状态下报警扰民）
    #[test]
    fn drift_none_when_all_services_ok() {
        assert!(judge_drift(&[], false).is_none());
    }

    /// 用户已 [接受现状] → 即使不达标也不再报（巡检线程同时会跳过，双保险）
    #[test]
    fn drift_silent_after_accept() {
        assert!(judge_drift(&["Wi-Fi".to_string()], true).is_none());
    }
}
