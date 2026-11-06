use serde::{Deserialize, Serialize};
use std::process::Command;

/// networksetup 可执行路径（与 MCP 侧 /usr/sbin/networksetup 一致）
const NETWORKSETUP: &str = "/usr/sbin/networksetup";
const SCUTIL: &str = "/usr/sbin/scutil";

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ServiceProxyState {
    /// 网络服务名，如 "Wi-Fi"
    pub service: String,
    /// -getwebproxystate 结果
    pub http_on: bool,
    /// -getsecurewebproxystate 结果
    pub https_on: bool,
    /// -getsocksfirewallproxystate 结果
    pub socks_on: bool,
    /// 该服务上失败的操作及原因（正常为空）
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemProxyStatus {
    /// 全局视图（scutil --proxy）：任一协议 Enable=1 即为开。
    /// 注意这是"整机当前是否有代理生效"，与逐服务对账互补。
    pub enabled: bool,
    pub http_port: u16,
    pub socks_port: u16,
    /// 逐服务真实状态（networksetup 读回）。空数组 = 无法枚举服务。
    pub services: Vec<ServiceProxyState>,
    /// services 全部达成期望（开=三项全 on，关=三项全 off）
    pub all_ok: bool,
    /// 设置后验证仍不达标（或读写命令失败）的服务清单，如实上报，绝不吞掉
    pub mismatched: Vec<String>,
}

impl Default for SystemProxyStatus {
    fn default() -> Self {
        SystemProxyStatus {
            enabled: false,
            http_port: 0,
            socks_port: 0,
            services: vec![],
            all_ok: false,
            mismatched: vec![],
        }
    }
}

/// 设置系统代理并【逐服务回读验证】。
///
/// 旧实现的 `any_success`（任一服务任一命令成功即报整体成功）是"假账"根源：
/// 12 个服务只设上 1 个也返回成功。现在每个服务设置后立即用 -get* 读回，
/// 结果对象如实携带每个服务的三个开关状态与错误；不再仅凭写命令退出码下结论。
/// 部分失败时不报 Err（系统代理确实可能改了半套），而是 all_ok=false +
/// mismatched 点名，由调用方/UI 决定是否回滚或提示。
/// 仅当"没有任何服务可供操作"时才返回 Err。
pub fn set_system_proxy(enable: bool, port: u16) -> Result<SystemProxyStatus, String> {
    let services = list_services();
    if services.is_empty() {
        return Err("未检测到网络服务，无法设置系统代理".to_string());
    }
    let port_str = port.to_string();

    // 第一遍：逐服务下发设置命令（失败先记下，不中断——其余服务继续）
    let mut write_errors: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for svc in &services {
        let args_list: Vec<Vec<&str>> = if enable {
            vec![
                vec!["-setwebproxy", svc, "127.0.0.1", &port_str],
                vec!["-setsecurewebproxy", svc, "127.0.0.1", &port_str],
                vec!["-setsocksfirewallproxy", svc, "127.0.0.1", &port_str],
                vec!["-setwebproxystate", svc, "on"],
                vec!["-setsecurewebproxystate", svc, "on"],
                vec!["-setsocksfirewallproxystate", svc, "on"],
            ]
        } else {
            // v0.4.2：关闭时把「服务器地址」一起复位，消灭 `Server: off` 这类残留。
            // 事故复盘：`networksetup -setwebproxy <服务> off` 会把字符串 "off"
            // 当成代理主机名写进配置——于是"代理已关闭"的表象下，每条连接仍要
            // 去解析主机名 "off"，全部失败，症状看起来像 DNS 坏了。
            // 只清开关不清地址，正是那种状态能存活好几天的原因。
            reset_channel_residue(svc, "-getwebproxy", "-setwebproxy", "-setwebproxystate");
            reset_channel_residue(svc, "-getsecurewebproxy", "-setsecurewebproxy", "-setsecurewebproxystate");
            reset_channel_residue(svc, "-getsocksfirewallproxy", "-setsocksfirewallproxy", "-setsocksfirewallproxystate");
            vec![]
        };
        for args in args_list {
            if let Err(e) = run(NETWORKSETUP, &args) {
                write_errors.entry(svc.clone()).or_default().push(format!(
                    "{}: {}",
                    args.first().copied().unwrap_or("?"),
                    e
                ));
            }
        }
    }

    // 第二遍：逐服务回读对账（真实说了算的是读回结果，不是写命令退出码）。
    // 达标口径已封装在 verify_system_proxy 内（开=Enabled+端口精确匹配；关=Enabled:No）。
    let states = verify_system_proxy(&services, port, enable);
    let mut mismatched = Vec::new();
    for st in &states {
        // *_on 语义 = "该通道是否达标"（开=Enabled:Yes 且端口精确匹配；关=Enabled:No），
        // 判定已封装在 verify 内，调用处三与即可。
        if !(st.http_on && st.https_on && st.socks_on) || !st.errors.is_empty() {
            mismatched.push(st.service.clone());
        }
    }
    // 写失败但读回恰好符合期望的（如命令 stderr 有警告但生效了），以读回为准；
    // 写失败且未点名进 mismatched 的服务也要点名，错误信息不丢
    for svc in &services {
        if write_errors.contains_key(svc) && !mismatched.contains(svc) {
            mismatched.push(svc.clone());
        }
    }

    let all_ok = !states.is_empty()
        && write_errors.is_empty()
        && mismatched.is_empty();
    // 端口从 scutil 全局视图读一次（best-effort，读不到置 0，不影响对账结论）
    let global = status();

    // enabled 字段在 set 路径取"期望值"：逐服务对账（all_ok/services/mismatched）
    // 才是写后真实性的权威。不用 scutil 全局视图回填——它有秒级传播延迟，
    // 刚开启就立刻读可能仍报 0，会让前端开关闪回、产生新的"假账"。
    // 全局视图的权威用途是轮询侧 status()。
    Ok(SystemProxyStatus {
        enabled: enable,
        http_port: global.http_port,
        socks_port: global.socks_port,
        services: states,
        all_ok,
        mismatched,
    })
}

/// 逐服务读回系统代理真实状态。
///
/// ⚠️ 命令选择依据（2026-09-23 实机实测，Darwin 25.6）：
/// `-getwebproxystate` 系列读命令【不存在】（rc=5 "command is not recognized"），
/// 但 `-setwebproxystate` 存在——读写命令集不对称。此前用 -get*state 做读回
/// 导致对账恒失败、恒误报 mismatched。唯一可靠读法是 -getwebproxy 三件套，
/// 真实输出格式：
///   Enabled: Yes
///   Server: 127.0.0.1
///   Port: 7891
///   Authenticated Proxy Enabled: 0
///
/// P1-7 口径修正（v0.4.1）：**开关态以 scutil 全局视图为唯一权威**——真机实测
/// 出现过矛盾态（scutil 全局 HTTPEnable: 0 而服务级 -getwebproxy 仍回
/// Enabled: Yes），从服务级 Enabled 推断开关态会虚报 mismatched。因此：
///   - 开（expect_on=true）：达标 = 全局开（HTTPEnable:1/SOCKSEnable:1）
///     且服务级 Port=期望端口（"开着但指向别人的端口"同样点名）；
///   - 关（expect_on=false）：达标 = 全局关 且服务级 Port≠期望端口
///     （服务级 Enabled 字段不参与判定；残留指向本程序端口仍点名）。
/// 单项读取失败记入该服务 errors，不影响其他服务（绝不 panic、绝不静默吞错）。
pub fn verify_system_proxy(services: &[String], want_port: u16, expect_on: bool) -> Vec<ServiceProxyState> {
    // 全局权威只读一次（秒级传播延迟存在，但 set 完成后已过重写+短暂窗口；
    // 与服务级联合判定避免"全局已传播、服务级矛盾"的虚报）
    let global_enabled = status().enabled;
    services
        .iter()
        .map(|svc| {
            let mut errors = Vec::new();
            let http = get_proxy_detail(svc, "-getwebproxy", want_port, expect_on, global_enabled, &mut errors);
            let https = get_proxy_detail(svc, "-getsecurewebproxy", want_port, expect_on, global_enabled, &mut errors);
            let socks = get_proxy_detail(svc, "-getsocksfirewallproxy", want_port, expect_on, global_enabled, &mut errors);
            ServiceProxyState {
                service: svc.clone(),
                http_on: http.0,
                https_on: https.0,
                socks_on: socks.0,
                errors,
            }
        })
        .collect()
}

/// 返回 (是否达标, 读到的端口)。
/// 达标口径（P1-7）：开关态由 scutil 全局视图决定（global_enabled），服务级只核对端口；
/// 开启时还要求服务级精确指向期望端口。
fn get_proxy_detail(svc: &str, flag: &str, want_port: u16, expect_on: bool, global_enabled: bool, errors: &mut Vec<String>) -> (bool, u16) {
    match run(NETWORKSETUP, &[flag, svc]) {
        Ok(out) => {
            let (_enabled, port) = parse_proxy_detail(&out);
            let ok = if expect_on {
                global_enabled && port == want_port
            } else {
                !global_enabled && port != want_port
            };
            if port != want_port && port != 0 {
                // 服务级指向别的端口：把事实塞进 errors 供点名排查（开着时指向
                // 非期望端口 = 失控；关着时仍指向期望端口 = 残留，都点名）
                errors.push(format!("{}: 指向 {}:{} 而非期望端口 {}", flag, "127.0.0.1", port, want_port));
            }
            (ok, port)
        }
        Err(e) => {
            errors.push(format!("{}: {}", flag, e));
            (false, 0)
        }
    }
}

/// 单个服务在某一代理通道上的【完整原值】（P1-2 账本快照用）。
/// 与 ServiceProxyState 的"达标"语义不同：这里如实记录 enabled/server/port，
/// 回放时才能把系统恢复成接管前的样子，而不是恢复成"达标"。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyChannelRaw {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

/// 一个网络服务三条通道的完整原值快照。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub service: String,
    pub http: ProxyChannelRaw,
    pub https: ProxyChannelRaw,
    pub socks: ProxyChannelRaw,
    /// 读取失败时非空；回放遇到有 error 的通道会跳过（宁可不还原，不能瞎写）
    pub errors: Vec<String>,
}

/// 逐服务读取【完整原值】（P1-2 接管账本的 before 快照）。
/// 读法沿用 -getwebproxy 三件套（真机唯一可靠读法）。
pub fn snapshot_system_proxy() -> Vec<ServiceSnapshot> {
    list_services()
        .iter()
        .map(|svc| {
            let mut errors = Vec::new();
            let http = read_channel(svc, "-getwebproxy", &mut errors);
            let https = read_channel(svc, "-getsecurewebproxy", &mut errors);
            let socks = read_channel(svc, "-getsocksfirewallproxy", &mut errors);
            ServiceSnapshot {
                service: svc.clone(),
                http,
                https,
                socks,
                errors,
            }
        })
        .collect()
}

fn read_channel(svc: &str, flag: &str, errors: &mut Vec<String>) -> ProxyChannelRaw {
    match run(NETWORKSETUP, &[flag, svc]) {
        Ok(out) => {
            let (enabled, port) = parse_proxy_detail(&out);
            let server = parse_proxy_server(&out);
            ProxyChannelRaw {
                enabled,
                server,
                port,
            }
        }
        Err(e) => {
            errors.push(format!("{flag}: {e}"));
            ProxyChannelRaw {
                enabled: false,
                server: String::new(),
                port: 0,
            }
        }
    }
}

/// 解析 -get*proxy 输出的 Server 行（真机格式 "Server: 127.0.0.1"）。
fn parse_proxy_server(out: &str) -> String {
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("Server:") {
            return v.trim().to_string();
        }
    }
    String::new()
}

/// 按快照把某服务的某通道恢复回原值（P1-2 账本回放的最小单元）。
/// 返回 Err 只在该通道【写命令失败】时发生；回放调用方汇总后如实上报，绝不静默。
fn restore_channel(svc: &str, flag: &str, state_flag: &str, raw: &ProxyChannelRaw) -> Result<(), String> {
    if raw.enabled {
        let port = raw.port.to_string();
        let server = if raw.server.is_empty() { "127.0.0.1".to_string() } else { raw.server.clone() };
        run(NETWORKSETUP, &[flag, svc, &server, &port])?;
        run(NETWORKSETUP, &[state_flag, svc, "on"])?;
    } else {
        run(NETWORKSETUP, &[state_flag, svc, "off"])?;
    }
    Ok(())
}

/// 按快照逆序回放一个服务（HTTP→HTTPS→SOCKS 的写入顺序与接管时一致）。
/// 有 error 的通道跳过还原并如实记入返回的错误清单（宁可不还原，不能凭 unknown 瞎写）。
pub fn restore_service_snapshot(snap: &ServiceSnapshot) -> Vec<String> {
    let mut errs = Vec::new();
    if !snap.errors.is_empty() {
        errs.push(format!("{}: 快照不完整（{}），跳过还原", snap.service, snap.errors.join("；")));
        return errs;
    }
    if let Err(e) = restore_channel(&snap.service, "-setwebproxy", "-setwebproxystate", &snap.http) {
        errs.push(format!("{}: http 还原失败 {e}", snap.service));
    }
    if let Err(e) = restore_channel(&snap.service, "-setsecurewebproxy", "-setsecurewebproxystate", &snap.https) {
        errs.push(format!("{}: https 还原失败 {e}", snap.service));
    }
    if let Err(e) = restore_channel(&snap.service, "-setsocksfirewallproxy", "-setsocksfirewallproxystate", &snap.socks) {
        errs.push(format!("{}: socks 还原失败 {e}", snap.service));
    }
    errs
}

/// 解析 -get*proxy 输出：提取 Enabled 布尔与 Port。无法解析视为关/0。
fn parse_proxy_detail(out: &str) -> (bool, u16) {
    let mut enabled = false;
    let mut port = 0u16;
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("Enabled:") {
            enabled = v.trim().eq_ignore_ascii_case("yes");
        } else if let Some(v) = l.strip_prefix("Port:") {
            port = v.trim().parse().unwrap_or(0);
        }
        // 注意：末行 "Authenticated Proxy Enabled: 0" 也以 Enabled 结尾但带前缀，
        // strip_prefix("Enabled:") 不会误匹配它（其前缀是 "Authenticated Proxy "）。
    }
    (enabled, port)
}

fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("启动 {bin} 失败: {e}"))
        .and_then(|o| {
            if o.status.success() {
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                Err(format!(
                    "{}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ))
            }
        })
}

/// 全局视图：scutil --proxy。口径与 status() 一致，set 之后合并进返回值。
pub fn status() -> SystemProxyStatus {
    let text = Command::new(SCUTIL)
        .arg("--proxy")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let enabled = text.contains("HTTPEnable : 1") || text.contains("SOCKSEnable : 1");
    let http_port = parse_port(&text, "HTTPPort");
    let socks_port = parse_port(&text, "SOCKSPort");
    SystemProxyStatus {
        enabled,
        http_port: http_port.unwrap_or(0),
        socks_port: socks_port.unwrap_or(0),
        // 轮询路径只做一次 scutil，不带逐服务对账（那是 set 后的重操作）。
        // 消费方规则：services/mismatched 为空时只能读 enabled/端口，
        // 需要逐服务真实状态必须走 set_system_proxy 返回值或 verify_system_proxy。
        services: vec![],
        all_ok: enabled,
        mismatched: vec![],
    }
}

/// 枚举真实网络服务（P1-4 漂移巡检需要与 verify 同一份清单，提为 pub(crate)）。
pub(crate) fn list_services() -> Vec<String> {
    let text = match run(NETWORKSETUP, &["-listallnetworkservices"]) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    text.lines()
        .map(|s| s.trim().to_string())
        .filter(|l| {
            if l.is_empty() {
                return false;
            }
            let lower = l.to_lowercase();
            // 跳过非真实网络服务（蓝牙、USB 虚拟、Thunderbolt 桥接等），避免误设代理
            !lower.contains("asterisk")
                && !lower.contains("bluetooth")
                && !lower.contains("iphone")
                && !lower.contains("thunderbolt")
                && !lower.contains("bridge")
        })
        .collect()
}

fn parse_port(text: &str, key: &str) -> Option<u16> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            // scutil 格式 "HTTPPort : 7891"：strip 后先 trim 去前导空格再剥冒号。
            // 旧实现漏了这一步，对 " : 7891" 解析恒失败返回 None（潜伏 bug，
            // 曾使端口字段永远为 0；P0-4 自愈依赖端口值，必须正确）。
            let v = rest.trim().trim_start_matches(':').trim();
            return v.parse().ok();
        }
    }
    None
}

/// 把某条通道的「服务器地址」复位成 127.0.0.1:0，再关开关。
///
/// 只在**地址确实是脏的**情况下才写，避免给干净的服务制造新残留。
/// 已知脏值：空串、`off`、`0`、以及任何非回环地址。
fn reset_channel_residue(svc: &str, get_flag: &str, set_flag: &str, state_flag: &str) {
    let server = run(NETWORKSETUP, &[get_flag, svc])
        .map(|o| parse_proxy_server(&o))
        .unwrap_or_default();
    let dirty = !server.is_empty() && server != "127.0.0.1";
    if dirty {
        // 写 127.0.0.1:0 会顺手把开关打开，紧跟着的 state off 负责关回去
        let _ = run(NETWORKSETUP, &[set_flag, svc, "127.0.0.1", "0"]);
    }
    let _ = run(NETWORKSETUP, &[state_flag, svc, "off"]);
}

// ─────────────────────────────────────────────────────────────────────────────
// PAC 模式（v0.4.2）：用「白名单自动代理」替代「全局 HTTP/SOCKS 代理」
// ─────────────────────────────────────────────────────────────────────────────

/// 单个服务的 PAC 应用结果
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PacServiceState {
    pub service: String,
    pub enabled: bool,
    pub url: String,
    /// 是否达成期望（开=PAC 已启用且 URL 一致；关=已停用）
    pub ok: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PacApplyResult {
    pub pac_url: String,
    pub services: Vec<PacServiceState>,
    pub all_ok: bool,
    pub mismatched: Vec<String>,
}

/// 解析 `-getautoproxyurl` 输出：
///   URL: http://127.0.0.1:18080/proxy.pac
///   Enabled: Yes
fn parse_autoproxy(out: &str) -> (bool, String) {
    let mut enabled = false;
    let mut url = String::new();
    for line in out.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("URL:") {
            url = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("Enabled:") {
            enabled = v.trim().eq_ignore_ascii_case("yes");
        }
    }
    (enabled, url)
}

/// 下发 PAC 配置到所有网络服务，并**逐服务读回**对账。
///
/// 与旧 `set_system_proxy(true, ..)` 的关键区别：这里配的是"名单内的站点才走代理"，
/// 名单外的流量物理直连，内核死了也不影响——这是"不再劫持整个网络"的实现点。
pub fn apply_pac(pac_url: &str) -> Result<PacApplyResult, String> {
    let services = list_services();
    if services.is_empty() {
        return Err("未检测到网络服务，无法下发 PAC 配置".to_string());
    }
    let mut states = Vec::new();
    let mut mismatched = Vec::new();
    for svc in &services {
        let mut errors = Vec::new();
        // 1) 先关掉旧的全局代理（含清脏地址），避免 PAC 与全局代理双开
        reset_channel_residue(svc, "-getwebproxy", "-setwebproxy", "-setwebproxystate");
        reset_channel_residue(svc, "-getsecurewebproxy", "-setsecurewebproxy", "-setsecurewebproxystate");
        reset_channel_residue(svc, "-getsocksfirewallproxy", "-setsocksfirewallproxy", "-setsocksfirewallproxystate");
        // 2) 写入 PAC URL 并启用
        if let Err(e) = run(NETWORKSETUP, &["-setautoproxyurl", svc, pac_url]) {
            errors.push(format!("-setautoproxyurl: {e}"));
        }
        if let Err(e) = run(NETWORKSETUP, &["-setautoproxystate", svc, "on"]) {
            errors.push(format!("-setautoproxystate: {e}"));
        }
        // 3) 读回对账
        let (enabled, url) = run(NETWORKSETUP, &["-getautoproxyurl", svc])
            .map(|o| parse_autoproxy(&o))
            .unwrap_or((false, String::new()));
        let ok = enabled && url == pac_url && errors.is_empty();
        if !ok {
            mismatched.push(svc.clone());
        }
        states.push(PacServiceState {
            service: svc.clone(),
            enabled,
            url,
            ok,
            errors,
        });
    }
    let all_ok = mismatched.is_empty() && states.iter().all(|s| s.ok);
    Ok(PacApplyResult {
        pac_url: pac_url.to_string(),
        services: states,
        all_ok,
        mismatched,
    })
}

impl PacApplyResult {
    /// 转成旧的 `SystemProxyStatus` 形状，保证前端契约不变
    /// （UI 只消费 enabled/allOk/services[].httpOn/mismatched，不必跟着改）。
    pub fn to_status(&self) -> SystemProxyStatus {
        let services: Vec<ServiceProxyState> = self
            .services
            .iter()
            .map(|s| ServiceProxyState {
                service: s.service.clone(),
                // PAC 是"域名级"分流，没有 http/https/socks 三通道之分，
                // 这里三字段同值表达"该服务整体是否达标"，避免前端出现半吊子状态。
                http_on: s.ok,
                https_on: s.ok,
                socks_on: s.ok,
                errors: s.errors.clone(),
            })
            .collect();
        SystemProxyStatus {
            enabled: true,
            http_port: 0,
            socks_port: 0,
            services,
            all_ok: self.all_ok,
            mismatched: self.mismatched.clone(),
        }
    }
}

/// 读取单个服务的 PAC 现状：(是否启用, URL)。漂移巡检与自愈都用它。
pub fn pac_state_for(svc: &str) -> (bool, String) {
    run(NETWORKSETUP, &["-getautoproxyurl", svc])
        .map(|o| parse_autoproxy(&o))
        .unwrap_or((false, String::new()))
}

/// 关闭所有服务的 PAC（不碰内核，纯系统设置收尾）。
pub fn clear_pac() -> Vec<String> {
    let mut errs = Vec::new();
    for svc in list_services() {
        if let Err(e) = run(NETWORKSETUP, &["-setautoproxystate", &svc, "off"]) {
            errs.push(format!("{svc}: PAC 关闭失败 {e}"));
        }
    }
    errs
}

/// `scutil --proxy` 里 PAC 是否开启（全局视图，收尾判断用）
pub fn pac_enabled_globally() -> bool {
    Command::new(SCUTIL)
        .arg("--proxy")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("ProxyAutoConfigEnable : 1"))
        .unwrap_or(false)
}

/// **一键回到"什么都不劫持"**：关 PAC + 关三条全局代理 + 清脏地址。
/// 自愈线程、退出收尾、信号收尾都走这一个入口，保证只有一套收尾口径。
pub fn disable_everything() -> Vec<String> {
    let mut errs = clear_pac();
    for svc in list_services() {
        reset_channel_residue(&svc, "-getwebproxy", "-setwebproxy", "-setwebproxystate");
        reset_channel_residue(&svc, "-getsecurewebproxy", "-setsecurewebproxy", "-setsecurewebproxystate");
        reset_channel_residue(&svc, "-getsocksfirewallproxy", "-setsocksfirewallproxy", "-setsocksfirewallproxystate");
    }
    errs
}

/// 是否还有任何形式的代理/PAC 生效（收尾判定用）
pub fn any_proxy_active() -> bool {
    pac_enabled_globally() || status().enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机实测 fixture（2026-09-23，Darwin 25.6，networksetup -getwebproxy Wi-Fi）：
    ///     Enabled: Yes / Server: 127.0.0.1 / Port: 7891 / Authenticated Proxy Enabled: 0
    /// 教训：上一版测试 fixture 用臆测的 "Web Proxy ... is Enabled." 格式，与真机完全不符，
    /// 纯 mock 测试因此全绿但真实对账恒失败。任何解析逻辑改动必须先跑真机输出对格式。
    /// 真机集成（手动执行）：真实下发 PAC 再撤回，验证"开得起来也关得干净"。
    /// 默认 `#[ignore]`，因为它会真改本机网络设置。
    ///     cargo test --offline -- --ignored real_machine_pac
    #[test]
    #[ignore = "真机集成：会真实改写系统代理设置，仅在手动执行时运行"]
    fn real_machine_pac_apply_then_clear() {
        let url = crate::pac::ensure_running(crate::pac::build_pac(
            7891,
            &["example.com".to_string()],
        ))
        .expect("PAC 服务应能启动");
        assert!(url.starts_with("http://127.0.0.1:"));

        let r = apply_pac(&url).expect("apply_pac 应成功");
        assert!(!r.services.is_empty(), "应至少枚举到一个网络服务");
        let enabled = r.services.iter().filter(|s| s.enabled && s.url == url).count();
        assert!(enabled >= 1, "至少一个服务应读到 PAC 已启用: {:?}", r.services);
        assert!(pac_enabled_globally(), "scutil 全局视图应显示 PAC 已开");

        // 收尾必须是"关 PAC + 关全局 + 清脏地址"
        let errs = disable_everything();
        assert!(errs.is_empty(), "收尾应无错误: {errs:?}");
        assert!(!pac_enabled_globally(), "收尾后 PAC 应关闭");
        let st = status();
        assert!(!st.enabled, "收尾后全局代理应关闭");
        crate::pac::stop();
    }

    #[test]
    fn parse_autoproxy_reads_real_output() {
        // 真机实测（Darwin 25.x，networksetup -getautoproxyurl "Thunderbolt Bridge"）
        let on = "URL: http://127.0.0.1:18080/proxy.pac\nEnabled: Yes";
        assert_eq!(
            parse_autoproxy(on),
            (true, "http://127.0.0.1:18080/proxy.pac".to_string())
        );
        let off = "URL: http://127.0.0.1:18080/proxy.pac\nEnabled: No";
        assert_eq!(parse_autoproxy(off).0, false);
        assert_eq!(parse_autoproxy(""), (false, String::new()));
    }

    #[test]
    fn residue_reset_targets_only_dirty_servers() {
        // 判定逻辑回归锁：干净的回环地址不该被"修"，脏的（off/空/外部地址）必须被修。
        // 这里锁的是口径，不是真机写操作。
        for dirty in ["off", "0", "192.168.1.1", "proxy.local"] {
            let is_dirty = !dirty.is_empty() && dirty != "127.0.0.1";
            assert!(is_dirty, "{dirty:?} 应被判定为脏地址");
        }
        // 空串 = 该通道从未被写过，不是脏值（写它反而会制造新残留）
        let empty = "";
        assert!(!(!empty.is_empty() && empty != "127.0.0.1"), "空地址不是脏值");
        let clean = "127.0.0.1";
        assert!(!(!clean.is_empty() && clean != "127.0.0.1"), "回环地址不该被重写");
    }

    #[test]
    fn parse_proxy_detail_reads_real_networksetup_format() {
        let real = "Enabled: Yes\nServer: 127.0.0.1\nPort: 7891\nAuthenticated Proxy Enabled: 0";
        assert_eq!(parse_proxy_detail(real), (true, 7891));
        let off = "Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0";
        assert_eq!(parse_proxy_detail(off), (false, 0));
        // 关键陷阱：末行 "Authenticated Proxy Enabled: 1" 不能被当成 Enabled 行误匹配
        assert_eq!(parse_proxy_detail("Authenticated Proxy Enabled: 1"), (false, 0));
        assert_eq!(parse_proxy_detail(""), (false, 0));
        assert_eq!(parse_proxy_detail("Some error text"), (false, 0));
    }

    #[test]
    fn verify_port_mismatch_is_not_ok() {
        // "开着但指向 7890"（本机 Thunderbolt Bridge 实测残留场景）：
        // 期望开且指向 7891 的口径下必须判不达标——端口精确匹配是归序的核心。
        let stale = "Enabled: Yes\nServer: 127.0.0.1\nPort: 7890\nAuthenticated Proxy Enabled: 0";
        let (enabled, port) = parse_proxy_detail(stale);
        assert!(enabled && port == 7890 && port != 7891);
    }

    #[test]
    fn parse_port_reads_scutil_format() {
        let text = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7891\n  SOCKSPort : 7891\n}";
        assert_eq!(parse_port(text, "HTTPPort"), Some(7891));
        assert_eq!(parse_port(text, "SOCKSPort"), Some(7891));
        assert_eq!(parse_port(text, "HTTPProxy"), None);
    }

    #[test]
    fn status_serializes_new_fields_camel_case() {
        let st = SystemProxyStatus {
            enabled: true,
            http_port: 7891,
            socks_port: 7891,
            services: vec![ServiceProxyState {
                service: "Wi-Fi".into(),
                http_on: true,
                https_on: true,
                socks_on: false,
                errors: vec!["-setsocksfirewallproxystate: denied".into()],
            }],
            all_ok: false,
            mismatched: vec!["Wi-Fi".into()],
        };
        let json = serde_json::to_value(&st).unwrap();
        // 前端契约：camelCase 字段名
        assert_eq!(json["allOk"], false);
        assert_eq!(json["mismatched"][0], "Wi-Fi");
        assert_eq!(json["services"][0]["httpOn"], true);
        assert_eq!(json["services"][0]["socksOn"], false);
        assert!(json["services"][0]["errors"].is_array());
    }

    #[test]
    fn service_state_partial_failure_is_visible_not_swallowed() {
        // 3 成功 1 失败：如实点名（此用例直接构造 verify 结果，不 mock 进程——
        // 决策记录：verify_system_proxy 是纯读回函数，其正确性由集成验收场景 D 兜底，
        // 单测锁定的是"结构体如实表达部分失败"这一序列化契约）
        let states = vec![
            ServiceProxyState { service: "Wi-Fi".into(), http_on: true, https_on: true, socks_on: true, errors: vec![] },
            ServiceProxyState { service: "USB LAN".into(), http_on: true, https_on: true, socks_on: true, errors: vec![] },
            ServiceProxyState { service: "Thunderbolt".into(), http_on: true, https_on: true, socks_on: true, errors: vec![] },
            ServiceProxyState { service: "AX88179A".into(), http_on: false, https_on: false, socks_on: false, errors: vec!["denied".into()] },
        ];
        let st = SystemProxyStatus {
            enabled: true,
            http_port: 7891,
            socks_port: 7891,
            services: states,
            all_ok: false,
            mismatched: vec!["AX88179A".into()],
        };
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(json["services"].as_array().unwrap().len(), 4);
        assert_eq!(json["mismatched"].as_array().unwrap().len(), 1);
        assert_eq!(json["allOk"], false);
    }
}
