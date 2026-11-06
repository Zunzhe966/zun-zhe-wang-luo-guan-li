use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyNode {
    pub name: String,
    pub server: String,
    pub port: u16,
    pub uuid: String,
    pub flow: String,
    pub network: String,
    pub tls: bool,
    pub udp: bool,
    pub fingerprint: String,
    pub public_key: String,
    pub short_id: String,
    pub sni: String,
    /// 节点来源："manual" 手动添加 | "subscription" 订阅拉取
    #[serde(default = "default_source")]
    pub source: String,
    /// 地区标签（如 美国/日本/香港），空则展示时从名称猜测
    #[serde(default)]
    pub region: String,
    /// 节点协议（P2-11，v0.4.1）："vless"（默认）| "vmess" | "trojan"。
    /// 订阅解析按协议分支生成 mihomo 配置；手动添加默认 vless。
    #[serde(default = "default_proto")]
    pub proto: String,
    /// trojan 节点密码（proto=trojan 时使用；其余协议为空）。
    #[serde(default)]
    pub password: String,
    /// vmess WebSocket 路径（proto=vmess 且 network=ws 时使用；空则不带 ws-opts）。
    #[serde(default)]
    pub ws_path: String,
}

fn default_proto() -> String {
    "vless".to_string()
}

fn default_source() -> String {
    "manual".to_string()
}

/// 软件分流设置：两态模型，没有"智能"。
/// 每个软件由用户与 AI 探讨后确认：直连，或走代理（可指定到具体节点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSetting {
    pub id: String,
    /// "direct" 直连 | "proxy" 走代理
    pub mode: String,
    /// mode=proxy 时指定的节点名；None 表示用"当前选中节点"
    #[serde(default)]
    pub node: Option<String>,
    /// 为什么这么定（AI 探讨时给出的理由，留在表里可查）
    #[serde(default)]
    pub reason: String,
    /// 用户确认过才生效；未确认的一律按直连处理
    #[serde(default)]
    pub confirmed: bool,
}

/// SSH 服务器连接信息（不存明文密码/私钥，只存标记）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: String, // "password" | "key"
    pub password_saved: bool,
    pub private_key_saved: bool,
    #[serde(default)]
    pub key_path: Option<String>,
}

/// 域名分流规则：按域名后缀指定去向，优先级高于国内域名清单（GEOSITE,cn）和进程规则。
/// 解决同一个软件内部混合源的问题（如下载器：GitHub 走代理、国内源直连）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainRule {
    pub domain: String, // 如 github.com、huggingface.co
    pub target: String, // "proxy" 走代理 | "direct" 直连 | 节点名（走该节点）
    /// 服务于哪个密钥/软件（如"WorkBuddy 的 OpenRouter 密钥"），防止日久失忆误删
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub nodes: Vec<ProxyNode>,
    #[serde(alias = "selected_node")]
    pub selected_node: Option<String>,
    pub apps: Vec<AppSetting>,
    #[serde(default, alias = "system_proxy")]
    pub system_proxy: bool,
    #[serde(alias = "auto_global")]
    pub auto_global: String,
    #[serde(default, alias = "subscription_url")]
    pub subscription_url: Option<String>,
    #[serde(default)]
    pub servers: Vec<ServerInfo>,
    #[serde(default, alias = "active_server_id")]
    pub active_server_id: Option<String>,
    #[serde(default)]
    pub domain_rules: Vec<DomainRule>,
    /// mihomo 控制 API（127.0.0.1:19091）的鉴权 secret。
    /// 缺失时在 load() 自动生成并持久化，防止本机任意进程/网页 CSRF 操控代理。
    #[serde(default, alias = "api_secret")]
    pub api_secret: Option<String>,
    /// 更新通道：`local`（本地开发测试，127.0.0.1:7878）
    /// 或 `github`（GitHub Releases，面向真实用户）。
    /// None / 未知值一律按 github 处理（面向用户更安全）。
    #[serde(default, alias = "update_channel")]
    pub update_channel: Option<String>,
    // 兼容旧配置：仍保留这几个字段，但新逻辑不再把明文写进 config.json
    #[serde(alias = "ssh_host")]
    pub ssh_host: Option<String>,
    #[serde(alias = "ssh_port")]
    pub ssh_port: Option<u16>,
    #[serde(alias = "ssh_user")]
    pub ssh_user: Option<String>,
    #[serde(alias = "ssh_auth")]
    pub ssh_auth: Option<String>,
    #[serde(alias = "ssh_password")]
    pub ssh_password: Option<String>,
    #[serde(alias = "ssh_private_key")]
    pub ssh_private_key: Option<String>,
    /// P1-3 确认模式：启动代理前先列示网络混乱源，用户过目后才执行接管。
    /// false（默认）= 快速模式，沿用直接接管行为。
    #[serde(default, alias = "confirm_takeover")]
    pub confirm_takeover: bool,
    /// P2-1 双引擎停用仲裁（v0.4.1）：MCP 侧 stop_proxy / restore_network 会把此标记
    /// 置 true 落盘（App 内存 should_run 改不了），App 看门狗读到此标记 = 用户/AI
    /// 明确要停，绝不 30 秒内自动复活内核。start_proxy（含已运行分支）置回 false。
    #[serde(default, alias = "user_stopped")]
    pub user_stopped: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // 节点默认空：不硬编码任何真实节点信息（隐私），由用户手动添加或订阅拉取
            nodes: vec![],
            selected_node: None,
            apps: vec![],
            system_proxy: true,
            auto_global: "auto".to_string(),
            subscription_url: None,
            servers: vec![],
            active_server_id: None,
            domain_rules: vec![],
            api_secret: None,
            user_stopped: false,
            // 缺省 GitHub 通道：开源版面向真实用户
            update_channel: Some("github".to_string()),
            ssh_host: None,
            ssh_port: Some(22),
            ssh_user: Some("root".to_string()),
            ssh_auth: Some("password".to_string()),
            ssh_password: None,
            ssh_private_key: None,
            confirm_takeover: false,
        }
    }
}

impl AppConfig {
    /// 当前激活的 SSH 服务器（优先 active_server_id，其次 servers 首项和旧字段）
    pub fn active_server(&self) -> Option<ServerInfo> {
        if let Some(id) = &self.active_server_id {
            if let Some(s) = self.servers.iter().find(|s| &s.id == id) {
                return Some(s.clone());
            }
        }
        if let Some(s) = self.servers.first() {
            return Some(s.clone());
        }
        // 旧字段兼容
        let host = self.ssh_host.clone().or_else(|| {
            // SSH 主机未配置时，从选中的代理节点推导——
            // 用户的代理节点就部署在云服务器上，SSH 和代理是同一台机器，
            // 不再要求用户在 SSH 页面重新填一遍服务器地址。
            let selected = self.selected_node.as_ref()?;
            let node = self.nodes.iter().find(|n| &n.name == selected)?;
            Some(node.server.clone())
        })?;
        Some(ServerInfo {
            id: format!("ssh-{}", host),
            name: host.clone(),
            host,
            port: self.ssh_port.unwrap_or(22),
            user: self.ssh_user.clone().unwrap_or_else(|| "root".to_string()),
            auth: self
                .ssh_auth
                .clone()
                .unwrap_or_else(|| "password".to_string()),
            password_saved: self
                .ssh_password
                .as_deref()
                .map(|p| !p.is_empty())
                .unwrap_or(false),
            private_key_saved: self
                .ssh_private_key
                .as_deref()
                .map(|k| !k.is_empty())
                .unwrap_or(false),
            key_path: self.ssh_private_key.clone(),
        })
    }
}

pub fn config_path() -> PathBuf {
    let dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.join("magic-agent").join("config.json")
}

/// 把 JSON object 的所有键从 snake_case 规整为 camelCase，递归处理嵌套对象。
/// 仅处理 object 的 key，不动 value（包括字符串值里的下划线）。
/// 用于兼容旧 config.json：早期版本部分键用 snake_case 写入，而 AppConfig
/// 用 rename_all="camelCase" 反序列化；不规整则 snake_case 键匹配不上字段。
fn normalize_keys_to_camel(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // 先收集所有键，再逐个替换（避免遍历时修改 map）
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let new_k = snake_to_camel(&k);
                if new_k != k {
                    if let Some(v) = map.remove(&k) {
                        // 若 camelCase 键已存在，保留后写入的那个（即蛇形的旧值被新值覆盖）；
                        // serde_json Value 解析重复键时已取最后一个，这里再插入不会丢数据。
                        map.insert(new_k, v);
                    }
                }
            }
            // 递归处理所有值
            for v in map.values_mut() {
                normalize_keys_to_camel(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_keys_to_camel(v);
            }
        }
        _ => {}
    }
}

/// snake_case → camelCase。只按下划线分割，首段全小写、后续每段首字母大写。
/// 已经是 camelCase（不含下划线）的原样返回。
fn snake_to_camel(s: &str) -> String {
    if !s.contains('_') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut upper_next = false;
    for (i, c) in s.chars().enumerate() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else if i == 0 {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 从字符串解析配置：
///   1) 先 parse 成 serde_json::Value：Value 解析器对重复键取最后一个值、不报错——
///      避免旧版遗留的 snake_case 键与新版 camelCase 键共存时，被字符串替换制造出
///      重复键、serde 直接 Err、整份配置被判定损坏、用户数据全丢。
///   2) 递归把键从 snake_case 规整为 camelCase，兼容旧配置。
///   3) from_value 反序列化为 AppConfig；类型不匹配等仍会 Err。
fn parse_config_str(s: &str) -> Result<AppConfig, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("JSON 语法错误: {e}"))?;
    let mut value = parsed;
    normalize_keys_to_camel(&mut value);
    serde_json::from_value::<AppConfig>(value).map_err(|e| format!("结构不兼容: {e}"))
}

/// 备份坏文件，但**只在备份不存在时**写入——避免二次事故把仅存的好备份覆盖掉。
fn backup_once(p: &std::path::Path) {
    let bak = p.with_extension("json.corrupt");
    if !bak.exists() {
        let _ = std::fs::copy(p, &bak);
    }
}

/// 尝试从 .corrupt 备份恢复配置。
fn recover_from_backup(p: &std::path::Path) -> Option<AppConfig> {
    let bak = p.with_extension("json.corrupt");
    let s = std::fs::read_to_string(&bak).ok()?;
    parse_config_str(&s).ok()
}

pub fn load() -> AppConfig {
    let p = config_path();

    // 文件不存在 → 首次运行：生成默认配置并落盘（安全，不会覆盖任何用户数据）
    let text = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(_) => {
            let mut cfg = AppConfig::default();
            cfg.api_secret = Some(generate_api_secret());
            let _ = save(&cfg);
            return cfg;
        }
    };

    match parse_config_str(&text) {
        Ok(mut cfg) => {
            // 正常解析：补齐 API secret（mihomo external-controller 的鉴权令牌）后返回
            if cfg
                .api_secret
                .as_deref()
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                cfg.api_secret = Some(generate_api_secret());
                let _ = save(&cfg);
            }
            cfg
        }
        Err(e) => {
            // 解析失败 / 文件为空：只备份（不覆盖已有备份）并尝试从备份自愈。
            // 关键：**绝不在此把空配置写回用户文件**——那正是"数据消失"的元凶。
            eprintln!("[magic-agent] config.json 解析失败，暂不回写用户文件: {e}");
            backup_once(&p);
            match recover_from_backup(&p) {
                Some(mut cfg) => {
                    eprintln!("[magic-agent] 已从 config.json.corrupt 自愈恢复");
                    if cfg
                        .api_secret
                        .as_deref()
                        .map(|s| s.is_empty())
                        .unwrap_or(true)
                    {
                        cfg.api_secret = Some(generate_api_secret());
                    }
                    let _ = save(&cfg);
                    cfg
                }
                // 无法恢复：仅在内存兜底，不动磁盘上的用户文件
                None => AppConfig::default(),
            }
        }
    }
}

/// 从 /dev/urandom 读 16 字节转 hex，无第三方依赖。
/// 失败时用当前纳秒时间戳 + 进程 id + 计数器异或打散，避免退化成全 0 的弱密钥。
fn generate_api_secret() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let mut filled = false;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            filled = true;
        }
    }
    if !filled {
        // 兜底：时间戳/进程号/地址异或，仍比全 0 强，且每次调用都不同
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = std::process::id() as u64;
        let addr = &buf as *const _ as u64;
        let mut seed = now ^ pid.rotate_left(17) ^ addr.rotate_left(31) ^ 0x9e3779b97f4a7c15u64;
        for b in buf.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn save(cfg: &AppConfig) -> Result<(), String> {
    let p = config_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 原子写：临时文件+rename，防止与 Python(MCP) 并发写时出现半截 JSON
    let tmp = p.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // 配置含 apiSecret 与节点 UUID/公钥，必须收紧为 0600。
    // 关键：chmod 必须在 rename 之前——若先 rename 再 chmod，rename 完成到
    // chmod 执行之间会有一个可被其他用户读取 secret 的短窗口（tmp 默认 0644）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
    Ok(())
}

/// 校验订阅 URL 的 host 是否指向公网，拒绝回环/内网/链路本地/组播/保留地址，
/// 防止恶意前端借 fetch_subscription 触发 SSRF 拉取内网内容。
/// 返回 Ok(()) 表示可安全访问；Err(msg) 表示被拦截。
pub fn validate_public_host(host: &str) -> Result<(), String> {
    // 去端口（IPv6 用方括号包裹）
    let h = host.trim();
    let host_only = if h.starts_with('[') {
        // [::1]:8080 形式
        match h.find(']') {
            Some(i) => &h[1..i],
            None => h,
        }
    } else if h.matches(':').count() > 1 {
        // 裸 IPv6（无方括号、无端口），如 ::1、fe80::1
        h
    } else {
        // host 或 host:port；IPv4 直接按冒号分隔，域名无冒号
        match h.rfind(':') {
            Some(i) if h[i + 1..].chars().all(|c| c.is_ascii_digit()) => &h[..i],
            _ => h,
        }
    };
    if host_only.is_empty() {
        return Err("订阅地址 host 为空".to_string());
    }
    // 字面 IP：直接判内网/保留地址
    if let Ok(ip) = host_only.parse::<std::net::IpAddr>() {
        if is_private_or_reserved(ip) {
            return Err(format!("订阅地址指向内网/保留地址 {}，已拦截", ip));
        }
        return Ok(());
    }
    // 域名：做 DNS 解析，任一解析结果落到内网/保留地址即拦截，
    // 堵住「域名解析到内网」的 SSRF 绕过（如 http://intranet.corp.local/）。
    // 注意：这里只做静态解析校验；真正的拉取仍走 curl，需配合 --resolve 或在
    // 解析后重连才 100% 防 DNS rebinding，但本机单人场景下先拦常见绕过。
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host_only, 443)) {
        for a in addrs {
            if is_private_or_reserved(a.ip()) {
                return Err(format!(
                    "订阅域名 {} 解析到内网/保留地址 {}，已拦截",
                    host_only,
                    a.ip()
                ));
            }
        }
    }
    Ok(())
}

/// 判断 IP 是否为回环/内网/链路本地/组播/未指定等非公网地址。
pub fn is_private_or_reserved(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 100.64.0.0/10 运营商级 NAT（CGNAT），非公网
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // IPv4-mapped IPv6 内网地址（::ffff:127.0.0.1 等）
                || v6.to_ipv4().map(is_private_or_reserved_ipv4).unwrap_or(false)
                // 唯一本地地址 fc00::/7（含 fd00::/8）
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // 链路本地 fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

pub fn is_private_or_reserved_ipv4(v4: std::net::Ipv4Addr) -> bool {
    is_private_or_reserved(std::net::IpAddr::V4(v4))
}

/// 从订阅文本中解析代理节点（P2-11，v0.4.1 起支持 vless / vmess / trojan）。
/// 订阅内容可能是：明文 vless:// / vmess:// / trojan:// 链接、每行一个，
/// 或 base64 编码的整段内容。
pub fn parse_subscription(text: &str) -> Result<Vec<ProxyNode>, String> {
    // 文本里没有任何已知协议前缀时，尝试 base64 解码（macOS 自带 base64 -D）
    let mut content = text.to_string();
    if !content.contains("vless://")
        && !content.contains("vmess://")
        && !content.contains("trojan://")
    {
        let cleaned: String = content.chars().filter(|c| !c.is_whitespace()).collect();
        if !cleaned.is_empty() {
            let out = std::process::Command::new("/usr/bin/base64")
                .arg("-D")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    if let Some(mut stdin) = c.stdin.take() {
                        let _ = stdin.write_all(cleaned.as_bytes());
                    }
                    c.wait_with_output()
                });
            if let Ok(out) = out {
                if out.status.success() {
                    if let Ok(decoded) = String::from_utf8(out.stdout) {
                        content = decoded;
                    }
                }
            }
        }
    }

    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in content.lines() {
        let line = line.trim();
        let mut offset = 0;
        while let Some((idx, _proto)) = ["vless://", "vmess://", "trojan://"]
            .iter()
            .filter_map(|p| line[offset..].find(p).map(|i| (i + offset, p)))
            .min_by_key(|(i, _)| *i)
        {
            let uri = &line[idx..];
            let node = if uri.starts_with("vless://") {
                // parse_vless_uri 内部自行 strip 前缀（旧函数契约）
                parse_vless_uri(uri)
            } else if let Some(rest) = uri.strip_prefix("vmess://") {
                parse_vmess_uri(rest)
            } else if let Some(rest) = uri.strip_prefix("trojan://") {
                parse_trojan_uri(rest)
            } else {
                Err("未知协议".to_string())
            };
            if let Ok(n) = node {
                // 按 server:port 去重：订阅可能重复返回同一节点，避免重复添加
                let key = format!("{}:{}", n.server, n.port);
                if seen.insert(key) {
                    nodes.push(n);
                }
            }
            // 跳到本行末尾（每行一个链接，避免重复扫描同一行）
            offset = line.len();
        }
    }
    if nodes.is_empty() {
        return Err("未能从订阅中解析出任何节点（支持 vless://、vmess://、trojan://）".to_string());
    }
    Ok(nodes)
}

/// 兼容旧入口：旧调用点名称（lib.rs fetch_subscription_blocking）保留转发。
pub fn parse_vless_subscription(text: &str) -> Result<Vec<ProxyNode>, String> {
    parse_subscription(text)
}

/// 解析单个 vmess:// URI 为 ProxyNode。
/// 标准 V2Ray share link：vmess://<base64(JSON)>，JSON 字段
/// {v, ps(名称), add(地址), port, id(uuid), aid, net, type, host, path, tls, sni, fp}。
fn parse_vmess_uri(rest: &str) -> Result<ProxyNode, String> {
    // vmess:// 后是 base64（可能带尾随字符）
    let b64: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    if b64.is_empty() {
        return Err("vmess base64 为空".to_string());
    }
    let out = std::process::Command::new("/usr/bin/base64")
        .arg("-D")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(b64.as_bytes());
            }
            c.wait_with_output()
        })
        .map_err(|e| format!("base64 子进程失败: {e}"))?;
    if !out.status.success() {
        return Err("vmess base64 解码失败（JSON 应可解码）".to_string());
    }
    let json_text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&json_text).map_err(|e| format!("vmess JSON 解析失败: {e}"))?;
    let host = v.get("add").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let port: u16 = v
        .get("port")
        .and_then(|x| {
            x.as_str()
                .map(|s| s.to_string())
                .or_else(|| x.as_u64().map(|n| n.to_string()))
        })
        .and_then(|s| s.parse().ok())
        .ok_or("vmess 端口无效")?;
    if host.is_empty() {
        return Err("vmess 缺少 add 字段".to_string());
    }
    let name = v.get("ps").and_then(|x| x.as_str()).unwrap_or(&host).to_string();
    let network = v.get("net").and_then(|x| x.as_str()).unwrap_or("tcp").to_string();
    let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let tls = v.get("tls").and_then(|x| x.as_str()).map(|s| s == "tls" || s == "true").unwrap_or(false);
    let sni = v.get("sni").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok(ProxyNode {
        region: guess_region(&name),
        name,
        server: host,
        port,
        uuid: v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        flow: String::new(),
        network,
        tls,
        udp: true,
        fingerprint: v.get("fp").and_then(|x| x.as_str()).unwrap_or("chrome").to_string(),
        public_key: String::new(),
        short_id: String::new(),
        sni,
        source: "subscription".to_string(),
        proto: "vmess".to_string(),
        password: String::new(),
        ws_path: path,
    })
}

/// 解析单个 trojan:// URI 为 ProxyNode。
/// trojan://password@host:port?security=tls&sni=...&type=tcp#name
/// 注意顺序：先拆 fragment（#），再拆 query（?）——否则 host:port#name 的
/// 端口串会混入 fragment 导致解析失败（实测 bug，已由单测锁住）。
fn parse_trojan_uri(rest: &str) -> Result<ProxyNode, String> {
    let (no_frag, fragment) = match rest.find('#') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let (auth_part, query) = match no_frag.find('?') {
        Some(i) => (&no_frag[..i], &no_frag[i + 1..]),
        None => (no_frag, ""),
    };
    let (password, hostport) = auth_part.rsplit_once('@').ok_or("trojan 缺少 @")?;
    let (host, port_str) = hostport.rsplit_once(':').ok_or("trojan 缺少端口")?;
    let port: u16 = port_str.parse().map_err(|_| "trojan 端口无效")?;
    let mut params = std::collections::HashMap::new();
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            params.insert(k, url_decode(v));
        }
    }
    let name = if fragment.is_empty() {
        host.to_string()
    } else {
        url_decode(fragment)
    };
    Ok(ProxyNode {
        region: guess_region(&name),
        name,
        server: host.to_string(),
        port,
        uuid: String::new(),
        flow: String::new(),
        network: params.get("type").cloned().unwrap_or_else(|| "tcp".to_string()),
        tls: params.get("security").map(|s| s == "tls" || s == "reality").unwrap_or(false),
        udp: true,
        fingerprint: String::new(),
        public_key: String::new(),
        short_id: String::new(),
        sni: params.get("sni").cloned().unwrap_or_default(),
        source: "subscription".to_string(),
        proto: "trojan".to_string(),
        password: password.to_string(),
        ws_path: String::new(),
    })
}

/// 解析单个 vless:// URI 为 ProxyNode。
fn parse_vless_uri(uri: &str) -> Result<ProxyNode, String> {
    let rest = uri.strip_prefix("vless://").ok_or("不是 vless 链接")?;
    // 分离 query 和 fragment
    let (auth_part, after) = match rest.find('?') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let (query, fragment) = match after.find('#') {
        Some(i) => (&after[..i], &after[i + 1..]),
        None => (after, ""),
    };

    // auth_part: uuid@host:port
    let (userinfo, hostport) = auth_part.rsplit_once('@').ok_or("缺少 @")?;
    let (host, port_str) = hostport.rsplit_once(':').ok_or("缺少端口")?;
    let port: u16 = port_str.parse().map_err(|_| "端口无效")?;

    // 解析 query 参数
    let mut params = std::collections::HashMap::new();
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            params.insert(k, url_decode(v));
        }
    }

    let name = if fragment.is_empty() {
        host.to_string()
    } else {
        url_decode(fragment)
    };

    Ok(ProxyNode {
        region: guess_region(&name),
            proto: "vless".to_string(),
            password: String::new(),
            ws_path: String::new(),
        name,
        server: host.to_string(),
        port,
        uuid: userinfo.to_string(),
        flow: params.get("flow").cloned().unwrap_or_default(),
        network: params
            .get("type")
            .cloned()
            .unwrap_or_else(|| "tcp".to_string()),
        tls: params
            .get("security")
            .map(|s| s == "reality" || s == "tls")
            .unwrap_or(false),
        udp: true,
        fingerprint: params
            .get("fp")
            .cloned()
            .unwrap_or_else(|| "chrome".to_string()),
        public_key: params.get("pbk").cloned().unwrap_or_default(),
        short_id: params.get("sid").cloned().unwrap_or_default(),
        sni: params.get("sni").cloned().unwrap_or_default(),
        source: "subscription".to_string(),
    })
}

/// 从节点名称猜测地区标签（用于分组展示）
pub fn guess_region(name: &str) -> String {
    let n = name.to_lowercase();
    let pairs = [
        (
            "美国",
            vec![
                "美国",
                "美",
                "us",
                "usa",
                "america",
                "texas",
                "硅谷",
                "洛杉矶",
                "纽约",
            ],
        ),
        (
            "日本",
            vec!["日本", "日", "jp", "japan", "tokyo", "东京", "大阪"],
        ),
        ("香港", vec!["香港", "港", "hk", "hongkong", "hong kong"]),
        ("台湾", vec!["台湾", "台", "tw", "taiwan", "台北"]),
        ("新加坡", vec!["新加坡", "新", "sg", "singapore", "狮城"]),
        ("韩国", vec!["韩国", "韩", "kr", "korea", "首尔"]),
        ("英国", vec!["英国", "英", "uk", "britain", "伦敦"]),
        ("德国", vec!["德国", "德", "de", "germany", "法兰克福"]),
    ];
    for (region, keys) in pairs {
        for k in keys {
            if n.contains(k) {
                return region.to_string();
            }
        }
    }
    String::new()
}

/// 简易 URL 百分号解码（%XX）
fn url_decode(s: &str) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // 只按字节判断，绝不切片 str：
        // 旧实现 `&s[i+1..i+2]` 在 "%" 后随多字节 UTF-8（如 "%中"）时会按字节切进
        // 字符内部，直接 panic——畸形订阅链接即可让 App 崩溃。
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_to_camel_works() {
        assert_eq!(snake_to_camel("system_proxy"), "systemProxy");
        assert_eq!(snake_to_camel("update_channel"), "updateChannel");
        assert_eq!(snake_to_camel("api_secret"), "apiSecret");
        assert_eq!(snake_to_camel("selected_node"), "selectedNode");
        // 已经是 camelCase 的不动
        assert_eq!(snake_to_camel("systemProxy"), "systemProxy");
        assert_eq!(snake_to_camel("nodes"), "nodes");
        // 连续下划线
        assert_eq!(snake_to_camel("a__b"), "aB");
    }

    #[test]
    fn normalize_keys_handles_duplicate_snake_and_camel() {
        // 回归：旧版遗留 snake_case 键与新版 camelCase 键共存时，
        // 旧的字符串替换会产生重复键导致 serde Err、整份配置被清空。
        // 现在先 parse 成 Value（重复键取最后一个），再规整键名，不应失败。
        let json = r#"{
            "systemProxy": true,
            "autoGlobal": "auto",
            "nodes": [],
            "apps": [],
            "system_proxy": false,
            "update_channel": "local"
        }"#;
        let mut value: serde_json::Value = serde_json::from_str(json).unwrap();
        normalize_keys_to_camel(&mut value);
        let cfg: AppConfig = serde_json::from_value(value).expect("应能解析含重复键的配置");
        // 重复键取最后一个值：system_proxy=false 在 systemProxy=true 之后
        assert_eq!(cfg.system_proxy, false);
        assert_eq!(cfg.update_channel.as_deref(), Some("local"));
        assert_eq!(cfg.auto_global, "auto");
    }

    #[test]
    fn load_recovers_real_corrupt_file() {
        // 用本机真实的 .corrupt 文件端到端验证：修复后 load() 应能恢复所有用户数据，
        // 不再因重复键回退默认配置。文件不存在则跳过（CI/其他机器）。
        // 路径运行时构造（与 config_path 同源），禁止硬编码用户名——本文件会开源推送。
        let p = config_path().with_extension("json.corrupt");
        if !p.exists() {
            eprintln!("跳过：.corrupt 文件不存在");
            return;
        }
        let s = std::fs::read_to_string(&p).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&s).expect("Value 应解析成功");
        normalize_keys_to_camel(&mut value);
        let cfg: AppConfig =
            serde_json::from_value(value).expect("修复后应能反序列化含重复键的旧配置");
        assert!(
            !cfg.nodes.is_empty(),
            "恢复后节点不应为空（原配置有节点）"
        );
        assert!(
            !cfg.apps.is_empty(),
            "恢复后软件分流不应为空"
        );
        assert!(
            !cfg.domain_rules.is_empty(),
            "恢复后域名规则不应为空"
        );
        assert!(
            !cfg.servers.is_empty(),
            "恢复后 SSH 服务器不应为空"
        );
        eprintln!(
            "恢复成功：nodes={} apps={} servers={} domainRules={} selectedNode={:?} channel={:?}",
            cfg.nodes.len(),
            cfg.apps.len(),
            cfg.servers.len(),
            cfg.domain_rules.len(),
            cfg.selected_node,
            cfg.update_channel
        );
    }

    #[test]
    fn self_heal_from_corrupt_backup() {
        // 端到端（在临时目录里、不碰线上文件）：
        //   * backup_once 只在无备份时写，已有 .corrupt 不被坏文件覆盖；
        //   * 解析失败时用坏文件副本触发自愈；
        //   * recover_from_backup 能从真实 .corrupt 恢复全部用户数据。
        // 路径运行时构造（与 config_path 同源），禁止硬编码用户名——本文件会开源推送。
        let real_bak = config_path().with_extension("json.corrupt");
        if !real_bak.exists() {
            eprintln!("跳过：真实 .corrupt 不存在");
            return;
        }
        let dir = std::env::temp_dir().join(format!("magic-agent-heal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let bak = p.with_extension("json.corrupt");

        // 好备份 = 真实 .corrupt；当前 config.json = 语法错
        std::fs::copy(&real_bak, &bak).unwrap();
        std::fs::write(&p, "{ this is not valid json").unwrap();
        assert!(parse_config_str("{ this is not valid json").is_err(), "坏文件应解析失败");

        // backup_once 不应覆盖已存在的好备份
        backup_once(&p);
        let bak_text = std::fs::read_to_string(&bak).unwrap();
        assert_eq!(bak_text, std::fs::read_to_string(&real_bak).unwrap(), ".corrupt 不应被覆盖");

        let recovered = recover_from_backup(&p).expect("应从 .corrupt 自愈恢复");
        assert!(!recovered.nodes.is_empty(), "自愈后节点不应为空");
        assert!(!recovered.apps.is_empty(), "自愈后软件分流不应为空");
        assert!(!recovered.domain_rules.is_empty(), "自愈后域名规则不应为空");
        assert!(!recovered.servers.is_empty(), "自愈后 SSH 服务器不应为空");
        eprintln!(
            "自愈成功：nodes={} apps={} servers={} domainRules={}",
            recovered.nodes.len(),
            recovered.apps.len(),
            recovered.servers.len(),
            recovered.domain_rules.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn url_decode_never_panics_on_multibyte() {
        // 回归："%" 后随多字节 UTF-8 曾按字节切 str 直接 panic
        assert_eq!(url_decode("%中"), "%中");
        assert_eq!(url_decode("%E4%B8%AD"), "中");
        assert_eq!(url_decode("%4"), "%4");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%41%42"), "AB");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn validate_public_host_blocks_private() {
        // 回环
        assert!(validate_public_host("127.0.0.1").is_err());
        assert!(validate_public_host("127.0.0.1:8080").is_err());
        assert!(validate_public_host("[::1]:8080").is_err());
        assert!(validate_public_host("::1").is_err());
        // 内网
        assert!(validate_public_host("192.168.1.1").is_err());
        assert!(validate_public_host("10.0.0.1").is_err());
        assert!(validate_public_host("172.16.0.1").is_err());
        assert!(validate_public_host("100.64.0.1").is_err());
        // 链路本地 / 组播 / 未指定
        assert!(validate_public_host("169.254.1.1").is_err());
        assert!(validate_public_host("224.0.0.1").is_err());
        assert!(validate_public_host("0.0.0.0").is_err());
        assert!(validate_public_host("fe80::1").is_err());
        assert!(validate_public_host("fc00::1").is_err());
        assert!(validate_public_host("fd00::1").is_err());
        // IPv4-mapped 内网
        assert!(validate_public_host("::ffff:127.0.0.1").is_err());
    }

    #[test]
    fn validate_public_host_allows_public_and_domain() {
        // 公网 IP
        assert!(validate_public_host("8.8.8.8").is_ok());
        assert!(validate_public_host("1.1.1.1:443").is_ok());
        // 域名
        assert!(validate_public_host("example.com").is_ok());
        assert!(validate_public_host("sub.example.com:8080").is_ok());
        // 空 host 拒绝
        assert!(validate_public_host("").is_err());
        assert!(validate_public_host(":8080").is_err());
    }

    #[test]
    fn validate_public_host_blocks_domain_resolving_to_localhost() {
        // 域名解析到回环地址，应被 SSRF 防护拦截（堵住域名绕过）。
        // 注意：本用例依赖本机 DNS/Hosts 能否解析该名字——"localhost" 由
        // /etc/hosts 保证，其余别名（如 localhost.localdomain）在部分机器上
        // 解析失败，届时函数按"无法验证 → 放行"返回 Ok，属预期行为，
        // 不能拿来做断言（v0.4.2 修掉这条环境敏感断言，避免假红）。
        assert!(validate_public_host("localhost").is_err());
        assert!(validate_public_host("localhost:8080").is_err());
        assert!(validate_public_host("[::1]:443").is_err());
    }

    #[test]
    fn parse_subscription_supports_vmess_and_trojan() {
        // P2-11（v0.4.1）：vmess://（base64 JSON）与 trojan:// 解析。
        use std::process::Command as Cmd;
        // 构造 vmess share link：base64(JSON)
        let json = r#"{"v":"2","ps":"东京-测试","add":"198.51.100.9","port":"443","id":"uuid-1","aid":"0","net":"ws","type":"none","host":"cdn.example.com","path":"/ws-path","tls":"tls","sni":"cdn.example.com"}"#;
        let b64 = {
            // 用 base64 子进程编码（与解析路径对称）
            let mut c = Cmd::new("/usr/bin/base64")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            c.stdin.take().unwrap().write_all(json.as_bytes()).unwrap();
            String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap()
        };
        let text = format!("vmess://{}", b64.trim());
        let nodes = parse_subscription(&text).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].proto, "vmess");
        assert_eq!(nodes[0].server, "198.51.100.9");
        assert_eq!(nodes[0].port, 443);
        assert_eq!(nodes[0].ws_path, "/ws-path");
        assert!(nodes[0].tls);

        // trojan://password@host:port?sni=...#name
        let t = parse_subscription(
            "trojan://pass-123@198.51.100.10:443?security=tls&sni=t.example.com#东京",
        )
        .unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].proto, "trojan");
        assert_eq!(t[0].password, "pass-123");
        assert_eq!(t[0].server, "198.51.100.10");
        assert_eq!(t[0].sni, "t.example.com");
        assert!(t[0].tls);

        // 混合订阅：vless + vmess + trojan 一行一个
        let mixed = "vless://uuid-1@198.51.100.1:443?security=reality&pbk=AAA&sid=1#节点A
vmess://".to_string() + &b64.trim() + "
trojan://pass-x@198.51.100.2:8443#节点C";
        let ms = parse_subscription(&mixed).unwrap();
        assert_eq!(ms.len(), 3);
        assert!(ms.iter().any(|n| n.proto == "vless"));
        assert!(ms.iter().any(|n| n.proto == "vmess"));
        assert!(ms.iter().any(|n| n.proto == "trojan"));
    }

    #[test]
    fn parse_vless_uri_reality() {
        let uri = "vless://00000000-0000-4000-8000-000000000000@1.1.1.1:443?encryption=none&security=reality&sni=www.example.com&fp=chrome&pbk=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&sid=0000000000000000&flow=xtls-rprx-vision&type=tcp#%E7%A4%BA%E4%BE%8B%E8%8A%82%E7%82%B9";
        let node = parse_vless_uri(uri).expect("parse should succeed");
        assert_eq!(node.server, "1.1.1.1");
        assert_eq!(node.port, 443);
        assert_eq!(node.uuid, "00000000-0000-4000-8000-000000000000");
        assert_eq!(node.flow, "xtls-rprx-vision");
        assert_eq!(
            node.public_key,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
        assert_eq!(node.short_id, "0000000000000000");
        assert_eq!(node.sni, "www.example.com");
        assert_eq!(node.name, "\u{793a}\u{4f8b}\u{8282}\u{70b9}");
    }

    #[test]
    fn active_server_returns_key_auth_with_key_path() {
        // 密钥认证服务器：active_server 必须保留 key_path（否则 SSH 探针无法用密钥登录）
        let cfg = AppConfig {
            servers: vec![ServerInfo {
                id: "server-example".into(),
                name: "示例服务器".into(),
                host: "1.1.1.1".into(),
                port: 22022,
                user: "root".into(),
                auth: "key".into(),
                password_saved: false,
                private_key_saved: true,
                key_path: Some("~/.ssh/example_ed25519".into()),
            }],
            active_server_id: Some("server-example".into()),
            ..Default::default()
        };
        let s = cfg.active_server().expect("active_server 应返回服务器");
        assert_eq!(s.host, "1.1.1.1");
        assert_eq!(s.port, 22022);
        assert_eq!(s.auth, "key");
        assert_eq!(s.key_path.as_deref(), Some("~/.ssh/example_ed25519"));
    }

    #[test]
    fn active_server_derives_from_selected_node() {
        // 没有显式 SSH 配置（servers 空、sshHost 空）时，
        // active_server 应从选中的代理节点推导 SSH 主机地址。
        let cfg = AppConfig {
            nodes: vec![ProxyNode {
                name: "示例节点".into(),
                server: "1.1.1.1".into(),
                port: 443,
                uuid: "test-uuid".into(),
                flow: "xtls-rprx-vision".into(),
                network: "tcp".into(),
                tls: true,
                udp: true,
                fingerprint: "chrome".into(),
                public_key: "pk".into(),
                short_id: "sid".into(),
                sni: String::new(),
                source: "manual".into(),
                region: String::new(),
            proto: "vless".to_string(),
            password: String::new(),
            ws_path: String::new(),
            }],
            selected_node: Some("示例节点".into()),
            servers: vec![],
            ssh_host: None,
            ssh_port: Some(22022),
            ssh_user: Some("root".into()),
            ssh_auth: Some("key".into()),
            ssh_private_key: Some("~/.ssh/example_ed25519".into()),
            ..Default::default()
        };
        let s = cfg.active_server().expect("active_server 应从节点推导");
        // 主机应等于节点的 server，不是 None
        assert_eq!(s.host, "1.1.1.1");
        // SSH 端口来自 sshPort（与代理端口 443 不同）
        assert_eq!(s.port, 22022);
        assert_eq!(s.user, "root");
        assert_eq!(s.auth, "key");
    }

    #[test]
    fn active_server_prefers_explicit_over_node() {
        // 有显式 servers 配置时，优先用 servers，不从节点推导
        let cfg = AppConfig {
            nodes: vec![ProxyNode {
                name: "node-a".into(),
                server: "1.2.3.4".into(),
                port: 443,
                uuid: "u".into(),
                flow: "xtls-rprx-vision".into(),
                network: "tcp".into(),
                tls: true,
                udp: true,
                fingerprint: "chrome".into(),
                public_key: "pk".into(),
                short_id: "sid".into(),
                sni: String::new(),
                source: "manual".into(),
                region: String::new(),
            proto: "vless".to_string(),
            password: String::new(),
            ws_path: String::new(),
            }],
            selected_node: Some("node-a".into()),
            servers: vec![ServerInfo {
                id: "explicit-server".into(),
                name: "显式配置".into(),
                host: "5.6.7.8".into(),
                port: 22,
                user: "admin".into(),
                auth: "password".into(),
                password_saved: true,
                private_key_saved: false,
                key_path: None,
            }],
            active_server_id: Some("explicit-server".into()),
            ..Default::default()
        };
        let s = cfg.active_server().expect("active_server 应返回显式配置");
        // 应该用显式配置的 5.6.7.8，不是节点的 1.2.3.4
        assert_eq!(s.host, "5.6.7.8");
        assert_eq!(s.user, "admin");
    }
}
