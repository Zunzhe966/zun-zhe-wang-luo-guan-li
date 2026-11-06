//! DNS 健康闸门（v0.4.2）
//!
//! ## 为什么需要这个模块
//! 015 事故复盘：Wi-Fi 的 DNS 被写死成一个**不应答的地址**（局域网/虚拟网关），
//! 结果是"Wi-Fi 已连接、但所有域名都解析不出来"——用户看到的现象是"整机没网"，
//! 却很难定位到是 DNS 被钉死。一个自称"网络管理"的软件，在这种网络状态下
//! **不应该继续接管**：它会把"DNS 已坏"叠加成"DNS 坏 + 全局代理指死端口"，
//! 让故障从"能 ping 通 IP 但打不开网页"恶化成"什么都连不上"。
//!
//! ## 设计红线
//! 1. **只读为主**：`check()` 绝不在未经用户明确同意时改系统 DNS。
//! 2. **主动验证，不信任配置**：真发一个 DNS 查询包，看有没有应答，而不是
//!    看 `scutil --dns` 有没有列出服务器（列出≠能用，本次事故正是如此）。
//! 3. **修复动作单列**：`repair_to_dhcp()` 只在用户点「修复 DNS」时调用，
//!    且先返回原值供 UI 展示（可追溯、可回填）。
//! 4. 探测必须带超时：无超时的解析在坏网络栈下可能长时间阻塞 UI。

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::time::Duration;

/// 金丝雀域名：国内可直连、且必然存在。用它验证"这台机器能不能把域名翻成 IP"。
pub const CANARY_HOSTS: &[&str] = &["www.baidu.com", "www.apple.com"];

/// 单台 DNS 服务器的探测超时（毫秒）。局域网内不应答通常 1 秒内就超时，
/// 给 1200ms 是为了容忍 Wi-Fi 抖动，同时保证 UI 不会卡。
const PROBE_TIMEOUT_MS: u64 = 1200;

/// 系统解析器兜底预算（毫秒）。某些环境走 DoH / mDNS 等非 53 端口路径，
/// 单台服务器探测失败不代表系统真的解析不了，给它一次机会避免误报。
const SYSTEM_RESOLVE_BUDGET_MS: u64 = 2500;

const NETWORKSETUP: &str = "/usr/sbin/networksetup";
const SCUTIL: &str = "/usr/sbin/scutil";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsHealth {
    /// 域名解析是否真的可用（唯一权威口径）
    pub ok: bool,
    /// `scutil --dns` 里 resolver #1 的服务器清单（可能为空）
    pub servers: Vec<String>,
    /// 探测无应答的服务器（ok=true 时通常是空）
    pub failing: Vec<String>,
    /// 一句话结论，直接给 UI 展示
    pub note: String,
    /// true = 「配了 DNS 但一个都不应答」的典型钉死特征
    pub looked_pinned: bool,
    /// 被**手工**写死 DNS 的网络服务名（可交给 repair 用）
    pub manual_services: Vec<String>,
}

/// 构造一个最小的 DNS 查询报文（单问题、A 记录、RD=1）。
/// 不引第三方依赖：12 字节头 + QNAME + QTYPE + QCLASS。
fn build_query(id: u16, host: &str) -> Option<Vec<u8>> {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&id.to_be_bytes()); // ID
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: RD=1
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    q.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    q.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in host.split('.') {
        let b = label.as_bytes();
        if b.is_empty() || b.len() > 63 {
            return None; // 非法标签：宁可不发，也不发畸形包
        }
        q.push(b.len() as u8);
        q.extend_from_slice(b);
    }
    q.push(0); // 根标签
    q.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    Some(q)
}

/// 解析应答头：返回 (是否合法应答, rcode, ancount)。
/// 只看头部不解析 answer 段——我们只需要知道"服务器有没有真的给出答案"。
fn parse_header(buf: &[u8], want_id: u16) -> Option<(u8, u16)> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    if id != want_id {
        return None; // ID 不匹配：不是我们这条查询的应答
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 == 0 {
        return None; // QR=0：这是查询不是应答
    }
    let rcode = (flags & 0x000F) as u8;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);
    Some((rcode, ancount))
}

/// 直接向指定服务器发查询，判断它是否**真的能解析**。
/// 只有 rcode=0 且 ancount>0 才算通——NXDOMAIN/SERVFAIL/超时都是"不能用"。
pub fn probe_server(server: &str, host: &str) -> bool {
    let ip: IpAddr = match server.parse() {
        Ok(i) => i,
        Err(_) => return false, // 含 zone id / 域名形式：本探测不支持，视为不可用
    };
    let bind = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = match UdpSocket::bind(bind) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(PROBE_TIMEOUT_MS)));
    let const_id: u16 = 0x4D41; // "MA"，固定即可（同一 socket 只发一次）
    let Some(query) = build_query(const_id, host) else {
        return false;
    };
    if sock.send_to(&query, SocketAddr::new(ip, 53)).is_err() {
        return false;
    }
    let mut buf = [0u8; 768];
    match sock.recv_from(&mut buf) {
        Ok((n, _)) => matches!(parse_header(&buf[..n], const_id), Some((0, an)) if an > 0),
        Err(_) => false,
    }
}

/// 走系统解析器（getaddrinfo）验证。带预算超时——getaddrinfo 无超时参数，
/// 必须丢到线程里等，否则坏网络下会把调用方卡死。
pub fn system_resolve_ok(host: &str, budget_ms: u64) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        let ok = (h.as_str(), 443u16)
            .to_socket_addrs()
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        let _ = tx.send(ok);
    });
    rx.recv_timeout(Duration::from_millis(budget_ms)).unwrap_or(false)
}

/// `scutil --dns` → resolver #1 的服务器清单。解析逻辑与体检卡同源，避免两套口径。
pub fn current_servers() -> Vec<String> {
    let out = match Command::new(SCUTIL).arg("--dns").output() {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let text = String::from_utf8_lossy(&out.stdout);
    crate::auditor::parse_dns_nameservers(&text)
}

/// 列出**被手工写死** DNS 的服务名。
/// 判据来自真机实测：`-getdnsservers` 在"继承 DHCP"时返回
/// "There aren't any DNS Servers set on <服务>"，只有手工填写才返回地址列表。
/// 所以"非空输出"就是"人工钉死"的可靠特征。
pub fn manual_dns_services() -> Vec<String> {
    let services = crate::system_proxy::list_services();
    let mut out = Vec::new();
    for svc in services {
        if let Ok(o) = Command::new(NETWORKSETUP)
            .args(["-getdnsservers", &svc])
            .output()
        {
            if !o.status.success() {
                continue;
            }
            let t = String::from_utf8_lossy(&o.stdout);
            let t = t.trim();
            if t.is_empty() || t.contains("aren't any DNS Servers") {
                continue;
            }
            out.push(svc);
        }
    }
    out
}

/// 主动体检：这台机器现在到底能不能把域名解析成 IP。
pub fn check() -> DnsHealth {
    let servers = current_servers();
    let manual = manual_dns_services();
    if servers.is_empty() {
        return DnsHealth {
            ok: false,
            servers,
            failing: vec![],
            note: "系统没有可用的 DNS 服务器（resolver #1 为空），域名无法解析".to_string(),
            looked_pinned: false,
            manual_services: manual,
        };
    }

    let mut failing = Vec::new();
    let mut answered = false;
    for s in &servers {
        if CANARY_HOSTS.iter().any(|h| probe_server(s, h)) {
            answered = true;
        } else {
            failing.push(s.clone());
        }
    }
    if answered {
        return DnsHealth {
            ok: true,
            servers,
            failing: vec![],
            note: "DNS 应答正常".to_string(),
            looked_pinned: false,
            manual_services: manual,
        };
    }

    // 服务器全都不应答：给系统解析器一次机会（可能走 DoH/mDNS 等非 53 路径）
    if CANARY_HOSTS.iter().any(|h| system_resolve_ok(h, SYSTEM_RESOLVE_BUDGET_MS)) {
        return DnsHealth {
            ok: true,
            servers,
            failing,
            note: "配置的 DNS 不应答，但系统解析可用（可能有 DoH/mDNS 兜底）".to_string(),
            looked_pinned: false,
            manual_services: manual,
        };
    }

    DnsHealth {
        ok: false,
        failing: failing.clone(),
        note: format!(
            "DNS 服务器全部不应答：{}。这就是「Wi-Fi 已连接却打不开网页」的直接原因",
            servers.join("、")
        ),
        looked_pinned: !manual.is_empty(),
        servers,
        manual_services: manual,
    }
}

/// 把指定服务（或全部手工钉死的服务）的 DNS 改回 `Empty`，即重新继承 DHCP。
/// **只在用户明确点「修复 DNS」时调用**。返回 (已改写的服务, 被覆盖的原值)。
pub fn repair_to_dhcp(only: Option<&str>) -> Result<(Vec<String>, Vec<(String, Vec<String>)>), String> {
    let targets: Vec<String> = match only {
        Some(s) => vec![s.to_string()],
        None => manual_dns_services(),
    };
    if targets.is_empty() {
        return Ok((vec![], vec![]));
    }
    let mut done = Vec::new();
    let mut before = Vec::new();
    for svc in targets {
        // 先记原值（可追溯；UI 可展示"我改了这些"）
        let old: Vec<String> = Command::new(NETWORKSETUP)
            .args(["-getdnsservers", &svc])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty() && !l.contains("aren't any DNS Servers"))
                    .collect()
            })
            .unwrap_or_default();
        let out = Command::new(NETWORKSETUP)
            .args(["-setdnsservers", &svc, "Empty"])
            .output()
            .map_err(|e| format!("调用 networksetup 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "{}: {}",
                svc,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        before.push((svc.clone(), old));
        done.push(svc);
    }
    Ok((done, before))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_packet_is_well_formed() {
        let q = build_query(0x1234, "www.baidu.com").expect("应能构造");
        assert_eq!(&q[0..2], &[0x12, 0x34]);
        assert_eq!(q[2], 0x01); // RD=1
        assert_eq!(u16::from_be_bytes([q[4], q[5]]), 1); // QDCOUNT
        // QNAME: 3www7baidu3com0
        assert_eq!(q[12], 3);
        assert_eq!(&q[13..16], b"www");
        assert_eq!(q[16], 5);
        assert_eq!(&q[17..22], b"baidu");
        assert_eq!(q[22], 3);
        assert_eq!(&q[23..26], b"com");
        assert_eq!(q[26], 0);
        // QTYPE=1 A, QCLASS=1 IN
        assert_eq!(u16::from_be_bytes([q[27], q[28]]), 1);
        assert_eq!(u16::from_be_bytes([q[29], q[30]]), 1);
        assert_eq!(q.len(), 31);
    }

    #[test]
    fn query_rejects_empty_and_oversized_labels() {
        assert!(build_query(1, "").is_none());
        let long = "a".repeat(64);
        assert!(build_query(1, &format!("{long}.com")).is_none());
    }

    #[test]
    fn header_requires_matching_id_and_qr_bit() {
        // 合法应答：QR=1 rcode=0 ancount=1
        let mut ok = vec![0u8; 12];
        ok[0] = 0x12;
        ok[1] = 0x34;
        ok[2] = 0x81;
        ok[3] = 0x00;
        ok[7] = 1;
        assert_eq!(parse_header(&ok, 0x1234), Some((0, 1)));
        // ID 不匹配 → None
        assert_eq!(parse_header(&ok, 0x9999), None);
        // QR=0（这是查询不是应答）→ None
        let mut query = ok.clone();
        query[2] = 0x01;
        assert_eq!(parse_header(&query, 0x1234), None);
        // 太短 → None
        assert_eq!(parse_header(&[0u8; 5], 0x1234), None);
    }

    #[test]
    fn servfail_is_not_ok() {
        let mut buf = vec![0u8; 12];
        buf[0] = 0x4D;
        buf[1] = 0x41;
        buf[2] = 0x81;
        buf[3] = 0x02; // rcode = SERVFAIL
        assert_eq!(parse_header(&buf, 0x4D41), Some((2, 0)));
        // probe 的判定式要求 (0, >0)，SERVFAIL 必被判失败
        assert!(!matches!(parse_header(&buf, 0x4D41), Some((0, an)) if an > 0));
    }

    #[test]
    fn nxdomain_is_not_ok() {
        let mut buf = vec![0u8; 12];
        buf[0] = 0x4D;
        buf[1] = 0x41;
        buf[2] = 0x81;
        buf[3] = 0x03; // rcode = NXDOMAIN
        assert!(!matches!(parse_header(&buf, 0x4D41), Some((0, an)) if an > 0));
    }

    #[test]
    fn probes_are_serializable_for_frontend() {
        let h = DnsHealth {
            ok: false,
            servers: vec!["192.168.148.128".into()],
            failing: vec!["192.168.148.128".into()],
            note: "DNS 服务器全部不应答".into(),
            looked_pinned: true,
            manual_services: vec!["Wi-Fi".into()],
        };
        let j = serde_json::to_value(&h).unwrap();
        // 前端契约：camelCase
        assert_eq!(j["lookedPinned"], true);
        assert_eq!(j["manualServices"][0], "Wi-Fi");
        assert!(j["failing"].is_array());
    }
}
