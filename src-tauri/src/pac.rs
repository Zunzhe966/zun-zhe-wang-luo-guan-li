//! PAC 白名单分流 + 失败自动直连（v0.4.2）
//!
//! ## 为什么不再用「系统 HTTP/HTTPS/SOCKS 代理」
//! v0.4.1 的做法是把三条通道对**每个网络服务**都指向 `127.0.0.1:7891`。
//! 这是一个**全局开关**：任何连接的第一步都是连 7891，连不上就直接失败——
//! mihomo 里那些 `GEOSITE,cn,DIRECT` 规则根本没机会执行。
//! 所以"90% 流量本来就走直连"这个设计，在内核死掉时**一点保护作用都没有**，
//! 断电重启后就是整机断网。
//!
//! ## 本模块的做法
//! 改用 PAC（自动代理配置）：
//! ```js
//! 命中白名单 → "PROXY 127.0.0.1:7891; DIRECT"   // 失败自动降级直连
//! 其余       → "DIRECT"                          // 物理直连，不经过内核
//! ```
//! 三条性质：
//! 1. **默认直连**：不在名单里的流量与内核生死无关。
//! 2. **fail-open**：`; DIRECT` 是 PAC 标准的多级回退，内核没起来只影响名单内站点。
//! 3. **断电安全**：PAC 只是一条指向本地文件的 URL，不需要常驻进程应答。
//!
//! ## 安全约束
//! - 白名单来自 `config.domain_rules`，是外部可控输入（订阅/手改），
//!   必须净化后再拼进 JS，绝不允许 `"` / `\` / 换行破坏字符串边界。
//! - PAC 服务只绑 `127.0.0.1`，端口在 18080 起顺延，绝不 `0.0.0.0`
//!   （否则局域网可拉取配置，泄露用户的白名单=泄露访问意图）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

/// PAC 默认端口（起不来会顺延 10 个）
pub const PAC_PORT: u16 = 18080;

/// 用户没有配任何域名规则时的默认白名单：只覆盖"确定需要出海"的常见站点。
/// 刻意保持小：名单越小，内核故障时的影响面越小——这正是本次改造的目的。
pub const DEFAULT_PROXY_DOMAINS: &[&str] = &[
    "openai.com",
    "anthropic.com",
    "claude.ai",
    "github.com",
    "githubusercontent.com",
    "google.com",
    "gstatic.com",
    "youtube.com",
    "x.com",
    "twitter.com",
    "telegram.org",
    "wikipedia.org",
];

/// 净化域名，只保留域名合法字符。
/// 与 mihomo 规则侧同一立场：外部输入若含引号/反斜杠/换行，会注入 PAC JS。
pub fn sanitize_domain(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

/// 生成 PAC 脚本。`proxy_port` 是 mihomo 的混合端口（默认 7891）。
pub fn build_pac(proxy_port: u16, domains: &[String]) -> String {
    let mut list: Vec<String> = Vec::new();
    for d in domains {
        let c = sanitize_domain(d);
        if c.is_empty() || !c.contains('.') {
            continue; // 空/不含点：不是可用的域名后缀
        }
        if !list.contains(&c) {
            list.push(c);
        }
    }
    let arr = list
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "// 尊者网络管理 自动生成，请勿手改。\n\
         // 命中白名单走代理；连不上代理时自动回退直连（fail-open）。\n\
         function FindProxyForURL(url, host) {{\n\
         \x20 host = host.toLowerCase();\n\
         \x20 var white = [{arr}];\n\
         \x20 for (var i = 0; i < white.length; i++) {{\n\
         \x20   var d = white[i];\n\
         \x20   if (host === d || dnsDomainIs(host, \".\" + d)) {{\n\
         \x20     return \"PROXY 127.0.0.1:{proxy_port}; DIRECT\";\n\
         \x20   }}\n\
         \x20 }}\n\
         \x20 return \"DIRECT\";\n\
         }}\n"
    )
}

/// 从配置里抽取"需要走代理"的域名清单。
/// `target == "direct"` 的显式直连规则不进来；节点名目标也算走代理。
pub fn proxy_domains_from_config(cfg: &crate::config::AppConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in &cfg.domain_rules {
        if r.target == "direct" {
            continue;
        }
        let d = sanitize_domain(&r.domain);
        if !d.is_empty() {
            out.push(d);
        }
    }
    if out.is_empty() {
        // 没有任何规则 → 用默认小名单，避免"开了代理却什么都不生效"的空转
        return DEFAULT_PROXY_DOMAINS.iter().map(|s| s.to_string()).collect();
    }
    out
}

/// 内存态 PAC 服务。内容可热更新（改白名单不必重启监听）。
pub struct PacServer {
    pub url: String,
    pub port: u16,
    body: Arc<Mutex<String>>,
}

impl PacServer {
    /// 在 127.0.0.1 上起服务；端口被占则顺延。
    pub fn start(body: String, from_port: u16) -> Result<PacServer, String> {
        let mut last_err = String::new();
        for port in from_port..from_port.saturating_add(10) {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => {
                    let shared = Arc::new(Mutex::new(body));
                    let body_for_thread = shared.clone();
                    std::thread::spawn(move || {
                        for conn in listener.incoming() {
                            let Ok(stream) = conn else { continue };
                            let b = body_for_thread.clone();
                            // 每连接一个短线程：PAC 请求很少，无需线程池
                            std::thread::spawn(move || serve_one(stream, b));
                        }
                    });
                    return Ok(PacServer {
                        url: format!("http://127.0.0.1:{port}/proxy.pac"),
                        port,
                        body: shared,
                    });
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(format!("PAC 服务无法绑定 127.0.0.1:{from_port}+: {last_err}"))
    }

    pub fn update(&self, body: String) {
        if let Ok(mut g) = self.body.lock() {
            *g = body;
        }
    }

    pub fn body_snapshot(&self) -> String {
        self.body.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

fn serve_one(mut stream: TcpStream, body: Arc<Mutex<String>>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(800)));
    let mut buf = [0u8; 512];
    let _ = stream.read(&mut buf); // 只读请求行，够判断路径即可
    let req = String::from_utf8_lossy(&buf);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let content = if path.starts_with("/proxy.pac") {
        body.lock().map(|g| g.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    let status = if path.starts_with("/proxy.pac") {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/x-ns-proxy-autoconfig\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{content}",
        content.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

static SERVER: OnceLock<Mutex<Option<PacServer>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<PacServer>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

/// 确保 PAC 服务在跑（幂等），并更新内容。返回可下发给系统的 PAC URL。
pub fn ensure_running(body: String) -> Result<String, String> {
    let mut g = slot().lock().map_err(|_| "PAC 状态锁中毒".to_string())?;
    match g.as_ref() {
        Some(s) => {
            s.update(body);
            Ok(s.url.clone())
        }
        None => {
            let s = PacServer::start(body, PAC_PORT)?;
            let url = s.url.clone();
            *g = Some(s);
            Ok(url)
        }
    }
}

/// 当前 PAC URL（未启动则为 None）。自愈/收尾路径用它判断要不要清理。
pub fn current_url() -> Option<String> {
    slot().lock().ok().and_then(|g| g.as_ref().map(|s| s.url.clone()))
}

pub fn stop() {
    if let Ok(mut g) = slot().lock() {
        *g = None; // 释放监听：线程随 listener drop 退出
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pac_is_fail_open_by_default() {
        let pac = build_pac(7891, &["github.com".to_string()]);
        // 白名单命中必须是 "PROXY xxx; DIRECT"——分号后的 DIRECT 就是 fail-open 的关键
        assert!(
            pac.contains("return \"PROXY 127.0.0.1:7891; DIRECT\";"),
            "白名单必须带 DIRECT 回退:\n{pac}"
        );
        // 未命中一律直连
        assert!(pac.contains("return \"DIRECT\";"), "{pac}");
        // 只有两个 return：不存在"未命中还走代理"的分支
        assert_eq!(pac.matches("return ").count(), 2, "{pac}");
    }

    #[test]
    fn pac_never_proxies_everything() {
        // 回归锁：哪怕白名单为空，也不能退化成"全局代理"
        let pac = build_pac(7891, &[]);
        assert!(pac.contains("var white = [];"));
        assert!(pac.contains("return \"DIRECT\";"));
        assert!(!pac.contains("PROXY 127.0.0.1:7891\";"), "不得出现无条件 PROXY");
    }

    #[test]
    fn malicious_domains_cannot_break_out_of_js_string() {
        // 外部输入（订阅/手改配置）试图闭合字符串并注入代码
        let evil = vec![
            "evil.com\",return\"PROXY 1.1.1.1:1".to_string(),
            "a.com\";alert(1);//".to_string(),
            "b.com\\\";return\"DIRECT".to_string(),
            "line.com\nreturn \"PROXY x:1\"".to_string(),
        ];
        let pac = build_pac(7891, &evil);
        assert!(!pac.contains("alert(1)"), "注入未净化:\n{pac}");
        assert!(!pac.contains("PROXY 1.1.1.1:1"), "注入未净化:\n{pac}");
        assert!(!pac.contains("return \"PROXY x:1\""), "换行注入:\n{pac}");
        // 引号/反斜杠/换行都必须被剔除，只留字母数字点横线下划线
        let body = pac.split("var white = [").nth(1).unwrap().split(']').next().unwrap();
        assert!(
            body.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '"' | ',' | '.' | '-' | '_')),
            "白名单里出现了非法字符: {body}"
        );
    }

    #[test]
    fn domains_without_dot_are_dropped() {
        let pac = build_pac(7891, &["localhost".into(), "com".into(), "ok.com".into()]);
        assert!(pac.contains("\"ok.com\""));
        assert!(!pac.contains("\"localhost\""));
        assert!(!pac.contains("\"com\""));
    }

    #[test]
    fn config_direct_rules_are_excluded() {
        use crate::config::{AppConfig, DomainRule};
        let cfg = AppConfig {
            domain_rules: vec![
                DomainRule { domain: "github.com".into(), target: "proxy".into(), reason: String::new() },
                DomainRule { domain: "bilibili.com".into(), target: "direct".into(), reason: String::new() },
                DomainRule { domain: "openai.com".into(), target: "我的节点".into(), reason: String::new() },
            ],
            ..Default::default()
        };
        let d = proxy_domains_from_config(&cfg);
        assert!(d.contains(&"github.com".to_string()));
        assert!(d.contains(&"openai.com".to_string()), "节点名目标也应走代理");
        assert!(!d.contains(&"bilibili.com".to_string()), "显式直连不得进白名单");
    }

    #[test]
    fn empty_rules_fall_back_to_default_whitelist() {
        let cfg = crate::config::AppConfig::default();
        let d = proxy_domains_from_config(&cfg);
        assert!(!d.is_empty(), "空规则时应给默认小名单，避免空转");
        assert!(d.contains(&"github.com".to_string()));
    }

    #[test]
    fn server_serves_pac_on_loopback_only() {
        let s = PacServer::start(build_pac(7891, &["a.com".into()]), 18190).expect("应能起服务");
        assert!(s.url.starts_with("http://127.0.0.1:"));
        // 端口必须只绑回环
        assert!(TcpStream::connect(("127.0.0.1", s.port)).is_ok());
        s.update(build_pac(7891, &["b.com".into()]));
        assert!(s.body_snapshot().contains("\"b.com\""));
    }
}
