#!/usr/bin/env python3
"""网络管理 MCP Server —— 让 AI 直接控制代理 App。

工具：
  status        查看代理状态
  start_proxy   启动代理
  stop_proxy    停止代理
  list_apps     列出扫描到的 App 与分流设置
  set_app_mode  设置某 App 的代理模式（proxy/direct）
  list_nodes    列出代理节点
  switch_node   切换当前节点
  check_network 测试国内外网站连通性
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request
import urllib.error

CONFIG_PATH = os.path.expanduser('~/Library/Application Support/magic-agent/config.json')
RUNTIME_DIR = os.path.expanduser('~/Library/Application Support/magic-agent/runtime')
# 内核优先用 runtime 常驻副本（App 升级替换 .app 不影响运行中的 mihomo），与 Rust bin_path 逻辑一致
_MIHOMO_RUNTIME_BIN = RUNTIME_DIR + '/bin/mihomo'

# ─────────────────────────────────────────────────────────────────────────────
# Server Instructions：给 LLM 的「整机用户手册」。
# MCP 协议 2025-06-18 引入的能力。客户端（WorkBuddy 等）会把这段文字注入系统提示词，
# 让智能体「一连上就天然知道」网络管理是干嘛的、什么时候该用、两条路怎么选，
# 而不需要用户额外解释。这是让智能体「根据自己抉择使用网络管理」的核心机制。
# 写作纪律（参考 MCP 官方博客 + agentpatterns 工具描述最佳实践）：
#   1) 讲「何时该用 / 何时不该用」，不只讲「能干嘛」；
#   2) 讲「两条路怎么选」（节点代理 7893 / 本机直连 7892），这是用户最在意的决策点；
#   3) 讲「与其他工具/软件的配合关系」；
#   4) 简洁、功能导向，不写营销话术。
# ─────────────────────────────────────────────────────────────────────────────
SERVER_INSTRUCTIONS = """你是「网络管理」这个本机代理软件的使用者。网络管理通过 MCP 工具暴露给你控制能力。请按下面的规则自己判断何时、如何使用它。

## 网络管理是什么
一个跑在本机（127.0.0.1）的代理网关，内核是 mihomo。它管理着一组代理节点（VLESS），并把本机网络流量分成两条「物理上分开的路」：
- 节点代理：http://127.0.0.1:7893 —— 流量从国外节点出去，用于访问被墙/国外网站（如 GitHub、Google、OpenAI、HuggingFace 等）。
- 本机直连：http://127.0.0.1:7892 —— 流量直接本机出去，用于访问国内网站（如百度、腾讯、阿里等）。

关键原则：网络管理**不替你自动判断**该走哪条路。决策权在你——你根据「要访问的目标是国内还是国外」自己拍板，然后把请求送进对应的端口。

## 什么时候用网络管理
- 你要下载/访问**国外**资源（GitHub 仓库、pip/npm 国外源、模型权重、国外 API、Google 等）→ 用 download_proxy 拿两条路地址，走节点代理端口 7893。
- 你要下载/访问**国内**资源 → 走本机直连端口 7892（或直接不用代理）。
- 你要排查「某个软件/域名到底走了代理还是直连」→ 用 list_connections 看实时连接、list_domain_rules 看分流规则。
- 你要切换节点、看节点延迟、测连通性 → 用 list_nodes / switch_node / test_node_delay / node_health / check_network。
- 你要实测「两条路到某个目标谁更快」→ 用 probe_route（返回两条路各自的延迟 + 下载速度对比 + 结论）。
- 你怀疑代理出问题了 → 先跑 doctor 一键自检。

## 什么时候不该用
- 访问本机回环服务（127.0.0.1、localhost）时，不要走代理端口。
- 访问 WorkBuddy 自己的中转站 / 模型接口时，不要套代理——这些是超长流式 JSON 请求，经过代理会损坏请求体。
- 纯本机文件操作、不涉及网络的活，跟网络管理无关。

## 两条路怎么选（核心决策）
1. 先调用 download_proxy（可选带 url 参数）拿到两条路地址和「建议」。
2. 拿不准、或这次下载很重要（大文件/模型/关键 API）→ 调 probe_route {"url":"目标地址"}，实测两条路到这个目标的真实延迟 + 下载吞吐，看数据拍板。别再凭「国内/国外」瞎猜：你的本机可能直连国外也通，也可能代理节点比直连还慢。
3. 建议/实测只是参考，最终由你判断：目标是国内 → 本机直连（7892）；目标是国外 → 节点代理（7893）。
4. 把下载/请求送进你选定的端口。送进去之后 mihomo 不会再二次判断，你选哪条就是哪条。

## 注意
- 代理默认可能未启动，先看 status 或用 start_proxy 启动。
- 改动会影响全局（切节点、改域名分流规则），操作前想清楚，改动后可以用 doctor 或 status 确认。"""


def _str_arg(args, key, default=''):
    """健壮地取字符串参数：兼容客户端传入非字符串（数字/None/dict 等）。
    直接 (args.get(k) or '').strip() 在遇到 int 时会 AttributeError。"""
    v = (args or {}).get(key, default)
    if v is None:
        return default
    return str(v).strip()


def _project_resource_bin():
    """推导项目内资源内核路径，避免硬编码开发机绝对路径。
    依次尝试：环境变量 MAGIC_AGENT_RESOURCES -> 项目根 src-tauri/resources -> 打包 .app 的 Resources。"""
    env = os.environ.get('MAGIC_AGENT_RESOURCES')
    if env:
        cand = os.path.join(env, 'bin', 'mihomo')
        if os.path.exists(cand):
            return cand
    # 项目根 = 本脚本 mcp/ 目录的上一级
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    cand = os.path.join(root, 'src-tauri', 'resources', 'bin', 'mihomo')
    if os.path.exists(cand):
        return cand
    # 打包 .app：Contents/Resources/bin/mihomo
    cand = os.path.join(root, '..', '..', 'Resources', 'bin', 'mihomo')
    if os.path.exists(cand):
        return cand
    return cand


MIHOMO_BIN = _MIHOMO_RUNTIME_BIN if os.path.exists(_MIHOMO_RUNTIME_BIN) \
    else _project_resource_bin()
# pgrep 精确匹配「本 App 特有的 runtime 常驻副本」内核路径。
# 绝不能用宽泛的 resources/bin/mihomo——FlClash/Clash Verge 等第三方内核
# 常放在它们自己的 .app/Contents/Resources/ 下，用 resources 关键词会误杀它们。
MIHOMO_PGREP_PATTERN = 'magic-agent/runtime/bin/mihomo'
API = 'http://127.0.0.1:19091'
# 本地 HTTP 桥接鉴权令牌。首次启动自动生成并持久化，避免任意网页通过
# localhost CSRF 直接调用代理控制工具。
HTTP_BRIDGE_TOKEN_PATH = os.path.expanduser('~/Library/Application Support/magic-agent/http-bridge.token')
HTTP_BRIDGE_TOKEN = ''


def load_http_bridge_token():
    """读取或生成 HTTP 桥接令牌（0600），返回当前令牌。"""
    global HTTP_BRIDGE_TOKEN
    try:
        with open(HTTP_BRIDGE_TOKEN_PATH) as f:
            token = f.read().strip()
    except OSError:
        token = ''
    if not token:
        import secrets
        token = secrets.token_hex(32)
        os.makedirs(os.path.dirname(HTTP_BRIDGE_TOKEN_PATH), exist_ok=True)
        with open(HTTP_BRIDGE_TOKEN_PATH, 'w') as f:
            f.write(token)
        os.chmod(HTTP_BRIDGE_TOKEN_PATH, 0o600)
    HTTP_BRIDGE_TOKEN = token
    return token

# 「两条路」端口（与 src-tauri/src/mihomo.rs 保持一致）：
#   7893 端口 → 无条件 PROXY；7892 端口 → 无条件 DIRECT。
# 决策权在智能体，mihomo 不做国内外自动分流。
PROXY_PORT = 7893
DIRECT_PORT = 7892

# 常见国内域名后缀（用于 download_proxy 的「建议」，仅建议不替智能体决定）。
# 用后缀匹配：host == d 或 host.endswith('.' + d) 都算国内。
CN_DOMAIN_SUFFIXES = (
    'baidu.com', 'qq.com', 'taobao.com', 'jd.com', '163.com', '126.com', 'weibo.com',
    'zhihu.com', 'bilibili.com', 'douyin.com', 'toutiao.com', 'aliyun.com', 'alibaba.com',
    'bytedance.com', 'meituan.com', 'dianping.com', 'ctrip.com', 'sina.com', 'sina.com.cn',
    'sohu.com', 'sogou.com', '360.cn', 'mi.com', 'huawei.com', 'oppo.com', 'vivo.com',
    'xiaomi.com', 'pinduoduo.com', 'cainiao.com', 'amap.com', 'autonavi.com', 'qcloud.com',
    'tencent.com', 'weixin.com', 'wechat.com', 'netease.com', 'douban.com', 'iqiyi.com',
    'youku.com', 'tudou.com', 'kuaishou.com', 'csdn.net', 'jianshu.com', 'juejin.cn',
    'gitee.com', 'oschina.net', 'cnblogs.com', 'ithome.com', '36kr.com', 'smzdm.com',
    'mgtv.com', 'suning.com', 'gome.com.cn', 'yhd.com', 'dangdang.com', 'vancl.com',
    'mogujie.com', 'meilishuo.com', 'vip.com', 'youzan.com', 'shopex.cn', 'baifendian.com',
    'tianya.cn', 'ifeng.com', 'people.com.cn', 'xinhuanet.com', 'cctv.com', 'cntv.cn',
    'chinanews.com', 'chinadaily.com.cn', 'gmw.cn', 'youth.cn', 'ce.cn', 'gov.cn',
    'edu.cn', 'mil.cn', 'org.cn', 'ac.cn', 'net.cn', 'com.cn',
)
# 关键防御：宿主环境（WorkBuddy/CI）会注入 HTTP_PROXY 等环境变量，
# 裸 urlopen 会把 127.0.0.1 的 mihomo API 请求转发给外部代理 → 502/误诊。
# 进程内清除代理变量 + 全部请求走无代理 opener，保证探测的是真实链路。
for _k in ('HTTP_PROXY', 'HTTPS_PROXY', 'http_proxy', 'https_proxy', 'ALL_PROXY', 'all_proxy'):
    os.environ.pop(_k, None)
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
# 旧特权控制器（v0.4.1 起废弃）：以下常量仅用于 remove_privileged_helper 判断卸载目标是否存在
CTL_PATH = '/usr/local/lib/magic-agent/mihomo-ctl.sh'

# 保命直连名单：这些目标永远走系统原生路由，绝不进 TUN/代理端口（与 Rust 侧一致）。
# 尤其 AI 助手访问的中转站——模型请求是超长流式 JSON，被应用层代理"拆-组"会损坏请求体。
# 保命直连哨兵（P2-9，v0.4.1）：203.0.113.74 属 RFC 5737 TEST-NET-3 文档保留段，
# 真实互联网不可路由。作用 = 保证第 0 层直连名单非空、规则可解析（与 Rust 侧
# PROTECTED_DIRECT_DOMAINS 一一对应，双引擎一致）。接入真实 AI 中转站时把真实
# 出口 IP 替换/追加在此；切勿清空最后一项。
PROTECTED_DIRECT_DOMAINS = ['203.0.113.74']


def remove_privileged_helper():
    """v0.4.1 安全整改：卸载旧特权控制器（弹一次管理员授权）。
    删除 /etc/sudoers.d/magic-agent-mihomo 白名单与 /usr/local/lib/magic-agent/mihomo-ctl.sh。
    背景：sudoers 免密 root 通道 = 系统后门级风险（脚本可被替换即免密 root 任意命令）；
    内核已用户态化（TUN 关闭、全普通端口），root 通道不再需要。卸载即封死该攻击面。"""
    script = (
        'set -e; '
        'rm -f /etc/sudoers.d/magic-agent-mihomo '
        '/etc/sudoers.d/magic-agent-mihomo.tmp; '
        'rm -f /usr/local/lib/magic-agent/mihomo-ctl.sh; '
        'rmdir /usr/local/lib/magic-agent 2>/dev/null || true'
    )
    apple = 'do shell script "%s" with administrator privileges' % (
        script.replace('\\', '\\\\').replace('"', '\\"'))
    p = subprocess.run(['/usr/bin/osascript', '-e', apple], capture_output=True, text=True, timeout=120)
    if p.returncode != 0:
        err = (p.stderr or '').strip()
        if 'cancel' in err.lower() or 'user canceled' in err.lower():
            return {'error': '用户取消了管理员授权，特权控制器未被卸载'}
        return {'error': f'卸载失败: {err[:150]}'}
    gone = not os.path.exists(CTL_PATH) and not os.path.exists('/etc/sudoers.d/magic-agent-mihomo')
    return {'ok': True, 'removed': gone,
            'message': '特权控制器已卸载（sudoers 白名单与 root 控制脚本已删除），代理内核已完全用户态化'}


def rotate_log(path, max_bytes=10 * 1024 * 1024):
    """日志超 10MB 轮转为 .old（启动前调用，运行中 mihomo 持有 fd 不受影响）"""
    try:
        if os.path.exists(path) and os.path.getsize(path) > max_bytes:
            old = path + '.old'
            if os.path.exists(old):
                os.remove(old)
            os.replace(path, old)
    except OSError:
        pass


def api_get(path, timeout=5):
    try:
        req = urllib.request.Request(API + path)
        _add_auth(req)
        r = _OPENER.open(req, timeout=timeout)
        return json.loads(r.read().decode())
    except Exception as e:
        return {'error': str(e)}


def api_secret():
    """mihomo 控制 API 的鉴权 secret（与 Rust 侧共享，存 config.json apiSecret 字段）。
    首次调用时生成并持久化，防止本机任意进程/网页 CSRF 操控代理。"""
    import secrets as _secrets
    cfg = read_config()
    if 'error' in cfg:
        return ''
    s = cfg.get('apiSecret')
    if not s:
        s = _secrets.token_hex(16)
        cfg['apiSecret'] = s
        write_config(cfg)
    return s


def _add_auth(req):
    s = api_secret()
    if s:
        req.add_header('Authorization', 'Bearer ' + s)


def system_proxy_enabled():
    """系统代理【真实】是否开启（与 Rust 侧 system_proxy.rs::status 一致，读 scutil --proxy）。
    不要用 config.json 的 systemProxy 字段当真实状态——那只是"上次的意图"，
    App 重启/外部改动后可能和现实不符。"""
    try:
        out = subprocess.run(['/usr/sbin/scutil', '--proxy'],
                             capture_output=True, text=True, timeout=5).stdout
        return 'HTTPEnable : 1' in out or 'SOCKSEnable : 1' in out
    except Exception:
        return False


def _parse_proxy_detail(out):
    """解析 -get*proxy 真实输出（2026-09-23 实机实测，Darwin 25.6）：
        Enabled: Yes
        Server: 127.0.0.1
        Port: 7891
        Authenticated Proxy Enabled: 0
    返回 (enabled, port)。末行 "Authenticated Proxy Enabled" 不得被当成 Enabled 行。"""
    enabled, port = False, 0
    for line in out.splitlines():
        s = line.strip()
        if s.startswith('Enabled:'):
            enabled = s.split(':', 1)[1].strip().lower() == 'yes'
        elif s.startswith('Port:'):
            try:
                port = int(s.split(':', 1)[1].strip())
            except ValueError:
                port = 0
    return enabled, port


def verify_system_proxy(services, want_port, expect_on):
    """逐服务读回系统代理真实状态，语义与 Rust 侧 system_proxy.rs::verify_system_proxy 一致：
    -get*state 系列读命令在真机【不存在】（读写命令集不对称），唯一可靠读法是
    -getwebproxy/-getsecurewebproxy/-getsocksfirewallproxy。
    P1-7 口径（v0.4.1）：开关态以 scutil 全局视图为唯一权威（真机实测出现
    全局 HTTPEnable:0 而服务级 Enabled:Yes 的矛盾态，服务级 Enabled 不可信）。
    返回的 httpOn/httpsOn/socksOn = 该通道是否【达标】：
    期望开 → 全局开且服务级端口精确等于 want_port；期望关 → 全局关且服务级
    端口不等于 want_port（残留指向本程序端口仍点名）。
    单项读取失败记入 errors，不抛异常。"""
    global_enabled = system_proxy_enabled()
    states = []
    for svc in services:
        errors = []
        def _get(flag):
            try:
                p = subprocess.run(['/usr/sbin/networksetup', flag, svc],
                                   capture_output=True, text=True, timeout=10)
                if p.returncode != 0:
                    errors.append(f'{flag}: {p.stderr.strip()}')
                    return False
                _enabled, port = _parse_proxy_detail(p.stdout)
                if port != want_port and port != 0:
                    errors.append(f'{flag}: 指向 127.0.0.1:{port} 而非期望端口 {want_port}')
                return (global_enabled and port == want_port) if expect_on else (not global_enabled and port != want_port)
            except Exception as e:
                errors.append(f'{flag}: {e}')
                return False
        states.append({
            'service': svc,
            'httpOn': _get('-getwebproxy'),
            'httpsOn': _get('-getsecurewebproxy'),
            'socksOn': _get('-getsocksfirewallproxy'),
            'errors': errors,
        })
    return states


def list_network_services():
    """网络服务列表（跳过蓝牙/USB/Thunderbolt 等虚拟口，避免误设）。与 Rust 侧 list_services 一致。"""
    out = subprocess.run(['/usr/sbin/networksetup', '-listallnetworkservices'],
                         capture_output=True, text=True, timeout=10).stdout
    services = []
    for line in out.splitlines():
        s = line.strip()
        if not s or s.startswith('An asterisk'):
            continue
        low = s.lower()
        if any(k in low for k in ('asterisk', 'bluetooth', 'iphone', 'thunderbolt', 'bridge')):
            continue
        services.append(s)
    return services


def set_system_proxy(enable, port=7891):
    """开/关 macOS 系统代理并【逐服务回读对账】（与 Rust 侧 set_system_proxy 一致）。
    旧实现的 any_ok（任一服务任一命令成功即报成功）是"假账"根源，已废弃：
    现在设置后逐个 -get* 读回，返回如实的
    {enabled, allOk, mismatched:[服务名], services:[{service,httpOn,httpsOn,socksOn,errors}]}。
    部分失败不抛异常（系统代理可能改了半套），由调用方看 allOk/mismatched；
    仅当没有任何服务可操作时才抛。
    enable=True 把 HTTP/HTTPS/SOCKS 指向 127.0.0.1:port 并开启；
    enable=False 只关开关（不动地址，便于快速恢复）。"""
    services = list_network_services()
    if not services:
        raise RuntimeError('未检测到网络服务，无法设置系统代理')
    port_str = str(port)
    write_errors = {}
    for svc in services:
        if enable:
            cmds = [['-setwebproxy', svc, '127.0.0.1', port_str],
                    ['-setsecurewebproxy', svc, '127.0.0.1', port_str],
                    ['-setsocksfirewallproxy', svc, '127.0.0.1', port_str],
                    ['-setwebproxystate', svc, 'on'],
                    ['-setsecurewebproxystate', svc, 'on'],
                    ['-setsocksfirewallproxystate', svc, 'on']]
        else:
            cmds = [['-setwebproxystate', svc, 'off'],
                    ['-setsecurewebproxystate', svc, 'off'],
                    ['-setsocksfirewallproxystate', svc, 'off']]
        for c in cmds:
            p = subprocess.run(['/usr/sbin/networksetup'] + c, capture_output=True, text=True, timeout=15)
            if p.returncode != 0:
                write_errors.setdefault(svc, []).append(f'{c[0]}: {p.stderr.strip()}')
    states = verify_system_proxy(services, port, enable)
    # *_On 语义 = 该通道是否达标（判定已封装在 verify 内），直接三与
    mismatched = []
    for st in states:
        if not (st['httpOn'] and st['httpsOn'] and st['socksOn']) or st['errors']:
            mismatched.append(st['service'])
    for svc in write_errors:
        if svc not in mismatched:
            mismatched.append(svc)
    all_ok = bool(states) and not write_errors and not mismatched
    return {'enabled': system_proxy_enabled(), 'allOk': all_ok,
            'mismatched': mismatched, 'services': states}


def _snake_to_camel(s):
    """snake_case → camelCase，与 Rust config.rs 的 normalize_keys_to_camel 对齐。"""
    if '_' not in s:
        return s
    parts = s.split('_')
    return parts[0] + ''.join(p[:1].upper() + p[1:] for p in parts[1:])


def _normalize_keys(obj):
    """递归把 dict 的所有 snake_case 键规整为 camelCase。
    与 Rust 侧 load() 对齐：旧版 config 可能遗留 snake_case 键，
    不规整则 MCP 侧 cfg.get('apiSecret') 找不到、误生成新 secret，
    导致 config 同时出现 apiSecret/api_secret 两个键。"""
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            nk = _snake_to_camel(k)
            # 若 camelCase 键已存在（混合配置），后出现的覆盖先出现的
            out[nk] = _normalize_keys(v)
        return out
    if isinstance(obj, list):
        return [_normalize_keys(i) for i in obj]
    return obj


def read_config():
    try:
        with open(CONFIG_PATH) as f:
            return _normalize_keys(json.load(f))
    except Exception as e:
        return {'error': str(e)}


def write_config(cfg):
    # 原子写：临时文件+rename，防止与 Rust(App) 并发写时出现半截 JSON
    tmp = CONFIG_PATH + '.tmp'
    with open(tmp, 'w') as f:
        json.dump(cfg, f, ensure_ascii=False, indent=2)
    os.replace(tmp, CONFIG_PATH)
    try:
        os.chmod(CONFIG_PATH, 0o600)
    except OSError:
        pass


# ─────────────────────────────────────────────────────────────────────────────
# P1-2 接管账本（ledger.json）——与 Rust src-tauri/src/ledger.rs 共写同一文件，
# schema 逐键一致（camelCase：version/sessions/id/status/reason/startedTs/
# settledTs/settledReason/entries/kind/key/before/after/reversible/ts）。
# 立场：接管必须先记账后动手；启动清理 ≠ 接管（只有显式启停/接管入口留账）。
# JSON 键改动必须两侧同步（schema 回归锁在 tests/test_regressions.py）。
# ─────────────────────────────────────────────────────────────────────────────

LEDGER_PATH = os.path.expanduser('~/Library/Application Support/magic-agent/ledger.json')


def _now_ts():
    return int(time.time())


def _ledger_load():
    """读账本。缺失→空；损坏→原文件保全为 .corrupt 证据（已有 corrupt 不覆盖，
    加时间戳另存）后从空账本继续。返回 (ledger, note)，note 非空必须打日志——静默=丢证据。
    与 Rust ledger.rs::load 同语义。"""
    if not os.path.exists(LEDGER_PATH):
        return {'version': 1, 'sessions': []}, None
    try:
        with open(LEDGER_PATH) as f:
            data = json.load(f)
        if (not isinstance(data, dict) or not isinstance(data.get('version'), int)
                or data.get('version', 0) < 1 or not isinstance(data.get('sessions'), list)):
            raise ValueError('schema 异常')
        return data, None
    except Exception as e:
        base = LEDGER_PATH + '.corrupt'
        bak = base if not os.path.exists(base) else f'{base}.{_now_ts()}'
        try:
            os.rename(LEDGER_PATH, bak)
            note = f'账本损坏（{e}），原文件已保全为 {bak}，从空账本继续'
        except OSError as re_e:
            note = f'账本损坏（{e}），且备份失败（{re_e}），从空账本继续'
        return {'version': 1, 'sessions': []}, note


def _ledger_save(ledger):
    # 原子写 + 0600（chmod 在 rename 前，与 Rust 同序，防泄露窗口）
    os.makedirs(os.path.dirname(LEDGER_PATH), exist_ok=True)
    tmp = LEDGER_PATH + '.tmp'
    with open(tmp, 'w') as f:
        json.dump(ledger, f, ensure_ascii=False, indent=2)
    try:
        os.chmod(tmp, 0o600)
    except OSError:
        pass
    os.replace(tmp, LEDGER_PATH)


def _snapshot_channel(svc, flag):
    """单通道完整原值（enabled/server/port）——还原的依据，不是"达标"判定。
    读法用 -get*proxy 三件套（真机唯一可靠读法）。失败返回兜底值 + error。"""
    try:
        p = subprocess.run(['/usr/sbin/networksetup', flag, svc],
                           capture_output=True, text=True, timeout=10)
        if p.returncode != 0:
            return {'enabled': False, 'server': '', 'port': 0}, f'{flag}: {p.stderr.strip()}'
        enabled, port = _parse_proxy_detail(p.stdout)
        server = ''
        for line in p.stdout.splitlines():
            s = line.strip()
            if s.startswith('Server:'):
                server = s.split(':', 1)[1].strip()
        return {'enabled': enabled, 'server': server, 'port': port}, None
    except Exception as e:
        return {'enabled': False, 'server': '', 'port': 0}, f'{flag}: {e}'


def snapshot_system_proxy():
    """逐服务完整原值快照（账本 before）。与 Rust system_proxy::snapshot_system_proxy 同语义。"""
    snaps = []
    for svc in list_network_services():
        errors = []
        channels = {}
        for key, flag in (('http', '-getwebproxy'), ('https', '-getsecurewebproxy'), ('socks', '-getsocksfirewallproxy')):
            ch, err = _snapshot_channel(svc, flag)
            channels[key] = ch
            if err:
                errors.append(err)
        snaps.append({'service': svc, **channels, 'errors': errors})
    return snaps


def ledger_begin(reason):
    """接管开始记账：必须在任何状态变更前调用。已有 open 会话 → 不动账本
    （before 永远是【最初】原值），返回 False。与 Rust begin_takeover 同语义。"""
    ledger, note = _ledger_load()
    if note:
        print(f'[ledger] {note}', file=sys.stderr)
    if any(s.get('status') == 'open' for s in ledger['sessions']):
        return False
    ts = _now_ts()
    entries = [{
        'kind': 'system_proxy', 'key': s['service'], 'before': s,
        'after': None, 'reversible': True, 'ts': ts,
    } for s in snapshot_system_proxy()]
    session = {
        'id': f'ls-{ts}-{os.getpid()}', 'status': 'open', 'reason': reason,
        'startedTs': ts, 'entries': entries,
    }
    ledger['sessions'].append(session)
    _ledger_save(ledger)
    return True


def ledger_record_killed(procs):
    """被清理的第三方进程入账（不可逆项 reversible=false）。无 open 会话静默跳过。"""
    if not procs:
        return
    ledger, note = _ledger_load()
    if note:
        print(f'[ledger] {note}', file=sys.stderr)
    ts = _now_ts()
    for s in ledger['sessions']:
        if s.get('status') == 'open':
            s['entries'].extend({
                'kind': 'process', 'key': p, 'before': 'running',
                'after': 'killed', 'reversible': False, 'ts': ts,
            } for p in procs)
            break
    _ledger_save(ledger)


def ledger_settle(reason):
    """结账（接管正常结束）。无 open 会话返回 False，幂等。与 Rust settle_open 同语义。"""
    ledger, note = _ledger_load()
    if note:
        print(f'[ledger] {note}', file=sys.stderr)
    done = False
    for s in ledger['sessions']:
        if s.get('status') == 'open':
            s['status'] = 'settled'
            s['settledTs'] = _now_ts()
            s['settledReason'] = reason
            done = True
            break
    if done:
        _ledger_save(ledger)
    return done


def _restore_channel(svc, flag, state_flag, raw):
    """按快照原值恢复单通道（与 Rust restore_channel 同语义）。返回错误串或 None。"""
    try:
        if raw.get('enabled'):
            server = raw.get('server') or '127.0.0.1'
            port = str(raw.get('port') or 0)
            p = subprocess.run(['/usr/sbin/networksetup', flag, svc, server, port],
                               capture_output=True, text=True, timeout=15)
            if p.returncode != 0:
                return f'{flag} {svc}: {p.stderr.strip()}'
            p2 = subprocess.run(['/usr/sbin/networksetup', state_flag, svc, 'on'],
                                capture_output=True, text=True, timeout=15)
            if p2.returncode != 0:
                return f'{state_flag} {svc}: {p2.stderr.strip()}'
        else:
            p = subprocess.run(['/usr/sbin/networksetup', state_flag, svc, 'off'],
                               capture_output=True, text=True, timeout=15)
            if p.returncode != 0:
                return f'{state_flag} {svc}: {p.stderr.strip()}'
        return None
    except Exception as e:
        return f'{flag} {svc}: {e}'


def restore_service_snapshot(snap):
    """回放一个服务（HTTP→HTTPS→SOCKS，与 Rust 同序）。快照不完整跳过并如实报告。"""
    if snap.get('errors'):
        return [f"{snap.get('service')}: 快照不完整（{'；'.join(snap['errors'])}），跳过还原"]
    errs = []
    for key, flag, state_flag in (
            ('http', '-setwebproxy', '-setwebproxystate'),
            ('https', '-setsecurewebproxy', '-setsecurewebproxystate'),
            ('socks', '-setsocksfirewallproxy', '-setsocksfirewallproxystate')):
        e = _restore_channel(snap['service'], flag, state_flag, snap.get(key) or {})
        if e:
            errs.append(e)
    return errs


def ledger_rollback(reason):
    """P1-3 回滚原语：当前 open 会话的 system_proxy 条目按 before 逐服务回放，
    然后结账。返回 (回放错误清单, 是否有账可回, 账本是否含不可逆 process 项)。
    与 Rust rollback_session 同语义：process 项回不来，如实声明不假装。"""
    session = ledger_open_session()
    if session is None:
        return [], False, False
    errs = []
    had_procs = False
    for entry in session.get('entries', []):
        if entry.get('kind') != 'system_proxy':
            had_procs = True
            continue
        before = entry.get('before')
        if not isinstance(before, dict) or 'service' not in before:
            errs.append(f"{entry.get('key')}: 快照解析失败，跳过还原")
            continue
        errs.extend(restore_service_snapshot(before))
    try:
        ledger_settle(reason)
    except Exception as e:
        errs.append(f'结账失败：{e}')
    return errs, True, had_procs


def ledger_open_session():
    """当前未结账本（dict 或 None）。损坏保全 note 同样如实打日志。"""
    ledger, note = _ledger_load()
    if note:
        print(f'[ledger] {note}', file=sys.stderr)
    for s in ledger['sessions']:
        if s.get('status') == 'open':
            return s
    return None


def judge_drift(mismatched):
    """P1-4 漂移判定（与 Rust judge_drift 同口径的纯函数部分）：
    接管生效中逐服务读回的不达标清单非空 = 外部改动 detected。
    返回漂移服务清单；无漂移返回 None。"""
    return list(mismatched) if mismatched else None


def probe_drift(port=7891):
    """P1-4 漂移抽验：接管生效中（有未结账本）时，逐服务读回系统代理现值，
    期望它们仍指向 127.0.0.1:接管端口。返回漂移服务清单或 None。
    仅只读，不写任何状态。MCP 无常驻线程，由 status 调用惰性触发——
    且只在确有未结账本且内核在跑时执行，避免无接管场景放大 networksetup 开销。"""
    if ledger_open_session() is None:
        return None
    if not mihomo_running():
        return None
    services = list_network_services()
    if not services:
        return None
    states = verify_system_proxy(services, port, True)
    mismatched = [st['service'] for st in states
                  if not (st['httpOn'] and st['httpsOn'] and st['socksOn']) or st['errors']]
    return judge_drift(mismatched)


def mihomo_running():
    """内核是否【真正在服务】（与 Rust 侧 mihomo.rs::status 逻辑保持一致）。

    旧实现只 pgrep 进程存在 → 会把"孤儿内核"（进程活着但端口/API 都没监听）
    误判为"代理已在运行"，导致 start_proxy 直接返回"已在运行"而拒绝真正启动，
    代理实际不工作。修复：要求 (进程存在 或 端口开放) 且 控制 API 可响应。
    """
    has_proc = bool(subprocess.run(
        ['/usr/bin/pgrep', '-f', MIHOMO_PGREP_PATTERN],
        capture_output=True, text=True).stdout.strip())
    if not has_proc:
        return False
    # 进程在，但必须确认控制 API 真的能响应（端口已就绪），才算"在运行"
    api = api_get('/version', timeout=2)
    return 'error' not in api


def stop_mihomo():
    # v0.4.1：用户态直接结束内核进程（内核是普通用户进程，无需特权控制器/osascript）
    p = subprocess.run(['/usr/bin/pgrep', '-f', MIHOMO_PGREP_PATTERN],
                       capture_output=True, text=True)
    for pid in p.stdout.strip().split():
        try:
            subprocess.run(['/bin/kill', pid], capture_output=True, text=True, timeout=10)
        except Exception:
            pass


def ensure_runtime_bin():
    """把内核复制到 runtime/bin 常驻（幂等）。App 升级替换 .app 不影响运行中的 mihomo。"""
    src = _project_resource_bin()
    if not os.path.exists(src):
        return
    dst_dir = RUNTIME_DIR + '/bin'
    os.makedirs(dst_dir, exist_ok=True)
    dst = dst_dir + '/mihomo'
    need = True
    if os.path.exists(dst):
        try:
            with open(src, 'rb') as f1, open(dst, 'rb') as f2:
                need = f1.read() != f2.read()
        except OSError:
            need = True
    if need:
        shutil.copyfile(src, dst)
        os.chmod(dst, 0o755)


def start_mihomo():
    # v0.4.1：用户态直接启动（内核零特权需求——TUN 已关闭、全部端口为普通端口，
    # 无需 sudoers 特权控制器、无需 osascript 弹窗提权）
    ensure_runtime_bin()
    conf = RUNTIME_DIR + '/mihomo.yaml'
    log = RUNTIME_DIR + '/mihomo.log'
    err = RUNTIME_DIR + '/mihomo.err.log'
    rotate_log(log)
    rotate_log(err)
    lf = open(log, 'ab')
    ef = open(err, 'ab')
    p = subprocess.Popen([MIHOMO_BIN, '-f', conf, '-d', RUNTIME_DIR],
                         stdout=lf, stderr=ef, start_new_session=True)
    return str(p.pid)


def _yaml_quote(s):
    """把字符串安全嵌入 YAML 双引号，防止恶意订阅字段（含 " 或换行）注入配置行。"""
    return (s.replace('\\', '\\\\').replace('"', '\\"')
             .replace('\n', '\\n').replace('\r', '\\r').replace('\t', '\\t'))


def _sanitize_rule_field(s):
    """净化规则字段（域名/IP），只保留合法字符；含非法字符返回与原值不同的结果，
    调用方据此丢弃恶意输入。域名/IP 本应只含 ASCII，非 ASCII 视为异常剔除是安全的。"""
    return ''.join(c for c in s if c.isalnum() or c in '.-_:*/#[]')


def _sanitize_node_name(s):
    """净化节点名（用于生成 NODE-<name> 组名及其规则引用）。

    与 Rust 侧 sanitize_node_name 语义一致：节点名用户可自定义（可含中文/空格），
    会同时出现在 proxy-group 的 name 与规则的 target 两处，必须用同一净化结果，
    否则组名与引用对不上、规则静默失效。

    只剔除会破坏规则结构的字符——逗号（字段分隔符）、换行/回车（行分隔符）及
    其他控制字符；其余可见字符（含 CJK、空格、字母数字、标点）一律保留。
    """
    return ''.join(c for c in s if c not in ',\r\n' and not (ord(c) < 0x20))


def generate_config(cfg):
    """根据 AppConfig 生成 mihomo.yaml"""
    nodes = cfg.get('nodes', [])
    selected = cfg.get('selectedNode') or (nodes[0]['name'] if nodes else None)
    lines = []
    lines.append('mixed-port: 7891')
    lines.append('mode: rule')
    # TUN 只接管「明确要代理」的流量，绝不碰直连流量（默认直连架构）：
    #   auto-route: false 不再改系统默认路由，所有未指名的流量走系统原生直连，
    #   不被网络管理"进-出"污染（尤其 AI 助手的超长流式 JSON 请求）。
    # v0.4.1：TUN 彻底关闭（enable:true 需要 root，且 dns-hijack any:53 绑定特权端口；
    # 本项目红线冻结 auto-route=false，TUN 不承载日常流量，系统代理是唯一流量入口）。
    # 关闭后内核零特权需求，普通用户态即可运行（与 Rust 侧 build_conf 保持一致）。
    lines.append('tun:')
    lines.append('  enable: false')
    # 两条物理上分开的「路」：智能体自己选节点代理(7893)还是本机直连(7892)，
    # 进去后 mihomo 不再二次判断。这与 Rust 侧 build_conf 保持一致。
    # 安全红线：listeners 必须逐条显式 listen: 127.0.0.1——allow-lan: false 管不到
    # listeners，不写 listen 时 mihomo 默认绑 0.0.0.0，局域网可匿名白嫖本机节点出口。
    lines.append('listeners:')
    lines.append('  - name: proxy-only')
    lines.append('    type: mixed')
    lines.append(f'    port: {PROXY_PORT}')
    lines.append('    listen: 127.0.0.1')
    lines.append('    proxy: PROXY')
    lines.append('  - name: direct-only')
    lines.append('    type: mixed')
    lines.append(f'    port: {DIRECT_PORT}')
    lines.append('    listen: 127.0.0.1')
    lines.append('    proxy: DIRECT')
    lines.append('log-level: info')
    lines.append('allow-lan: false')
    lines.append('ipv6: false')
    lines.append('find-process-mode: always')
    lines.append('external-controller: 127.0.0.1:19091')
    # 控制 API 鉴权（与 Rust 侧共享 config.json 的 apiSecret 字段）
    if cfg.get('apiSecret'):
        lines.append(f'secret: {cfg["apiSecret"]}')
    lines.append('geo-auto-update: false')
    lines.append('geodata-mode: false')
    lines.append('geodata-loader: memconservative')
    lines.append('')
    lines.append('dns:')
    lines.append('  enable: true')
    lines.append('  listen: 127.0.0.1:1054')
    lines.append('  enhanced-mode: redir-host')
    lines.append('  nameserver:')
    lines.append('    - https://dns.alidns.com/dns-query')
    lines.append('    - https://doh.pub/dns-query')
    lines.append('  fallback:')
    lines.append('    - tls://8.8.8.8')
    lines.append('    - tls://1.1.1.1')
    lines.append('  fallback-filter:')
    lines.append('    geoip: true')
    lines.append('    geoip-code: CN')
    lines.append('')
    lines.append('proxies:')
    for n in nodes:
        proto = str(n.get('proto', 'vless'))
        lines.append(f'  - name: "{_yaml_quote(str(n["name"]))}"')
        if proto == 'vmess':
            # P2-11（v0.4.1）：vmess 分支（uuid/alterId/cipher + ws-opts）
            lines.append(f'    type: vmess')
            lines.append(f'    server: "{_yaml_quote(str(n["server"]))}"')
            lines.append(f'    port: {n["port"]}')
            lines.append(f'    uuid: "{_yaml_quote(str(n.get("uuid", "")))}"')
            lines.append('    alterId: 0')
            lines.append('    cipher: auto')
            lines.append(f'    network: {_yaml_quote(str(n.get("network", "tcp")))}')
            lines.append(f'    tls: {n.get("tls", True)}')
            lines.append(f'    udp: {n.get("udp", True)}')
            lines.append(f'    client-fingerprint: "{_yaml_quote(str(n.get("fingerprint", "chrome")))}"')
            if n.get('sni'):
                lines.append(f'    servername: "{_yaml_quote(str(n["sni"]))}"')
            if str(n.get('network', 'tcp')) == 'ws' and n.get('wsPath'):
                lines.append('    ws-opts:')
                lines.append(f'      path: "{_yaml_quote(str(n["wsPath"]))}"')
                lines.append('      headers:')
                lines.append(f'        Host: "{_yaml_quote(str(n["server"]))}"')
        elif proto == 'trojan':
            # trojan 分支（password + tls/sni）
            lines.append(f'    type: trojan')
            lines.append(f'    server: "{_yaml_quote(str(n["server"]))}"')
            lines.append(f'    port: {n["port"]}')
            lines.append(f'    password: "{_yaml_quote(str(n.get("password", "")))}"')
            lines.append(f'    udp: {n.get("udp", True)}')
            lines.append(f'    network: {_yaml_quote(str(n.get("network", "tcp")))}')
            if n.get('tls') or n.get('sni'):
                lines.append(f'    tls: {n.get("tls", True)}')
                lines.append(f'    sni: "{_yaml_quote(str(n.get("sni", "")))}"')
        else:
            # vless（默认）
            lines.append(f'    type: vless')
            lines.append(f'    server: "{_yaml_quote(str(n["server"]))}"')
            lines.append(f'    port: {n["port"]}')
            lines.append(f'    uuid: "{_yaml_quote(str(n["uuid"]))}"')
            lines.append(f'    network: {_yaml_quote(str(n.get("network", "tcp")))}')
            lines.append(f'    tls: {n.get("tls", True)}')
            lines.append(f'    udp: {n.get("udp", True)}')
            lines.append(f'    flow: "{_yaml_quote(str(n.get("flow", "")))}"')
            lines.append(f'    client-fingerprint: "{_yaml_quote(str(n.get("fingerprint", "chrome")))}"')
            if n.get('sni'):
                lines.append(f'    servername: "{_yaml_quote(str(n["sni"]))}"')
            if n.get('publicKey'):
                lines.append('    reality-opts:')
                lines.append(f'      public-key: "{_yaml_quote(str(n["publicKey"]))}"')
                lines.append(f'      short-id: "{_yaml_quote(str(n.get("shortId", "")))}"')
    lines.append('')
    lines.append('proxy-groups:')
    # PROXY 组：fallback 类型——选中节点优先，探测失败自动落到下一个可用节点（与 mihomo.rs 保持一致）
    lines.append('  - name: PROXY')
    lines.append('    type: fallback')
    lines.append('    url: http://www.gstatic.com/generate_204')
    lines.append('    interval: 60')
    lines.append('    proxies:')
    ordered = sorted(nodes, key=lambda x: 0 if x['name'] == selected else 1)
    for n in ordered:
        lines.append(f'      - "{_yaml_quote(str(n["name"]))}"')
    for n in nodes:
        # 组名与规则引用必须用同一净化结果（_sanitize_node_name），否则对不上
        lines.append(f'  - name: "NODE-{_yaml_quote(_sanitize_node_name(str(n["name"])))}"')
        lines.append('    type: select')
        lines.append('    proxies:')
        lines.append(f'      - "{_yaml_quote(str(n["name"]))}"')
    lines.append('')
    lines.append('rules:')
    # 两条「路」的死锁分流（IN-PORT 双保险）：从 7893 端口进来的无条件走 PROXY、
    # 从 7892 端口进来的无条件 DIRECT。优先级高于下面所有规则（含 GEOSITE,cn）。
    # 用户要求 mihomo 不自动判断国内外，决策权在智能体——自己选走代理还是直连。
    lines.append(f'  - IN-PORT,{PROXY_PORT},PROXY')
    lines.append(f'  - IN-PORT,{DIRECT_PORT},DIRECT')
    # 默认直连架构（与 src-tauri/src/mihomo.rs build_rules 保持一致，顺序即优先级）：
    # 第0层 保命直连名单：AI 助手中转站等目标永远走系统原生路由，绝不进隧道。
    for d in PROTECTED_DIRECT_DOMAINS:
        lines.append(f'  - IP-CIDR,{d}/32,DIRECT,no-resolve')
    # 第1层 进程级：防卷——节点服务器自身流量永远直连（多节点共用服务器时去重）
    seen_servers = set()
    for n in nodes:
        server = _sanitize_rule_field(str(n['server']))
        # 含非法字符（换行/逗号等）视为恶意输入，跳过，绝不生成规则
        if not server or server != str(n['server']).strip():
            continue
        if server in seen_servers:
            continue
        seen_servers.add(server)
        if server.replace('.', '').isdigit():
            lines.append(f'  - IP-CIDR,{server}/32,DIRECT,no-resolve')
        else:
            lines.append(f'  - DOMAIN-SUFFIX,{server},DIRECT')
    # 第2层 域名级：显式域名规则（target 支持 proxy/direct/节点名）
    for dr in cfg.get('domainRules', []):
        domain = _sanitize_rule_field(str(dr.get('domain', '')))
        if not domain or domain != str(dr.get('domain', '')).strip():
            continue
        t = dr.get('target', 'direct')
        if t == 'proxy':
            target = 'PROXY'
        elif t == 'direct':
            target = 'DIRECT'
        else:
            valid_nodes = {str(n.get('name', '')).strip() for n in nodes}
            target = (f'NODE-{_sanitize_node_name(str(t))}'
                      if str(t).strip() in valid_nodes else 'PROXY')
        lines.append(f'  - DOMAIN-SUFFIX,{domain},{target}')
    # 第2层续：国内域名/IP 清单直连，解决"软件设为代理后访问百度绕美国"
    lines.append('  - GEOSITE,cn,DIRECT')
    lines.append('  - GEOIP,CN,DIRECT')
    # 第3层 节点级：进程规则（软件默认去向），隧道内剩余流量按此转发
    for app in cfg.get('apps', []):
        if not app.get('confirmed'):
            continue
        paths = app_paths_for(app['id'])
        if app.get('mode') == 'proxy':
            node = app.get('node')
            valid_nodes = {str(n.get('name', '')).strip() for n in nodes}
            target = (f'NODE-{_sanitize_node_name(str(node))}'
                      if node and str(node).strip() in valid_nodes else 'PROXY')
        else:
            target = 'DIRECT'
        for p in paths:
            lines.append(f'  - PROCESS-PATH-REGEX,^{p},{target}')
    lines.append('  - GEOIP,LAN,DIRECT,no-resolve')
    lines.append('  - MATCH,DIRECT')
    return '\n'.join(lines) + '\n'


def app_paths_for(app_id):
    """根据 app id（app-<Name> 或 bin-<绝对路径>）返回 mihomo 规则路径前缀。"""
    import re as _re
    # bin- 前缀：非 .app 的裸二进制（如监控 daemon），id 后半段就是进程路径
    if app_id.startswith('bin-'):
        return [_re.escape(app_id[4:])]
    name = app_id
    if app_id.startswith('app-'):
        name = app_id[4:]
    paths = []
    for base in ['/Applications', '/System/Applications', '/System/Applications/Utilities']:
        app_dir = os.path.join(base, name + '.app')
        if os.path.isdir(app_dir):
            p = _re.escape(app_dir + '/Contents/')
            paths.append(p)
            break
    if not paths:
        # 未找到 App 时用名字构造常见路径
        paths.append(_re.escape('/Applications/' + name + '.app/Contents/'))
    if name == 'Safari':
        paths.append(_re.escape('/System/Library/Frameworks/WebKit.framework/'))
    return paths


def _write_mihomo_conf(conf_text):
    """写 mihomo.yaml 并收紧为 0600（含 API secret/节点 UUID/公钥，0644 会信息泄露）。"""
    path = RUNTIME_DIR + '/mihomo.yaml'
    with open(path, 'w') as f:
        f.write(conf_text)
    os.chmod(path, 0o600)


def regenerate_config():
    cfg = read_config()
    if 'error' in cfg:
        return cfg
    os.makedirs(RUNTIME_DIR, exist_ok=True)
    conf = generate_config(cfg)
    _write_mihomo_conf(conf)
    return {'ok': True}


def hot_reload_rules(cfg):
    """写完整 YAML 后重载运行中的 mihomo。
    优先 PUT /configs 全量重载（免 root、不弹授权框，实测进程规则完整保留）；
    失败再退回提权 SIGHUP。注意：PATCH /configs 会丢多条 PROCESS-PATH-REGEX，不可用。
    """
    try:
        os.makedirs(RUNTIME_DIR, exist_ok=True)
        conf_text = generate_config(cfg)
        conf_path = RUNTIME_DIR + '/mihomo.yaml'
        _write_mihomo_conf(conf_text)
        if mihomo_running():
            # 首选：API 全量重载
            try:
                body = json.dumps({'path': conf_path}).encode()
                req = urllib.request.Request(API + '/configs', data=body, method='PUT')
                req.add_header('Content-Type', 'application/json')
                _add_auth(req)
                _OPENER.open(req, timeout=10)
                return True
            except Exception:
                pass
            # 兜底：提权 SIGHUP（会弹管理员授权框）
            p = subprocess.run(['/usr/bin/pgrep', '-f', MIHOMO_PGREP_PATTERN], capture_output=True, text=True)
            pid = p.stdout.strip().split()[0] if p.stdout.strip() else ''
            if not pid:
                return False
            script = f'do shell script "/bin/kill -HUP {pid}" with administrator privileges'
            p2 = subprocess.run(['/usr/bin/osascript', '-e', script], capture_output=True, text=True, timeout=300)
            return p2.returncode == 0
        return False
    except Exception:
        return False


def _rule_reload_result(cfg, saved_message):
    """规则写入后的统一返回：保存成功但热更新失败必须明确暴露，不能假报 ok。"""
    if mihomo_running() and not hot_reload_rules(cfg):
        return {'ok': False, 'message': f'{saved_message}，但规则热更新失败；代理仍在运行，请重试或重启代理'}
    return {'ok': True, 'message': saved_message}


def _is_private_or_reserved_ip(ip):
    """判断 IP 是否为回环/内网/链路本地/组播/保留等非公网地址（与 Rust 侧 is_private_or_reserved 对齐）。"""
    import ipaddress
    try:
        a = ipaddress.ip_address(ip)
    except ValueError:
        return False
    return (a.is_loopback or a.is_private or a.is_link_local or a.is_multicast
            or a.is_unspecified or a.is_reserved)


def validate_public_host_py(host):
    """校验订阅 URL 的 host 是否指向公网，返回 (ok, err_msg)。
    与 Rust 侧 validate_public_host 对齐：拦字面内网 IP + 域名解析到内网的情况，
    堵住「域名解析到内网」的 SSRF 绕过。"""
    import ipaddress
    from urllib.parse import urlparse
    h = (host or '').strip()
    if not h:
        return False, '订阅地址 host 为空'
    # 去端口/方括号：统一取 host 部分
    if h.startswith('['):
        end = h.find(']')
        host_only = h[1:end] if end > 0 else h
    elif h.count(':') > 1:
        host_only = h  # 裸 IPv6
    else:
        host_only = h.rsplit(':', 1)[0] if ':' in h and h.rsplit(':', 1)[1].isdigit() else h
    if not host_only:
        return False, '订阅地址 host 为空'
    # 字面 IP：直接判内网
    try:
        ip = ipaddress.ip_address(host_only)
        if _is_private_or_reserved_ip(ip):
            return False, f'订阅地址指向内网/保留地址 {ip}，已拦截'
        return True, ''
    except ValueError:
        pass
    # 域名：DNS 解析后校验，任一结果落内网即拦截
    try:
        import socket
        infos = socket.getaddrinfo(host_only, 443, type=socket.SOCK_STREAM)
        for info in infos:
            ip_str = info[4][0]
            ip = ipaddress.ip_address(ip_str)
            if _is_private_or_reserved_ip(ip):
                return False, f'订阅域名 {host_only} 解析到内网/保留地址 {ip_str}，已拦截'
    except Exception:
        # DNS 解析失败不阻断（可能离线/临时失败），由 curl 实际拉取时再报错
        pass
    return True, ''


def fetch_subscription_from_url(url):
    """拉取订阅并解析 VLESS 节点。"""
    import base64
    import re
    from urllib.parse import unquote, urlparse
    url = (url or '').strip()
    # 只允许 http/https，防止 curl 访问 file:// 等本地协议造成信息外泄
    if not (url.startswith('http://') or url.startswith('https://')):
        return {'error': '订阅地址必须是 http:// 或 https:// 链接'}
    # SSRF 防护（与 Rust 侧 fetch_subscription 对齐）：解析 host 并拒绝内网/回环/保留地址，
    # 防止恶意调用借本工具拉取内网内容（如 127.0.0.1 服务、192.168.x 设备、内网域名）。
    try:
        parsed = urlparse(url)
        host = parsed.hostname or ''
        # hostname 可能带 userinfo 场景（urlparse 已剥离），这里 host 直接是纯 host
        ok, err = validate_public_host_py(host)
        if not ok:
            return {'error': err}
    except Exception as e:
        return {'error': f'订阅地址解析失败: {e}'}
    p = subprocess.run(['/usr/bin/curl', '-sL', '--max-time', '20',
                        '--noproxy', '*', '-A', 'Mozilla/5.0', url],
                       capture_output=True, text=True, timeout=30)
    if p.returncode != 0:
        return {'error': f'curl 拉取失败 rc={p.returncode}: {p.stderr[:100]}'}
    text = p.stdout
    if not any(x in text for x in ('vless://', 'vmess://', 'trojan://')):
        # 尝试 base64 解码
        try:
            decoded = base64.b64decode(text).decode('utf-8')
            if any(x in decoded for x in ('vless://', 'vmess://', 'trojan://')):
                text = decoded
        except Exception:
            pass
    nodes = []
    for line in text.splitlines():
        # 每行扫描任意协议前缀，取最早出现位置
        found = None
        for proto in ('vless://', 'vmess://', 'trojan://'):
            i = line.find(proto)
            if i >= 0 and (found is None or i < found[0]):
                found = (i, proto)
        if found is None:
            continue
        _idx, proto = found
        uri = line[_idx:].strip().split()[0]
        uri = uri.rstrip('\\"\'').rstrip(',')
        if proto == 'vmess://':
            node = parse_vmess_uri_py(uri[len('vmess://'):])
        elif proto == 'trojan://':
            node = parse_trojan_uri_py(uri[len('trojan://'):])
        else:
            node = parse_vless_uri_py(uri)
        if node:
            nodes.append(node)
    if not nodes:
        return {'error': '订阅中未解析到节点（支持 vless://、vmess://、trojan://）'}
    return nodes


def parse_vless_uri_py(uri):
    from urllib.parse import unquote, urlparse, parse_qs
    rest = uri[len('vless://'):]
    if '?' in rest:
        auth, after = rest.split('?', 1)
    else:
        auth, after = rest, ''
    if '#' in after:
        query, fragment = after.split('#', 1)
    else:
        query, fragment = after, ''
    if '@' not in auth:
        return None
    uuid, hostport = auth.rsplit('@', 1)
    if ':' not in hostport:
        return None
    host, port_str = hostport.rsplit(':', 1)
    try:
        port = int(port_str)
    except ValueError:
        return None
    params = {}
    for kv in query.split('&'):
        if '=' in kv:
            k, v = kv.split('=', 1)
            params[k] = unquote(v)
    return {
        'name': unquote(fragment) if fragment else host,
        'server': host,
        'port': port,
        'uuid': uuid,
        'flow': params.get('flow', ''),
        'network': params.get('type', 'tcp'),
        'tls': params.get('security', '') in ('reality', 'tls'),
        'udp': True,
        'fingerprint': params.get('fp', 'chrome'),
        'publicKey': params.get('pbk', ''),
        'shortId': params.get('sid', ''),
        'sni': params.get('sni', ''),
        'source': 'subscription',
        'region': '',
    }


def list_connections(limit=50):
    """实时连接快照：每条流的主机/命中规则/出口节点/流量。智能体看清流量的主入口。"""
    req = urllib.request.Request(API + '/connections')
    _add_auth(req)
    data = json.loads(_OPENER.open(req, timeout=8).read())
    conns = data.get('connections') or []
    out = []
    for c in conns:
        md = c.get('metadata', {})
        host = md.get('host') or md.get('destinationIP', '?')
        chains = c.get('chains') or []
        proc = (md.get('processPath') or '').split('/')[-1]
        out.append({
            'host': f"{host}:{md.get('destinationPort', '')}",
            'rule': f"{c.get('rule')}({c.get('rulePayload') or ''})",
            'node': chains[0] if chains else '?',
            'proc': proc,
            'up': c.get('upload', 0),
            'down': c.get('download', 0),
        })
    out.sort(key=lambda x: -x['down'])
    return {'total': len(conns), 'top': out[:limit]}


def node_health():
    """全部节点健康探测 + fallback 组当前状态"""
    cfg = read_config()
    if not mihomo_running():
        return {'error': '代理未运行'}
    results = {'nodes': [], 'fallback_group': None}
    for n in cfg.get('nodes', []):
        name = n['name']
        q = urllib.parse.quote(name, safe='')
        u = f'{API}/proxies/{q}/delay?timeout=5000&url=' + \
            urllib.parse.quote('https://www.gstatic.com/generate_204', safe='')
        req = urllib.request.Request(u)
        _add_auth(req)
        try:
            r = json.loads(_OPENER.open(req, timeout=10).read())
            results['nodes'].append({'node': name, 'ok': True, 'delay_ms': r.get('delay')})
        except Exception as e:
            results['nodes'].append({'node': name, 'ok': False, 'err': str(e)[:80]})
    try:
        req = urllib.request.Request(API + '/proxies/PROXY')
        _add_auth(req)
        g = json.loads(_OPENER.open(req, timeout=8).read())
        results['fallback_group'] = {'type': g.get('type'), 'using': g.get('now'), 'members': g.get('all')}
    except Exception as e:
        results['fallback_group'] = {'err': str(e)[:80]}
    return results


def download_proxy(args):
    """智能体下载前先看「两条路」并自己拍板，mihomo 不替它自动分流。

    返回两条物理上分开的入口（HTTP 代理地址）：
      - 节点代理：http://127.0.0.1:7893 —— 无条件走节点（PROXY）
      - 本机直连：http://127.0.0.1:7892 —— 进来的流量全部本机直连
    调用方（智能体）根据自己这次下载的目标自己决定走哪条，然后自行把请求
    送进对应端口。两条路是「死锁」的：进去后不再被 mihomo 二次判断国内外。

    可选参数 url：若给出，会返回该域名的「建议」（仅建议，不替智能体决定）：
      - 国内域名/IP → 建议直连（direct）
      - 其余 → 建议代理（proxy）
    """
    running = mihomo_running()
    if not running:
        return {'running': False, 'error': '代理未运行，两条路都不可用。请先 start_proxy'}
    resp = {
        'running': True,
        'proxy': f'http://127.0.0.1:{PROXY_PORT}',      # 节点代理：进来的流量全部走节点
        'direct': f'http://127.0.0.1:{DIRECT_PORT}',    # 本机直连：进来的流量全部本机直连
        'note': '两条路物理分开、死锁分流：proxy 端口进来的流量全部走节点，direct 端口进来的流量全部本机直连，'
                'mihomo 不做国内外自动判断。请自行决定本次下载走哪条。',
    }
    url = _str_arg(args, 'url')
    if url:
        # 仅给出「建议」，决策权仍在智能体
        try:
            host = urllib.parse.urlsplit(url).hostname or url
        except Exception:
            host = url
        suggestion = 'proxy'  # 默认建议走代理（保守：非国内域名的默认）
        if host:
            # 纯 IP 走 GEOIP 判断；域名走 GEOSITE 判断
            is_cn = False
            try:
                if host.replace('.', '').isdigit() or ':' in host:
                    # IP：查 mihomo GEOIP（CN）
                    req = urllib.request.Request(API + '/configs/geoip')
                    _add_auth(req)
                    # 简化：直接用内置判断——国内保留段粗略识别
                    import ipaddress
                    ip = ipaddress.ip_address(host)
                    is_cn = (ip.is_private or ip.is_loopback or ip.is_link_local
                             or _is_cn_ip(str(ip)))
                else:
                    # 域名：常见国内后缀 + 明确国内域名白名单（粗略，仅建议）
                    is_cn = host.endswith(('.cn', '.中国', '.公司', '.网络')) or \
                        any(host == d or host.endswith('.' + d) for d in CN_DOMAIN_SUFFIXES)
            except Exception:
                is_cn = False
            suggestion = 'direct' if is_cn else 'proxy'
        resp['suggestion'] = suggestion
        resp['suggestion_note'] = '仅建议，不替智能体决定：国内域名/IP 建议直连(direct)，其余建议代理(proxy)。'
    return resp


def _is_cn_ip(ip_str):
    """粗略判断 IP 是否为中国大陆段（仅用于 download_proxy 的「建议」，非权威）。"""
    import ipaddress
    ip = ipaddress.ip_address(ip_str)
    cn_ranges = [
        '1.0.1.0/24', '1.0.2.0/23', '1.0.8.0/21', '1.0.32.0/19', '1.1.0.0/24',
        '1.1.2.0/23', '1.1.4.0/22', '1.1.8.0/21', '1.1.16.0/20', '1.1.32.0/19',
        '14.0.0.0/22', '14.104.0.0/13', '27.8.0.0/13', '27.16.0.0/12', '27.36.0.0/14',
        '36.0.0.0/22', '36.4.0.0/14', '36.16.0.0/12', '36.32.0.0/14', '36.36.0.0/16',
        '36.37.0.0/19', '36.40.0.0/13', '36.48.0.0/15', '36.51.0.0/16', '36.56.0.0/13',
        '36.96.0.0/11', '36.128.0.0/10', '39.0.0.0/24', '39.64.0.0/11', '39.128.0.0/10',
        '42.0.0.0/22', '42.4.0.0/14', '42.48.0.0/13', '42.56.0.0/14', '42.62.0.0/17',
        '42.80.0.0/15', '42.83.0.0/17', '42.99.0.0/16', '42.100.0.0/14', '42.120.0.0/15',
        '42.122.0.0/16', '42.123.0.0/19', '42.128.0.0/12', '42.156.0.0/17', '42.160.0.0/12',
        '42.176.0.0/13', '42.184.0.0/15', '42.192.0.0/13', '42.200.0.0/12', '42.224.0.0/12',
        '49.4.0.0/14', '49.64.0.0/11', '49.112.0.0/13', '49.120.0.0/14', '49.128.0.0/24',
        '49.140.0.0/15', '49.152.0.0/14', '49.208.0.0/15', '49.220.0.0/14', '49.232.0.0/14',
        '49.239.0.0/18', '49.246.0.0/15', '54.222.0.0/15', '58.14.0.0/15', '58.16.0.0/13',
        '58.24.0.0/15', '58.30.0.0/15', '58.32.0.0/11', '58.64.0.0/13', '58.72.0.0/15',
        '58.100.0.0/15', '58.116.0.0/14', '58.128.0.0/13', '58.144.0.0/16', '58.208.0.0/12',
        '59.32.0.0/11', '59.64.0.0/12', '59.80.0.0/14', '59.107.0.0/16', '59.108.0.0/14',
        '59.151.0.0/17', '59.172.0.0/14', '59.191.0.0/17', '59.192.0.0/10', '60.0.0.0/11',
        '60.55.0.0/16', '60.63.0.0/16', '60.160.0.0/11', '60.194.0.0/15', '60.200.0.0/13',
        '60.208.0.0/12', '61.4.0.0/14', '61.48.0.0/13', '61.128.0.0/10', '61.232.0.0/14',
        '61.236.0.0/15', '61.240.0.0/14', '101.0.0.0/22', '101.16.0.0/12', '101.32.0.0/12',
        '101.48.0.0/15', '101.64.0.0/13', '101.72.0.0/14', '101.80.0.0/12', '101.96.0.0/11',
        '101.128.0.0/13', '101.224.0.0/13', '101.232.0.0/14', '101.236.0.0/14', '101.240.0.0/13',
        '103.1.0.0/22', '103.4.0.0/14', '103.8.0.0/13', '103.16.0.0/12', '103.32.0.0/14',
        '103.36.0.0/16', '103.40.0.0/13', '103.48.0.0/14', '103.52.0.0/14', '103.56.0.0/13',
        '103.96.0.0/11', '103.128.0.0/13', '103.192.0.0/14', '103.196.0.0/15', '103.224.0.0/14',
        '103.228.0.0/14', '103.232.0.0/13', '103.240.0.0/13', '103.248.0.0/14', '106.0.0.0/24',
        '106.0.2.0/23', '106.0.4.0/22', '106.0.8.0/21', '106.0.16.0/20', '106.0.64.0/18',
        '106.2.0.0/15', '106.4.0.0/14', '106.8.0.0/15', '106.11.0.0/16', '106.12.0.0/14',
        '106.16.0.0/12', '106.32.0.0/12', '106.48.0.0/15', '106.50.0.0/16', '106.52.0.0/14',
        '106.56.0.0/13', '106.74.0.0/15', '106.80.0.0/12', '106.108.0.0/14', '106.112.0.0/12',
        '106.224.0.0/12', '110.6.0.0/15', '110.16.0.0/14', '110.40.0.0/14', '110.48.0.0/16',
        '110.51.0.0/16', '110.52.0.0/15', '110.56.0.0/13', '110.64.0.0/15', '110.72.0.0/15',
        '110.75.0.0/16', '110.76.0.0/18', '110.80.0.0/13', '110.88.0.0/14', '110.96.0.0/11',
        '110.152.0.0/14', '110.156.0.0/15', '110.176.0.0/12', '110.192.0.0/11', '110.240.0.0/12',
        '111.0.0.0/10', '111.64.0.0/11', '111.96.0.0/14', '111.112.0.0/14', '111.116.0.0/15',
        '111.118.0.0/16', '111.119.0.0/19', '111.120.0.0/14', '111.124.0.0/16', '111.126.0.0/15',
        '111.128.0.0/11', '111.160.0.0/13', '111.170.0.0/16', '111.172.0.0/14', '111.176.0.0/13',
        '111.184.0.0/13', '111.192.0.0/12', '111.208.0.0/13', '111.224.0.0/13', '112.0.0.0/10',
        '112.64.0.0/14', '112.80.0.0/12', '112.96.0.0/13', '112.109.0.0/16', '112.111.0.0/16',
        '112.112.0.0/14', '112.116.0.0/15', '112.122.0.0/15', '112.124.0.0/14', '112.128.0.0/14',
        '112.132.0.0/16', '112.137.0.0/15', '112.192.0.0/14', '112.224.0.0/11', '113.0.0.0/13',
        '113.8.0.0/15', '113.11.0.0/16', '113.12.0.0/14', '113.16.0.0/15', '113.18.0.0/16',
        '113.24.0.0/14', '113.31.0.0/16', '113.44.0.0/14', '113.48.0.0/14', '113.52.0.0/15',
        '113.54.0.0/15', '113.56.0.0/15', '113.58.0.0/16', '113.59.0.0/17', '113.62.0.0/15',
        '113.64.0.0/10', '113.128.0.0/15', '113.130.0.0/16', '113.132.0.0/14', '113.136.0.0/13',
        '113.194.0.0/15', '113.200.0.0/15', '113.202.0.0/16', '113.204.0.0/14', '113.208.0.0/14',
        '113.212.0.0/18', '113.224.0.0/12', '114.28.0.0/16', '114.54.0.0/15', '114.60.0.0/14',
        '114.64.0.0/14', '114.68.0.0/16', '114.79.0.0/16', '114.80.0.0/12', '114.96.0.0/13',
        '114.104.0.0/14', '114.110.0.0/16', '114.112.0.0/14', '114.116.0.0/15', '114.118.0.0/16',
        '114.119.0.0/17', '114.119.192.0/18', '114.132.0.0/16', '114.135.0.0/16', '114.138.0.0/15',
        '114.215.0.0/16', '114.216.0.0/13', '114.224.0.0/11', '115.24.0.0/14', '115.28.0.0/15',
        '115.31.0.0/16', '115.32.0.0/14', '115.44.0.0/14', '115.48.0.0/12', '115.84.0.0/18',
        '115.100.0.0/14', '115.104.0.0/14', '115.120.0.0/14', '115.124.0.0/15', '115.148.0.0/14',
        '115.152.0.0/13', '115.168.0.0/13', '115.180.0.0/14', '115.192.0.0/11', '115.224.0.0/11',
        '116.0.0.0/12', '116.16.0.0/12', '116.32.0.0/14', '116.50.0.0/20', '116.52.0.0/14',
        '116.56.0.0/15', '116.58.0.0/16', '116.62.0.0/15', '116.66.0.0/17', '116.68.0.0/15',
        '116.76.0.0/14', '116.85.0.0/16', '116.89.0.0/17', '116.90.0.0/15', '116.95.0.0/16',
        '116.96.0.0/12', '116.112.0.0/14', '116.116.0.0/15', '116.128.0.0/10', '116.192.0.0/16',
        '116.193.0.0/16', '116.194.0.0/15', '116.196.0.0/12', '116.212.0.0/14', '116.216.0.0/14',
        '116.224.0.0/12', '116.242.0.0/15', '116.244.0.0/14', '116.248.0.0/15', '116.251.0.0/17',
        '116.252.0.0/15', '116.254.0.0/16', '117.8.0.0/13', '117.21.0.0/16', '117.22.0.0/15',
        '117.24.0.0/13', '117.32.0.0/13', '117.40.0.0/14', '117.44.0.0/15', '117.48.0.0/14',
        '117.53.0.0/17', '117.57.0.0/16', '117.58.0.0/17', '117.59.0.0/16', '117.60.0.0/14',
        '117.64.0.0/13', '117.72.0.0/15', '117.74.0.0/16', '117.79.0.0/16', '117.80.0.0/12',
        '117.96.0.0/14', '117.100.0.0/15', '117.103.0.0/16', '117.106.0.0/15', '117.112.0.0/13',
        '117.120.0.0/14', '117.124.0.0/14', '117.128.0.0/10', '118.24.0.0/13', '118.64.0.0/15',
        '118.66.0.0/16', '118.72.0.0/14', '118.80.0.0/15', '118.84.0.0/15', '118.88.0.0/13',
        '118.96.0.0/14', '118.100.0.0/15', '118.102.0.0/16', '118.112.0.0/13', '118.120.0.0/14',
        '118.124.0.0/15', '118.126.0.0/16', '118.127.0.0/19', '118.132.0.0/14', '118.144.0.0/14',
        '118.178.0.0/16', '118.180.0.0/14', '118.184.0.0/13', '118.192.0.0/13', '118.202.0.0/15',
        '118.204.0.0/14', '118.212.0.0/16', '118.213.0.0/16', '118.224.0.0/14', '118.228.0.0/15',
        '118.230.0.0/16', '118.239.0.0/16', '119.0.0.0/15', '119.2.0.0/19', '119.4.0.0/14',
        '119.8.0.0/15', '119.10.0.0/17', '119.15.0.0/16', '119.18.0.0/16', '119.28.0.0/15',
        '119.30.0.0/16', '119.32.0.0/14', '119.36.0.0/16', '119.38.0.0/16', '119.40.0.0/18',
        '119.44.0.0/15', '119.48.0.0/13', '119.57.0.0/16', '119.60.0.0/15', '119.62.0.0/16',
        '119.63.0.0/17', '119.80.0.0/15', '119.82.0.0/16', '119.84.0.0/14', '119.88.0.0/14',
        '119.96.0.0/13', '119.108.0.0/15', '119.112.0.0/12', '119.128.0.0/12', '119.144.0.0/14',
        '119.148.0.0/16', '119.151.0.0/16', '119.160.0.0/16', '119.161.0.0/19', '119.162.0.0/15',
        '119.164.0.0/14', '119.176.0.0/12', '119.232.0.0/15', '119.235.0.0/16', '119.248.0.0/14',
        '120.0.0.0/12', '120.24.0.0/14', '120.30.0.0/15', '120.32.0.0/12', '120.48.0.0/15',
        '120.52.0.0/14', '120.64.0.0/13', '120.72.0.0/15', '120.76.0.0/14', '120.80.0.0/13',
        '120.88.0.0/14', '120.92.0.0/16', '120.94.0.0/15', '120.128.0.0/13', '120.136.0.0/16',
        '120.137.0.0/17', '120.192.0.0/10', '121.0.0.0/16', '121.4.0.0/15', '121.8.0.0/13',
        '121.16.0.0/12', '121.32.0.0/14', '121.36.0.0/16', '121.37.0.0/16', '121.38.0.0/15',
        '121.40.0.0/14', '121.46.0.0/16', '121.48.0.0/15', '121.50.0.0/16', '121.51.0.0/16',
        '121.52.0.0/14', '121.56.0.0/14', '121.60.0.0/14', '121.68.0.0/14', '121.76.0.0/15',
        '121.79.0.0/16', '121.89.0.0/16', '121.100.0.0/16', '121.101.0.0/16', '121.192.0.0/13',
        '121.201.0.0/16', '121.204.0.0/14', '121.224.0.0/12', '121.248.0.0/14', '122.0.0.0/21',
        '122.4.0.0/14', '122.8.0.0/13', '122.48.0.0/15', '122.51.0.0/16', '122.64.0.0/11',
        '122.96.0.0/15', '122.102.0.0/20', '122.112.0.0/14', '122.119.0.0/16', '122.128.0.0/14',
        '122.136.0.0/13', '122.144.0.0/14', '122.152.0.0/14', '122.156.0.0/15', '122.188.0.0/14',
        '122.192.0.0/14', '122.200.0.0/13', '122.208.0.0/12', '122.224.0.0/12', '122.240.0.0/13',
        '122.248.0.0/14', '123.0.0.0/15', '123.4.0.0/14', '123.8.0.0/13', '123.49.0.0/16',
        '123.50.0.0/16', '123.52.0.0/14', '123.56.0.0/15', '123.58.0.0/16', '123.59.0.0/16',
        '123.60.0.0/15', '123.62.0.0/16', '123.64.0.0/11', '123.96.0.0/15', '123.98.0.0/17',
        '123.100.0.0/19', '123.101.0.0/16', '123.103.0.0/17', '123.108.0.0/15', '123.112.0.0/12',
        '123.128.0.0/13', '123.136.0.0/14', '123.160.0.0/12', '123.176.0.0/12', '123.196.0.0/15',
        '123.206.0.0/15', '123.232.0.0/14', '123.242.0.0/17', '123.244.0.0/14', '123.249.0.0/16',
        '124.42.0.0/16', '124.64.0.0/15', '124.66.0.0/16', '124.67.0.0/16', '124.68.0.0/14',
        '124.72.0.0/13', '124.88.0.0/13', '124.108.0.0/16', '124.112.0.0/13', '124.126.0.0/15',
        '124.128.0.0/13', '124.147.0.0/16', '124.151.0.0/16', '124.152.0.0/15', '124.160.0.0/13',
        '124.172.0.0/14', '124.192.0.0/15', '124.196.0.0/16', '124.200.0.0/13', '124.220.0.0/14',
        '124.224.0.0/12', '124.240.0.0/13', '124.248.0.0/15', '125.31.0.0/16', '125.32.0.0/12',
        '125.58.0.0/14', '125.62.0.0/15', '125.64.0.0/11', '125.96.0.0/13', '125.104.0.0/15',
        '125.112.0.0/12', '125.169.0.0/16', '125.171.0.0/16', '125.208.0.0/18', '125.216.0.0/13',
        '130.41.0.0/16', '139.9.0.0/16', '139.159.0.0/16', '139.217.0.0/16', '140.143.0.0/16',
        '144.0.0.0/16', '144.7.0.0/16', '144.12.0.0/16', '144.48.0.0/16', '144.52.0.0/16',
        '144.123.0.0/16', '144.255.0.0/16', '150.109.0.0/16', '150.129.0.0/16', '150.138.0.0/15',
        '150.158.0.0/16', '150.222.0.0/16', '150.223.0.0/16', '150.242.0.0/16', '150.248.0.0/16',
        '152.136.0.0/16', '153.0.0.0/16', '153.3.0.0/16', '153.34.0.0/15', '153.36.0.0/15',
        '153.99.0.0/16', '153.101.0.0/16', '153.118.0.0/15', '157.0.0.0/16', '157.61.0.0/16',
        '157.122.0.0/16', '157.148.0.0/16', '157.156.0.0/16', '157.255.0.0/16', '159.75.0.0/16',
        '159.226.0.0/16', '160.19.0.0/16', '160.20.0.0/16', '160.202.0.0/16', '160.238.0.0/16',
        '161.120.0.0/16', '161.189.0.0/16', '161.207.0.0/16', '162.14.0.0/16', '162.105.0.0/16',
        '163.0.0.0/16', '163.125.0.0/16', '163.142.0.0/16', '163.177.0.0/16', '163.179.0.0/16',
        '163.204.0.0/16', '166.111.0.0/16', '167.139.0.0/16', '168.160.0.0/16', '171.8.0.0/13',
        '171.34.0.0/15', '171.36.0.0/14', '171.40.0.0/13', '171.80.0.0/12', '171.104.0.0/13',
        '171.112.0.0/12', '171.208.0.0/12', '175.0.0.0/12', '175.16.0.0/13', '175.24.0.0/14',
        '175.30.0.0/15', '175.42.0.0/15', '175.44.0.0/16', '175.46.0.0/15', '175.48.0.0/12',
        '175.64.0.0/11', '175.102.0.0/16', '175.106.0.0/16', '175.146.0.0/15', '175.148.0.0/16',
        '175.152.0.0/14', '175.158.0.0/16', '175.160.0.0/13', '175.178.0.0/16', '175.184.0.0/16',
        '175.185.0.0/16', '175.186.0.0/16', '175.188.0.0/16', '180.76.0.0/15', '180.84.0.0/15',
        '180.86.0.0/16', '180.88.0.0/14', '180.94.0.0/15', '180.96.0.0/11', '180.129.0.0/16',
        '180.130.0.0/16', '180.136.0.0/14', '180.148.0.0/16', '180.149.0.0/16', '180.150.0.0/16',
        '180.152.0.0/13', '180.160.0.0/13', '180.178.0.0/15', '180.184.0.0/14', '180.188.0.0/17',
        '180.192.0.0/12', '180.208.0.0/14', '180.212.0.0/14', '180.224.0.0/13', '180.233.0.0/16',
        '180.235.0.0/16', '180.240.0.0/13', '180.248.0.0/15', '182.16.0.0/16', '182.18.0.0/16',
        '182.32.0.0/12', '182.48.0.0/14', '182.54.0.0/16', '182.61.0.0/16', '182.80.0.0/13',
        '182.88.0.0/14', '182.92.0.0/16', '182.96.0.0/11', '182.128.0.0/12', '182.144.0.0/13',
        '182.157.0.0/16', '182.160.0.0/13', '182.200.0.0/13', '182.236.0.0/15', '182.238.0.0/16',
        '182.239.0.0/19', '182.240.0.0/13', '182.254.0.0/16', '183.0.0.0/10', '183.64.0.0/13',
        '183.78.0.0/16', '183.81.0.0/16', '183.84.0.0/15', '183.91.0.0/19', '183.92.0.0/14',
        '183.128.0.0/11', '183.160.0.0/13', '183.168.0.0/15', '183.170.0.0/16', '183.172.0.0/14',
        '183.182.0.0/19', '183.184.0.0/13', '183.192.0.0/10', '202.0.0.0/16', '202.4.0.0/14',
        '202.8.0.0/15', '202.12.0.0/16', '202.14.0.0/16', '202.20.0.0/16', '202.38.0.0/16',
        '202.43.0.0/16', '202.45.0.0/16', '202.46.0.0/16', '202.47.0.0/16', '202.57.0.0/16',
        '202.60.0.0/16', '202.62.0.0/16', '202.63.0.0/16', '202.75.0.0/16', '202.76.0.0/16',
        '202.79.0.0/16', '202.84.0.0/16', '202.85.0.0/16', '202.86.0.0/16', '202.87.0.0/16',
        '202.90.0.0/16', '202.91.0.0/16', '202.96.0.0/12', '202.112.0.0/13', '202.120.0.0/15',
        '202.127.0.0/16', '202.130.0.0/16', '202.141.0.0/16', '202.142.0.0/16', '202.143.0.0/16',
        '202.152.0.0/16', '202.158.0.0/16', '202.160.0.0/16', '202.165.0.0/16', '202.170.0.0/16',
        '202.181.0.0/16', '202.182.0.0/16', '202.189.0.0/16', '202.192.0.0/12', '203.80.0.0/16',
        '203.86.0.0/16', '203.90.0.0/16', '203.91.0.0/16', '203.93.0.0/16', '203.94.0.0/16',
        '203.95.0.0/16', '203.100.0.0/16', '203.104.0.0/16', '203.105.0.0/16', '203.107.0.0/16',
        '203.110.0.0/16', '203.112.0.0/16', '203.114.0.0/16', '203.118.0.0/16', '203.119.0.0/16',
        '203.130.0.0/16', '203.132.0.0/16', '203.134.0.0/16', '203.135.0.0/16', '203.142.0.0/16',
        '203.148.0.0/16', '203.149.0.0/16', '203.160.0.0/16', '203.164.0.0/16', '203.175.0.0/16',
        '203.184.0.0/16', '203.187.0.0/16', '203.189.0.0/16', '203.191.0.0/16', '203.192.0.0/16',
        '203.195.0.0/16', '203.207.0.0/16', '203.208.0.0/16', '203.212.0.0/16', '203.217.0.0/16',
        '203.223.0.0/16', '210.5.0.0/16', '210.12.0.0/15', '210.14.0.0/15', '210.16.0.0/14',
        '210.21.0.0/16', '210.22.0.0/16', '210.32.0.0/12', '210.51.0.0/16', '210.52.0.0/15',
        '210.56.0.0/16', '210.72.0.0/14', '210.76.0.0/15', '210.78.0.0/16', '210.79.0.0/16',
        '210.82.0.0/16', '210.87.0.0/16', '210.192.0.0/14', '211.64.0.0/13', '211.80.0.0/12',
        '211.96.0.0/13', '211.136.0.0/13', '211.144.0.0/12', '211.160.0.0/13', '211.224.0.0/13',
        '218.0.0.0/11', '218.56.0.0/13', '218.64.0.0/11', '218.96.0.0/14', '218.100.0.0/16',
        '218.104.0.0/13', '218.185.0.0/16', '218.192.0.0/12', '218.240.0.0/13', '218.249.0.0/16',
        '219.72.0.0/16', '219.82.0.0/16', '219.128.0.0/11', '219.216.0.0/13', '219.232.0.0/15',
        '219.234.0.0/15', '219.236.0.0/15', '219.238.0.0/15', '220.101.0.0/16', '220.112.0.0/14',
        '220.152.0.0/14', '220.160.0.0/11', '220.192.0.0/12', '220.231.0.0/18', '220.232.0.0/15',
        '220.234.0.0/16', '220.242.0.0/16', '220.247.0.0/16', '220.248.0.0/14', '220.252.0.0/16',
        '221.0.0.0/13', '221.8.0.0/14', '221.12.0.0/17', '221.13.0.0/16', '221.14.0.0/15',
        '221.122.0.0/15', '221.128.0.0/15', '221.130.0.0/16', '221.131.0.0/16', '221.133.0.0/16',
        '221.136.0.0/15', '221.172.0.0/14', '221.176.0.0/13', '221.192.0.0/14', '221.196.0.0/15',
        '221.198.0.0/16', '221.199.0.0/16', '221.200.0.0/14', '221.203.0.0/16', '221.204.0.0/15',
        '221.206.0.0/16', '221.207.0.0/16', '221.208.0.0/12', '221.224.0.0/12', '222.16.0.0/12',
        '222.32.0.0/11', '222.64.0.0/11', '222.126.0.0/16', '222.128.0.0/12', '222.160.0.0/14',
        '222.168.0.0/13', '222.176.0.0/12', '222.192.0.0/11', '222.240.0.0/13', '222.248.0.0/15',
        '223.0.0.0/12', '223.20.0.0/15', '223.27.0.0/16', '223.64.0.0/10', '223.128.0.0/15',
        '223.144.0.0/12', '223.160.0.0/14', '223.166.0.0/16', '223.192.0.0/15', '223.198.0.0/16',
        '223.201.0.0/16', '223.202.0.0/16', '223.208.0.0/13', '223.220.0.0/15', '223.223.0.0/16',
        '223.240.0.0/13', '223.248.0.0/14', '223.252.0.0/16',
    ]
    return any(ip in net for net in (ipaddress.ip_network(r) for r in cn_ranges))


def _http_get_via(proxy_port, url, timeout=12, read_bytes=256 * 1024):
    """通过指定端口（7892 直连 / 7893 代理）访问 url，实测延迟与下载吞吐。

    返回 dict：{ok, status, latency_ms, tcp_ms, first_byte_ms, read_ms, bytes,
               speed_mbps, error}。
    - latency_ms：从发起到收到首字节的墙钟时间（含 TCP + TLS + 首包）
    - speed_mbps：读 read_bytes 字节的实测吞吐（单位兆比特/秒）
    """
    t0 = time.time()
    try:
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({'http': f'http://127.0.0.1:{proxy_port}',
                                         'https': f'http://127.0.0.1:{proxy_port}'}))
        req = urllib.request.Request(url, headers={'User-Agent': 'magic-agent/0.1'})
        # 用 read() 到指定字节数测吞吐，读完即断开（不整文件拉）
        resp = opener.open(req, timeout=timeout)
        status = getattr(resp, 'status', None)
        first_byte_ms = int((time.time() - t0) * 1000)
        chunk = resp.read(read_bytes)
        read_ms = max(1, (time.time() - t0) * 1000 - first_byte_ms)
        n = len(chunk)
        # 吞吐 = 字节 / 读取耗时，转 Mbps（×8/1e6，再 ×1000ms/s）
        speed_mbps = round(n * 8 / 1e6 / (read_ms / 1000.0), 2) if read_ms > 0 else 0.0
        return {'ok': True, 'status': status, 'latency_ms': first_byte_ms,
                'read_ms': int(read_ms), 'bytes': n, 'speed_mbps': speed_mbps}
    except Exception as e:
        # 友好化常见网络错误：原始 urllib/ssl 堆栈（如 "UNEXPECTED_EOF_WHILE_READING"）
        # 对调用者没有行动价值，翻译成结论性描述。
        raw = str(e)
        if 'UNEXPECTED_EOF' in raw or 'EOF occurred' in raw:
            msg = 'TLS 连接被中断（目标被墙/链路重置）'
        elif 'timed out' in raw or 'timeout' in raw.lower():
            msg = '连接超时'
        elif 'Connection refused' in raw:
            msg = '连接被拒绝（本机代理端口未监听？）'
        elif 'Name or service not known' in raw or 'nodename nor servname' in raw:
            msg = 'DNS 解析失败'
        else:
            msg = raw[:120]
        return {'ok': False, 'latency_ms': int((time.time() - t0) * 1000), 'error': msg}


def probe_route(args):
    """【拿不准走哪条路时先调这个】实测「本机直连(7892) vs 节点代理(7893)」
    到同一个目标 url 的真实延迟 + 下载吞吐，返回对比数据 + 一个明确结论。

    这是让两条路「变聪明」的核心：不再凭「国内/国外」规则猜，而是实测路况。
    - 目标国外站时：可能直连也通（你的本机本来就能上外网），但走代理更稳/更快；
    - 目标国内站时：直连几乎必然更快，走代理是绕远路。
    探完你就知道这次下载/访问该走代理还是直连，不用猜。

    参数 url 必填。可选 timeout（秒，默认 12）、read_bytes（测吞吐读的字节数，默认 256KB）。
    返回 example：{"url":..., "routes":[{"name":"本机直连 127.0.0.1:7892","port":7892,...},{"name":"节点代理 127.0.0.1:7893","port":7893,...}],
                  "conclusion":"..."}
    """
    url = _str_arg(args, 'url')
    if not url:
        return {'error': 'url 必填，如 probe_route {"url":"https://huggingface.co"}'}
    # 参数校验（P1-5）：只接受 http/https，其余 scheme（file://、ftp:// 等）直接拒绝——
    # 防探测器被诱导去请求本机/内网文件路径等非预期目标。
    if not url.startswith(('http://', 'https://')):
        return {'error': f'url 仅支持 http/https scheme（收到: {url[:80]}）'}
    # 拒绝把任何非 URL 文本静默拼成 https（旧行为会把 file:///etc/passwd 变成无意义探测）
    if not mihomo_running():
        return {'error': '代理未运行，两条路都不可用。请先 start_proxy'}
    try:
        timeout = int((args or {}).get('timeout', 12))
        read_bytes = int((args or {}).get('read_bytes', 256 * 1024))
    except (TypeError, ValueError):
        timeout, read_bytes = 12, 256 * 1024
    timeout = max(3, min(timeout, 60))          # 兜底：3~60 秒
    read_bytes = max(1024, min(read_bytes, 4 * 1024 * 1024))  # 兜底：1KB~4MB

    routes = []
    # 命名直白写清「做什么+地址端口」，不再只用比喻：直连（不经过任何节点）
    # vs 代理（流量从当前选中的国外节点出去）。
    for label, port in ((f'本机直连 127.0.0.1:{DIRECT_PORT}', DIRECT_PORT),
                        (f'节点代理 127.0.0.1:{PROXY_PORT}', PROXY_PORT)):
        r = _http_get_via(port, url, timeout=timeout, read_bytes=read_bytes)
        r['name'] = label
        r['port'] = port
        routes.append(r)

    # 生成结论（只陈述实测结果，不替智能体做「该走哪条」的强制命令）
    ok = [r for r in routes if r.get('ok')]
    if not ok:
        conclusion = '两条路都连不上该目标，可能目标本身不可达或网络异常。'
    elif len(ok) == 1:
        conclusion = f'只有「{ok[0]["name"]}」能连通，另一条路失败。'
    else:
        direct = routes[0]
        proxy = routes[1]
        faster = direct['name'] if (direct.get('speed_mbps') or 0) >= (proxy.get('speed_mbps') or 0) else proxy['name']
        conclusion = (f'两条路都通。实测吞吐：{direct["name"]} {direct.get("speed_mbps")} Mbps vs '
                      f'{proxy["name"]} {proxy.get("speed_mbps")} Mbps，{faster}更快。'
                      f'延迟：直连 {direct.get("latency_ms")}ms / 代理 {proxy.get("latency_ms")}ms。')
    return {'url': url, 'routes': routes, 'conclusion': conclusion}


# ============ 云服务器管理（SSH 探针） ============
# SSH 凭据与 App(Rust) 共用同一份：密码存 macOS Keychain（service=com.magic.agent，
# account=ssh-password-<user>@<host>），config.json 只存 host/port/user/auth 不存明文密码。
KEYCHAIN_SERVICE = 'com.magic.agent'


def _keychain_password(host, user):
    """从 macOS Keychain 读取 SSH 密码（与 App 的 keychain.rs 共用同一 account）。"""
    account = f'ssh-password-{user}@{host}'
    p = subprocess.run(['/usr/bin/security', 'find-generic-password',
                        '-s', KEYCHAIN_SERVICE, '-a', account, '-w'],
                       capture_output=True, text=True)
    if p.returncode != 0:
        return None
    return p.stdout.strip()


def _keychain_key(host, user):
    """从 macOS Keychain 读取 SSH 私钥内容（与 Rust 端 key_account 共用同一 account）。"""
    account = f'ssh-key-{user}@{host}'
    p = subprocess.run(['/usr/bin/security', 'find-generic-password',
                        '-s', KEYCHAIN_SERVICE, '-a', account, '-w'],
                       capture_output=True, text=True)
    if p.returncode != 0:
        return None
    return p.stdout


def _active_server():
    """读取当前激活的云服务器连接信息。返回 dict 或 None。"""
    cfg = read_config()
    if 'error' in cfg:
        return None
    servers = cfg.get('servers', [])
    active_id = cfg.get('activeServerId')
    if active_id:
        for s in servers:
            if s.get('id') == active_id:
                return s
    if servers:
        return servers[0]
    # 兼容旧字段
    host = cfg.get('sshHost')
    # 没有显式 SSH 配置时，从选中的代理节点推导主机——
    # 用户的代理节点就部署在云服务器上，SSH 和代理是同一台机器，
    # 不再要求用户在 SSH 页面重新填一遍服务器地址。
    if not host:
        selected = cfg.get('selectedNode')
        for n in cfg.get('nodes', []):
            if n.get('name') == selected:
                host = n.get('server')
                break
    if host:
        return {'id': f'ssh-{host}', 'name': host, 'host': host,
                'port': cfg.get('sshPort', 22), 'user': cfg.get('sshUser', 'root'),
                'auth': cfg.get('sshAuth', 'password')}
    return None


def _shell_quote(s):
    """给 expect spawn 的参数做单引号包裹（转义内部单引号），与 Rust 端 shell_quote 对齐。"""
    return "'" + s.replace("'", "'\\''") + "'"


def _tcl_escape(s):
    """把字符串安全地放进 Tcl 双引号字面量：转义反斜杠、双引号、美元符、反引号、方括号。"""
    return (s.replace('\\', '\\\\').replace('"', '\\"')
             .replace('$', '\\$').replace('`', '\\`')
             .replace('[', '\\[').replace(']', '\\]'))


def ssh_exec(command, timeout_secs=15):
    """在激活的云服务器上非交互式执行单条命令，返回 (stdout, stderr, exit_code)。
    供 server_metrics 等工具使用。密码从 Keychain 读取，不落盘。"""
    srv = _active_server()
    if not srv:
        return ('', '尚未配置云服务器：请先在 App「云服务器」页添加 SSH 连接', -1)
    host = srv.get('host', '')
    port = int(srv.get('port', 22))
    user = srv.get('user', 'root')
    auth = srv.get('auth', 'password')
    key_path = srv.get('keyPath') or srv.get('key_path') or srv.get('private_key')
    key_tmp = None
    if auth == 'key':
        if key_path:
            key_path = os.path.expanduser(str(key_path))
        else:
            key_content = _keychain_key(host, user)
            if key_content:
                import tempfile
                fd, key_tmp = tempfile.mkstemp(prefix='magic-ssh-key-')
                try:
                    os.write(fd, key_content.encode())
                finally:
                    os.close(fd)
                os.chmod(key_tmp, 0o600)
                key_path = key_tmp

    args = ['/usr/bin/ssh', '-o', 'StrictHostKeyChecking=accept-new',
            '-o', 'ConnectTimeout=10', '-o', 'BatchMode=no']
    if port != 22:
        args += ['-p', str(port)]
    # 密钥认证：显式指定 -i 密钥文件（不依赖 ~/.ssh/config 的 Host 别名匹配，
    # 因为这里用的是 host IP 而非别名）。展开 ~ 到绝对路径。
    if auth == 'key' and key_path:
        args += ['-i', key_path]
    args += [f'{user}@{host}', command]

    pw = _keychain_password(host, user) if auth == 'password' else None
    if pw:
        # 用 expect 喂密码（密码不进 argv，不落盘）。
        # 关键：spawn 的每个参数都要 shell 引号包裹，否则命令里的 ';' '|' 空格
        # 会被 expect 错误拆分，导致 server_metrics 探针命令根本无法执行。
        spawn_args = ' '.join(_shell_quote(a) for a in args)
        script = ('#!/usr/bin/expect -f\n'
                  'set timeout {t}\n'
                  'spawn {spawn_args}\n'
                  'expect {{\n'
                  '  -re "(?i)password:\\\\s*" {{ send "{pw}\\r" }}\n'
                  '  -re "Are you sure.*" {{ send "yes\\r"; exp_continue }}\n'
                  '  eof {{ exit 1 }}\n'
                  '}}\n'
                  'expect eof\n'
                  'set rc [lindex [wait] 3]\n'
                  'exit $rc\n'.format(
                      t=timeout_secs, spawn_args=spawn_args, pw=_tcl_escape(pw)))
        try:
            p = subprocess.run(['/usr/bin/expect', '-f', '-'], input=script,
                               capture_output=True, text=True, timeout=timeout_secs + 10)
        except subprocess.TimeoutExpired:
            return ('', f'SSH 执行超时（>{timeout_secs}s）', -1)
        except FileNotFoundError:
            return ('', '未找到 /usr/bin/expect，密码认证需要它。请改用密钥认证。', -1)
    else:
        try:
            p = subprocess.run(args, capture_output=True, text=True, timeout=timeout_secs + 5)
        except subprocess.TimeoutExpired:
            return ('', f'SSH 执行超时（>{timeout_secs}s）', -1)
        except FileNotFoundError:
            return ('', '未找到 /usr/bin/ssh。', -1)
        finally:
            if key_tmp:
                try:
                    os.remove(key_tmp)
                except OSError:
                    pass
    return (p.stdout or '', p.stderr or '', p.returncode)


def server_metrics(args=None):
    """云服务器一键探针：采集 CPU/内存/磁盘/带宽/负载/在线时长，返回结构化 JSON。
    这是把云服务器纳入网络管理「可控范围」的核心——智能体能远程看清服务器状态，
    而不只是盲敲命令。"""
    srv = _active_server()
    if not srv:
        return {'error': '尚未配置云服务器。请在 App「云服务器」页添加 SSH 连接'
                         '（host/port/user/password），之后我就能远程查看它的状态。'}
    cmd = ("echo '---CPU---'; top -bn1 | grep 'Cpu(s)' || echo 'n/a'; "
           "echo '---MEM---'; free -m | grep 'Mem' || echo 'n/a'; "
           "echo '---DISK---'; df -h / | tail -1 || echo 'n/a'; "
           "echo '---LOAD---'; cat /proc/loadavg 2>/dev/null || sysctl -n vm.loadavg 2>/dev/null || echo 'n/a'; "
           "echo '---UPTIME---'; uptime | sed 's/^ *//' || echo 'n/a'; "
           "echo '---NET---'; cat /proc/net/dev | grep -E 'eth0|ens|enp' | head -5 || echo 'n/a'")
    out, err, code = ssh_exec(cmd, timeout_secs=20)
    if code != 0:
        return {'error': f'探针执行失败 (exit {code}): {err.strip()[:150]}'}
    metrics = _parse_server_metrics(out)
    metrics['server'] = {'name': srv.get('name', srv.get('host')),
                         'host': srv.get('host'), 'user': srv.get('user')}
    return metrics


def _parse_server_metrics(raw):
    """解析探针原始输出为 dict（与 Rust 端 parse_server_metrics 对齐）。"""
    m = {}
    section = ''
    for line in raw.splitlines():
        t = line.strip()
        if t.startswith('---') and t.endswith('---'):
            section = t.strip('-').strip()
            continue
        if section == 'CPU':
            if t.startswith('Cpu') or t.startswith('%Cpu') or 'us,' in t:
                m['cpu_usage_pct'] = round(100.0 - _extract_pct(t, 'id'), 1)
        elif section == 'MEM':
            cols = t.split()
            if len(cols) >= 7 and cols[0] == 'Mem:':
                try:
                    total = float(cols[1]); used = float(cols[2]); avail = float(cols[6])
                    m['mem_total_mb'] = total; m['mem_used_mb'] = used; m['mem_avail_mb'] = avail
                    m['mem_usage_pct'] = round(used / total * 100.0, 1)
                except ValueError:
                    pass
        elif section == 'DISK':
            cols = t.split()
            if len(cols) >= 5 and '/' in t:
                m['disk_size'] = cols[1]; m['disk_used'] = cols[2]
                m['disk_avail'] = cols[3]; m['disk_usage_pct'] = cols[4].rstrip('%')
        elif section == 'LOAD':
            cols = t.split()
            if len(cols) >= 3:
                m['load_1m'] = cols[0]; m['load_5m'] = cols[1]; m['load_15m'] = cols[2]
        elif section == 'UPTIME':
            m['uptime'] = t
        elif section == 'NET':
            if ':' in t:
                ifname = t.split(':')[0].strip()
                nums = t.split(':')[1].split()
                if len(nums) >= 10:
                    m[f'net_{ifname}_rx_bytes'] = float(nums[0])
                    m[f'net_{ifname}_tx_bytes'] = float(nums[8])
    m['probe_ok'] = True
    return m


def _extract_pct(line, key):
    """从 top 的 CPU 行抓字段值（"89.8 id" 形式，值在字段名前）。"""
    for part in line.split(','):
        words = part.split()
        prev = None
        for w in words:
            if w.rstrip(',') == key:
                if prev is not None:
                    return prev
            try:
                prev = float(w.rstrip(',%'))
            except ValueError:
                prev = None
    return 0.0


# ─────────────────────────────────────────────────────────────────────────────
# P1-1 audit_network：网络体检（只读采集）。与 Rust 侧 auditor.rs 同口径：
# 同一份第三方名单、同一组死端口、同样的降级原则（单项失败→unknown 不炸整报）。
# ─────────────────────────────────────────────────────────────────────────────

_AUDIT_FOREIGN_APPS = [
    'flclash', 'clash verge', 'clash-verge', 'clashverge', 'clash for windows',
    'clashx', 'clash-nyanpasu', 'clash.meta', 'v2rayx', 'v2rayu', 'qv2ray',
    'shadowsocksx', 'shadowsocks-ng', 'surge', 'quantumult', 'stash', 'loon',
    'sing-box', 'singbox', 'trojan', 'naiveproxy', 'hysteria', 'xray', 'v2ray',
]
_AUDIT_FOREIGN_CORES = [
    'flclashcore', 'clash-verge-service', 'clash-verge-service-ipc', 'verge-mihomo',
    'clash-meta', 'clash-meta-core', 'sing-box', 'v2ray-core', 'xray-core',
    'hysteria', 'naive', 'trojan-go',
]
# 本程序端口（与 Rust MihomoManager: port=7891, PROXY_PORT=7893, DIRECT_PORT=7892 一致）
_AUDIT_OWN_PORTS = [7891, 7892, 7893]


def _audit_run(bin_, args, cap=3.0):
    """跑一条采集命令，超时/失败返回 None（降级），绝不抛异常。"""
    try:
        p = subprocess.run([bin_] + args, capture_output=True, text=True, timeout=cap)
        return p.stdout if p.returncode == 0 or p.stdout else None
    except Exception:
        return None


def _audit_foreign_procs():
    """第三方代理进程：只匹配可执行路径首 token（与 Rust find_foreign_proxies 同铁律，
    绝不匹配整个命令行，防误杀 grep/编辑器）。"""
    out = _audit_run('/bin/ps', ['-axo', 'pid=,args='], 5)
    if out is None:
        return None
    found = []
    me = str(os.getpid())
    ppid = str(os.getppid())
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) < 2:
            continue
        pid, args_s = parts[0], parts[1]
        if pid in (me, ppid):
            continue
        low = args_s.lower()
        if ('magic-agent' in low or 'magic_probe' in low or 'dump_conf' in low
                or RUNTIME_DIR.lower() in low):
            continue
        exe = low.split()[0]
        exe_name = exe.rsplit('/', 1)[-1]
        hit = None
        for key in _AUDIT_FOREIGN_APPS:
            if key in exe or exe_name == key:
                hit = key
                break
        if not hit:
            for core in _AUDIT_FOREIGN_CORES:
                if core in exe_name:
                    hit = core + ' 内核'
                    break
        if hit:
            found.append(f'{hit} (PID {pid})')
    return found


def _audit_listen():
    """LISTEN 套接字摘要 + 端口占用者映射。"""
    out = _audit_run('/usr/sbin/lsof', ['-nP', '-iTCP', '-sTCP:LISTEN'], 3)
    if out is None:
        return None, None
    sockets, holders = [], {}
    for line in out.splitlines()[1:]:
        cols = line.split()
        if len(cols) < 9 or cols[-1] != '(LISTEN)':
            continue
        name = cols[-2]
        idx = name.rfind(':')
        if idx < 0:
            continue
        addr, port_s = name[:idx], name[idx + 1:]
        try:
            port = int(port_s)
        except ValueError:
            continue
        addr_low = addr.lower()
        lan = not (addr_low.startswith('127.') or addr in ('::1', '[::1]', 'localhost'))
        sockets.append({'command': cols[0], 'pid': cols[1], 'port': port, 'lanExposed': lan})
        holders.setdefault(port, []).append((cols[0], cols[1]))
    return sockets, holders


def _audit_stale_and_pac(holders):
    """系统代理残留（指向本程序死端口但内核没在跑）+ PAC。
    kernel_up 判定与 Rust 侧一致：按本程序内核进程存在与否（pgrep runtime 路径），
    绝不能用"own 端口有 LISTEN"——第三方占用 7891 时会把真·死端口残留漏报。
    只报告不自愈（改状态是 P1-3 编排的事）。"""
    text = _audit_run('/usr/sbin/scutil', ['--proxy'], 3)
    if text is None:
        return {'detected': False, 'detail': 'scutil 采集失败（unknown）'}, 'unknown'
    pac = ('yes' if 'ProxyAutoConfigEnable : 1' in text
           else 'no' if 'ProxyAutoConfigEnable : 0' in text else 'unknown')

    def gport(key):
        for line in text.splitlines():
            l = line.strip()
            if l.startswith(key):
                try:
                    return int(l[len(key):].strip().lstrip(':').strip())
                except ValueError:
                    return None
        return None

    pg = _audit_run('/usr/bin/pgrep', ['-f', MIHOMO_PGREP_PATTERN], 3)
    kernel_up = bool(pg and pg.strip())
    hits = []
    if not kernel_up:
        if 'HTTPEnable : 1' in text:
            p = gport('HTTPPort')
            if p in _AUDIT_OWN_PORTS:
                hits.append(f'HTTP 代理指向死端口 {p}')
        if 'SOCKSEnable : 1' in text:
            p = gport('SOCKSPort')
            if p in _AUDIT_OWN_PORTS:
                hits.append(f'SOCKS 代理指向死端口 {p}')
    return {'detected': bool(hits), 'detail': '；'.join(hits)}, pac


def _audit_route():
    text = _audit_run('/sbin/route', ['-n', 'get', 'default'], 3)
    if text is None:
        return {'state': 'unknown'}
    gw = iface = ''
    for line in text.splitlines():
        l = line.strip()
        if l.startswith('gateway:'):
            gw = l.split(':', 1)[1].strip()
        elif l.startswith('interface:'):
            iface = l.split(':', 1)[1].strip()
    if not gw and not iface:
        return {'state': 'unknown'}
    return {'state': 'ok', 'gateway': gw, 'interface': iface,
            'tunInterface': iface.startswith('utun')}


def _audit_dns():
    text = _audit_run('/usr/sbin/scutil', ['--dns'], 3)
    if text is None:
        return {'state': 'unknown', 'nameservers': []}
    ns = []
    in_first = False
    for line in text.splitlines():
        l = line.strip()
        if l.startswith('resolver #'):
            if l == 'resolver #1':
                in_first = True
            else:
                break  # 只取 #1（系统默认出口），后续是分流 resolver
        elif in_first and l.startswith('nameserver['):
            rest = l.split(']', 1)[-1].strip().lstrip(':').strip()
            if rest:
                ns.append(rest)
    return {'state': 'ok' if ns else 'unknown', 'nameservers': ns}


def _audit_env():
    names = ['http_proxy', 'https_proxy', 'all_proxy', 'ftp_proxy', 'no_proxy',
             'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY']
    vars_ = [f'{n}={os.environ[n]}' for n in names if os.environ.get(n)]
    return {'vars': vars_}


def audit_network(args=None):
    """网络体检总入口（纯只读，零副作用）：看清本机网络秩序现状——
    第三方代理进程、本程序端口被占/对局域网暴露、崩溃残留系统代理、
    默认路由与 TUN、DNS 出口、PAC、环境变量代理。不改任何状态。"""
    degraded = []
    foreign = _audit_foreign_procs()
    if foreign is None:
        degraded.append('foreign_procs')
        foreign = []
    sockets, holders = _audit_listen()
    if sockets is None:
        degraded.append('listen_sockets')
        sockets, holders = [], {}
    conflicts = []
    for p in _AUDIT_OWN_PORTS:
        for cmd, pid in (holders or {}).get(p, []):
            if RUNTIME_DIR.lower() not in cmd.lower():
                conflicts.append({'port': p, 'holderCommand': cmd, 'holderPid': pid})
    stale, pac = _audit_stale_and_pac(holders)
    lan_open = [{'command': s['command'], 'pid': s['pid'], 'port': s['port']}
                for s in sockets if s['lanExposed'] and s['port'] in _AUDIT_OWN_PORTS]
    route = _audit_route()
    # P1-2：账本视角也是"秩序"的一部分——未结账本必须出现在体检结论里
    open_ledger = None
    try:
        open_ledger = ledger_open_session()
    except Exception as e:
        print(f'[ledger] WARN 体检读取账本失败: {e}', file=sys.stderr)
    summary_bits = []
    if foreign:
        summary_bits.append(f'{len(foreign)} 个第三方代理进程')
    if conflicts:
        summary_bits.append(f'{len(conflicts)} 个本程序端口被占')
    if stale['detected']:
        summary_bits.append('系统代理残留指向死端口')
    if pac == 'yes':
        summary_bits.append('PAC 自动代理已启用')
    if lan_open:
        summary_bits.append(f'{len(lan_open)} 个本程序端口对局域网暴露')
    if route.get('tunInterface'):
        summary_bits.append(f"默认路由走 {route['interface']}")
    if open_ledger:
        summary_bits.append(f"存在未结接管账本（{open_ledger.get('reason')}）")
    summary = '、'.join(summary_bits) if summary_bits else '未发现混乱源'
    if degraded:
        summary += f'（{len(degraded)} 项采集降级）'
    return {
        'ts': int(time.time()),
        'foreignProcs': foreign,
        'portConflicts': conflicts,
        'ownPortLanExposed': lan_open,
        'staleProxy': stale,
        'openLedger': open_ledger,
        'route': route,
        'dns': _audit_dns(),
        'pacEnabled': pac,
        'envProxy': _audit_env(),
        'summary': summary,
        'degraded': degraded,
    }


def doctor():
    """一键自检：分流引擎、鉴权、fallback 组、节点健康。智能体排查问题的第一入口。"""
    report = {}
    cfg = read_config()
    # 1. 配置完整性
    report['config'] = {
        'nodes': len(cfg.get('nodes', [])),
        'selectedNode': cfg.get('selectedNode'),
        'domainRules': len(cfg.get('domainRules', [])),
        'apiSecret': bool(cfg.get('apiSecret')),
    }
    # 2. 进程与 API 鉴权
    running = mihomo_running()
    report['process'] = {'running': running}
    if running:
        try:
            req = urllib.request.Request(API + '/version')
            _add_auth(req)
            v = json.loads(_OPENER.open(req, timeout=5).read())
            report['auth'] = f"OK (mihomo {v.get('version', '?')})"
        except Exception as e:
            report['auth'] = f'FAIL ({str(e)[:60]})'
        # 3. 生效配置里的 PROXY 组必须是 fallback（防回退成 select 丢掉故障转移）
        try:
            with open(RUNTIME_DIR + '/mihomo.yaml') as f:
                text = f.read()
            ok = '- name: PROXY\n    type: fallback' in text
            report['failover_group'] = 'OK (PROXY=fallback)' if ok else 'FAIL (PROXY 不是 fallback！)'
            report['secret_line'] = 'OK' if re.search(r'^secret:', text, re.M) else 'MISSING'
        except Exception as e:
            report['failover_group'] = f'FAIL ({str(e)[:60]})'
        # 4. 节点健康
        try:
            report['nodes_health'] = node_health().get('nodes')
        except Exception as e:
            report['nodes_health'] = str(e)[:80]
    else:
        report['auth'] = 'SKIP (未运行)'
        report['failover_group'] = 'SKIP'
        report['nodes_health'] = 'SKIP'
    # 5. 三层漏斗顺序抽检（读配置生成逻辑，不依赖运行态）
    rules_text = None
    try:
        conf_path = RUNTIME_DIR + '/mihomo.yaml'
        with open(conf_path) as f:
            rules_text = f.read()
        order_ok = (rules_text.find('IP-CIDR,') != -1 or True)
        idx_geo = rules_text.find('GEOSITE,cn,DIRECT')
        idx_match = rules_text.find('MATCH,DIRECT')
        report['funnel_order'] = 'OK (GEOSITE 在 MATCH 兜底之前)' if 0 < idx_geo < idx_match else 'FAIL'
    except Exception as e:
        report['funnel_order'] = f'SKIP ({str(e)[:50]})'
    return report


def check_update(args):
    """检查更新（只读，不下载）。按 config.updateChannel 选端点拉 latest.json，
    与编译进 App 的版本号（src-tauri/tauri.conf.json）比较。
    github 通道经节点代理端口访问（GitHub 直连不通）；local 通道直连本机 7878。"""
    cfg = read_config()
    if 'error' in cfg:
        return cfg
    channel = cfg.get('updateChannel') or 'github'
    if channel not in ('local', 'github'):
        channel = 'github'
    endpoint = ('http://127.0.0.1:7878/latest.json' if channel == 'local' else
                'https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json')
    # 当前版本：以源码 tauri.conf.json 为准（与本机开发构建一致）
    try:
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        with open(os.path.join(root, 'src-tauri', 'tauri.conf.json')) as f:
            current = json.load(f)['version']
    except Exception as e:
        return {'error': f'读取当前版本失败: {e}'}
    # 拉远端 feed：github 走节点代理（被墙），local 直连
    try:
        if channel == 'local':
            feed = json.loads(_OPENER.open(endpoint, timeout=5).read())
        else:
            proxy_handler = urllib.request.ProxyHandler(
                {'http': f'http://127.0.0.1:{PROXY_PORT}', 'https': f'http://127.0.0.1:{PROXY_PORT}'})
            opener = urllib.request.build_opener(proxy_handler)
            feed = json.loads(opener.open(endpoint, timeout=15).read())
    except Exception as e:
        hint = ('（本地通道需先 cd scripts/updater-feed && python3 -m http.server 7878 --bind 127.0.0.1）'
                if channel == 'local' else '（GitHub 需要节点代理可用，确认代理已启动）')
        return {'error': f'检查更新失败: {e} {hint}', 'channel': channel, 'endpoint': endpoint}
    latest = feed.get('version', '')
    def _v(s):
        try:
            return tuple(int(x) for x in str(s).split('.'))
        except Exception:
            return (0,)
    return {'currentVersion': current, 'latestVersion': latest,
            'available': _v(latest) > _v(current),
            'channel': channel, 'endpoint': endpoint,
            'notes': feed.get('notes')}


# ── 更新验签（与 App 内置 updater 同一把 minisign ed25519 公钥）────────────────
# 私钥 ~/.tauri/magic-agent.key 绝不入库；公钥 32 字节 ed25519 可公开。
# 签名格式（minisign-verify 0.2.5）：文件 blake2b-512 预哈希后做 Ed25519。
_MAGIC_AGENT_PUB32_HEX = '503929e6ddaa360d140ee72797fedfcb875849527052382be605b87af79518fc'
_MAGIC_AGENT_KEYID_HEX = '74202349fc8d43f3'
_VERIFY_PYTHON = '/usr/local/bin/python3'  # 主进程只依赖标准库；验签子进程带 cryptography

_VERIFY_SCRIPT = r'''
import base64, sys, hashlib
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
tar_path, sig_text, pub32_hex = sys.argv[1], sys.argv[2], sys.argv[3]
data = open(tar_path, 'rb').read()
inner = base64.b64decode(sig_text.strip()).decode()
lines = inner.splitlines()
block = base64.b64decode(lines[1])
keyid, sig64 = block[2:10].hex(), block[10:74]
h = hashlib.blake2b(data, digest_size=64).digest()
Ed25519PublicKey.from_public_bytes(bytes.fromhex(pub32_hex)).verify(sig64, h)
print(keyid)
'''


def apply_update(args):
    """通过 MCP 触发 App 自更新（智能体可控的升级手段）。
    流程：拉 feed → 下载 darwin-aarch64 包与签名 → Blake2b-512+Ed25519 验签
    （硬门：验签不过绝不安装）→ 停运行中的旧 App → 解压覆盖 /Applications。
    args 可带 {"channel": "github"|"local"} 覆盖当前通道。"""
    import tempfile
    cfg = read_config()
    if 'error' in cfg:
        return cfg
    channel = _str_arg(args, 'channel') or cfg.get('updateChannel') or 'github'
    if channel not in ('local', 'github'):
        channel = 'github'
    feed_url = ('http://127.0.0.1:7878/latest.json' if channel == 'local' else
                'https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json')
    try:
        if channel == 'local':
            feed = json.loads(_OPENER.open(feed_url, timeout=5).read())
        else:
            proxy_handler = urllib.request.ProxyHandler(
                {'http': f'http://127.0.0.1:{PROXY_PORT}', 'https': f'http://127.0.0.1:{PROXY_PORT}'})
            feed = json.loads(urllib.request.build_opener(proxy_handler).open(feed_url, timeout=20).read())
    except Exception as e:
        hint = '（本地通道需先起 7878 测试源）' if channel == 'local' else '（github 通道需节点代理可用）'
        return {'error': f'拉取更新信息失败: {e} {hint}'}
    machine = os.uname().machine
    platform = 'darwin-aarch64' if machine in ('arm64', 'aarch64') else f'darwin-{machine}'
    plat = feed.get('platforms', {}).get(platform)
    if not plat or not plat.get('url') or not plat.get('signature'):
        return {'error': f'feed 缺少 {platform} 平台条目', 'feedVersion': feed.get('version')}
    pkg_url = plat['url']
    if channel == 'local':
        if not pkg_url.startswith('http://127.0.0.1:7878/'):
            return {'error': f'local 通道包 URL 不在 loopback: {pkg_url}'}
    elif not pkg_url.startswith('https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/download/'):
        return {'error': f'github 通道包 URL 不在允许域名: {pkg_url}'}
    tmpdir = tempfile.mkdtemp(prefix='magic-agent-update-')
    tar_path = os.path.join(tmpdir, 'update.tar.gz')
    try:
        if channel == 'local':
            with open(tar_path, 'wb') as f:
                f.write(_OPENER.open(pkg_url, timeout=180).read())
        else:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler(
                {'http': f'http://127.0.0.1:{PROXY_PORT}', 'https': f'http://127.0.0.1:{PROXY_PORT}'}))
            with opener.open(pkg_url, timeout=180) as r, open(tar_path, 'wb') as f:
                shutil.copyfileobj(r, f)
    except Exception as e:
        return {'error': f'下载更新包失败: {e}'}
    try:
        out = subprocess.run([_VERIFY_PYTHON, '-c', _VERIFY_SCRIPT, tar_path,
                              plat['signature'], _MAGIC_AGENT_PUB32_HEX],
                             capture_output=True, text=True, timeout=60)
        if out.returncode != 0:
            return {'error': f'验签失败，已拒绝安装: {(out.stderr or out.stdout).strip()}'}
        if out.stdout.strip() != _MAGIC_AGENT_KEYID_HEX:
            return {'error': f"签名 keyid 不匹配: {out.stdout.strip()}，拒绝安装"}
    except FileNotFoundError:
        return {'error': f'验签解释器不存在: {_VERIFY_PYTHON}（需 cryptography 库）'}
    extract_dir = os.path.join(tmpdir, 'extract')
    os.makedirs(extract_dir, exist_ok=True)
    subprocess.run(['tar', '-xzf', tar_path, '-C', extract_dir], check=True)
    app_path = None
    for root, dirs, _ in os.walk(extract_dir):
        for d in dirs:
            if d.endswith('.app'):
                app_path = os.path.join(root, d)
                break
        if app_path:
            break
    if not app_path:
        return {'error': '压缩包内未找到 .app'}
    app_name = os.path.basename(app_path)
    dest = os.path.join('/Applications', app_name)
    try:
        subprocess.run(['pkill', '-f', app_name], capture_output=True, timeout=10)
        time.sleep(1)
    except Exception:
        pass
    if os.path.exists(dest):
        shutil.rmtree(dest, ignore_errors=True)
    shutil.move(app_path, dest)
    return {'installed': True, 'version': feed.get('version'), 'appPath': dest,
            'channel': channel, 'note': '已安装到 /Applications，请重新打开 App'}


def check_network():
    results = {}
    # baidu 直连测试
    try:
        req = urllib.request.Request('https://www.baidu.com', method='HEAD')
        _OPENER.open(req, timeout=8)
        results['baidu_direct'] = 'OK'
    except Exception as e:
        results['baidu_direct'] = f'FAIL ({e})'
    # google 走代理测试：用 mihomo API 对当前选中节点做端到端延迟探测。
    # 不能用 curl -x 7891 测——按三层漏斗设计，不在进程表的程序命中 MATCH,DIRECT
    # 直连出局（且 mihomo 对 CONNECT 乐观应答会造成假阳性），此路永远测不了代理。
    # API 延迟探测的流量从 mihomo 自身经节点隧道出站，才是真实的"代理可用"信号。
    try:
        cfg = read_config()
        sel = cfg.get('selectedNode')
        if not sel:
            results['google_proxy'] = 'FAIL (未选择节点)'
        elif not mihomo_running():
            results['google_proxy'] = 'FAIL (代理未运行)'
        else:
            q = urllib.parse.quote(sel, safe='')
            url = urllib.parse.quote('https://www.gstatic.com/generate_204', safe='')
            u = f'http://127.0.0.1:19091/proxies/{q}/delay?timeout=8000&url={url}'
            req = urllib.request.Request(u)
            _add_auth(req)
            r = json.loads(_OPENER.open(req, timeout=12).read())
            results['google_proxy'] = f"OK (经节点 {sel}，延迟 {r.get('delay')}ms)"
    except Exception as e:
        results['google_proxy'] = f'FAIL ({e})'
    return results


TOOLS = [
    {'name': 'status', 'description': '查看网络管理当前是否在运行、当前选中节点、系统代理状态、节点数量。不确定代理状态时先调这个'},
    {'name': 'start_proxy', 'description': '启动代理并接管流量入口（系统代理指向 127.0.0.1:7891，需管理员授权）。接管后所有走系统代理的 App 流量进入分流引擎：按你的软件规则/域名规则决定直连或走哪个节点。'},
    {'name': 'stop_proxy', 'description': '停止代理'},
    {'name': 'list_nodes', 'description': '列出代理节点'},
    {'name': 'switch_node', 'description': '切换当前节点'},
    {'name': 'list_apps', 'description': '列出软件分流配置'},
    {'name': 'set_app_mode', 'description': '设置某 App 的代理模式（proxy/direct）'},
    {'name': 'check_network', 'description': '测试国内外网站连通性'},
    {'name': 'check_update', 'description': '检查 App 是否有新版本（只检查，不下载不安装）。按当前更新通道（local=本机 7878 测试源 / github=正式发布源）拉取版本信息并与当前版本比较。安装更新请让用户在 App 设置页操作'},
    {'name': 'apply_update', 'description': '通过 MCP 直接升级 App（智能体可控的安装手段）：按当前通道拉取最新包 → Blake2b-512+Ed25519 验签（同一把发布公钥，验签不过绝不安装）→ 停运行中的旧 App → 覆盖安装到 /Applications。可用 {"channel": "local"|"github"} 覆盖通道。升级完成后用户重新打开 App 即可。'},
    {'name': 'list_domain_rules', 'description': '列出域名分流规则（哪些域名走代理/直连）'},
    {'name': 'add_domain_rule', 'description': '添加或更新域名分流规则，target 支持 proxy（走代理）、direct（直连）或节点名（走指定节点），建议带 reason 注明服务于哪个密钥/软件。如 {"domain":"openai.com","target":"示例节点","reason":"WorkBuddy 的 OpenAI 密钥"}'},
    {'name': 'remove_domain_rule', 'description': '删除域名分流规则，如 {"domain":"github.com"}'},
    {'name': 'fetch_subscription', 'description': '从订阅 URL 拉取 VLESS 节点，如 {"url":"https://..."}'},
    {'name': 'test_node_delay', 'description': '测试节点延迟（通过 mihomo API），如 {"name":"示例节点"}'},
    {'name': 'list_free_models', 'description': '列出 OpenRouter 免费模型台账（读 docs/free_models.json，按厂商分组）。过期时用 scripts/openrouter_free_models.py 刷新。可选 {"vendor":"google"} 按厂商过滤'},
    {'name': 'list_connections', 'description': '实时连接快照：当前每条流量走的主机、命中规则、出口节点、进程、上下行字节，按下载量排序。诊断"某软件流量到底走了哪"用这个，不要裸 curl。可选 {"limit":50}'},
    {'name': 'node_health', 'description': '全部节点健康探测（延迟）+ fallback 组状态（当前实际使用哪个节点）。判断节点是否挂了/故障转移是否生效用这个'},
    {'name': 'download_proxy', 'description': '【下载/访问网络前先调这个】拿到网络管理的两条路入口并自己决定走哪条。返回节点代理 http://127.0.0.1:7893（访问国外 GitHub/Google/HuggingFace/国外 API 用这条）、本机直连 http://127.0.0.1:7892（访问国内百度/腾讯/阿里用这条）。网络管理不替你自动分流，决策权在你：目标在国外走 7893，国内走 7892。可选 {"url":"https://..."} 会附带该域名的国内/国外建议（仅建议，最终你拍板）'},
    {'name': 'doctor', 'description': '一键自检（排查任何"代理好像不对劲"先跑这个）：配置完整性、进程与 API 鉴权、fallback 故障转移组、secret、规则顺序、节点健康，返回各检查项 OK/FAIL'},
    {'name': 'audit_network', 'description': '【网络体检，纯只读零副作用】看清本机网络秩序现状：第三方代理进程、本程序端口被谁占/是否对局域网暴露、崩溃残留的系统代理死端口、默认路由与 TUN 接口、DNS 出口、PAC、环境变量代理。返回 summary 一句话结论 + degraded 列出采集降级的维度。开代理前或怀疑"网络被人接管"时先跑这个；改状态请走 start_proxy/stop_proxy。'},
    {'name': 'remove_privileged_helper', 'description': 'v0.4.1 安全整改：卸载旧特权控制器（弹一次管理员授权）：删除 sudoers 免密 root 白名单与 /usr/local/lib/magic-agent 下的 root 控制脚本。内核已用户态化（TUN 关闭、全普通端口），root 通道不再需要；卸载即封死系统后门级风险。旧版升级用户建议执行一次'},
    {'name': 'probe_route', 'description': '【拿不准走哪条路时先调这个】实测「本机直连(7892) vs 节点代理(7893)」到同一个目标 url 的真实延迟 + 下载吞吐，返回对比数据和明确结论。不再凭国内/国外规则猜，而是实测路况后拍板。如 {"url":"https://huggingface.co"}，可选 {"timeout":12,"read_bytes":262144}'},
    {'name': 'server_metrics', 'description': '云服务器一键探针：远程采集当前激活云服务器的 CPU/内存/磁盘/带宽/负载/在线时长，返回结构化数据。用于远程看清服务器状态（而不是盲敲命令）。未配置服务器时会返回配置指引'},
    {'name': 'list_servers', 'description': '列出已配置的云服务器（SSH），标出当前激活的一台。server_metrics/ssh_exec 都作用于「当前激活」服务器，想确认或更换作用目标时先用这个看列表'},
    {'name': 'select_server', 'description': '切换当前激活的云服务器（server_metrics/ssh_exec 的作用目标随之改变），如 {"id":"ssh-1.2.3.4"}。可用 id 从 list_servers 获取'},
    {'name': 'set_system_proxy', 'description': '开/关 macOS 系统代理（指向 127.0.0.1:7891），如 {"enabled":true}。接管生效期间系统代理就是流量入口，请勿手动关闭（关了内核就空转，界面会提示漂移）。单独调整时才用。关系统代理前请确认代理内核在运行，否则用户会断网'},
    {'name': 'ssh_exec', 'description': '在当前激活的云服务器上非交互式执行一条命令并返回 (stdout, stderr, exit_code)。用于远程管理服务器（装软件、看日志、跑脚本）。密码从 macOS Keychain 读取，不落盘。如 {"command":"df -h","timeout_secs":15}'},
    {'name': 'restore_network', 'description': '【一键还原】停止代理内核，并按接管账本把系统代理逐服务回放到接管前的原值、结账。是 start_proxy 接管的逆操作；账本中"接管时关闭的第三方进程"不可逆，返回里如实列出。无未结账本时只停内核+关系统代理回直连并说明局限。'},
    {'name': 'reapply_takeover', 'description': '【漂移归位】接管期间系统代理被外部改动（status 的 drift 字段非空）时，重新逐服务设回 127.0.0.1:7891 并回读对账。前提：内核在跑且有未结接管账本，否则拒绝改写。'},
    {'name': 'guide', 'description': '返回网络管理的完整使用手册（是什么、何时用、两条路怎么选、各工具配合关系）。首次接触网络管理、或不确定该怎么用它时，先调这个了解全貌'},
]


def _tool_schema(properties=None, required=None):
    schema = {
        'type': 'object',
        'properties': properties or {},
        'additionalProperties': False,
    }
    if required:
        schema['required'] = list(required)
    return schema


_TOOL_SCHEMAS = {
    'status': _tool_schema({'verify': {'type': 'boolean',
                                       'description': 'true 时额外做逐服务 networksetup 读回对账（较慢），返回 services/mismatched/allOk'}}),
    'start_proxy': _tool_schema(),
    'stop_proxy': _tool_schema(),
    'list_nodes': _tool_schema(),
    'switch_node': _tool_schema({'name': {'type': 'string'}}, ['name']),
    'list_apps': _tool_schema(),
    'set_app_mode': _tool_schema({
        'id': {'type': 'string'},
        'mode': {'type': 'string', 'enum': ['proxy', 'direct']},
        'node': {'type': ['string', 'null']},
    }, ['id', 'mode']),
    'check_network': _tool_schema(),
    'check_update': _tool_schema(),
    'apply_update': _tool_schema({'channel': {'type': 'string', 'description': 'github 或 local；缺省读 App 当前通道'}}),
    'list_domain_rules': _tool_schema(),
    'add_domain_rule': _tool_schema({
        'domain': {'type': 'string'},
        'target': {'type': 'string'},
        'reason': {'type': 'string'},
    }, ['domain', 'target']),
    'remove_domain_rule': _tool_schema({'domain': {'type': 'string'}}, ['domain']),
    'fetch_subscription': _tool_schema({'url': {'type': 'string'}}, ['url']),
    'test_node_delay': _tool_schema({'name': {'type': 'string'}}, ['name']),
    'list_free_models': _tool_schema({'vendor': {'type': 'string'}}),
    'list_connections': _tool_schema({'limit': {'type': 'integer'}}),
    'node_health': _tool_schema(),
    'download_proxy': _tool_schema({'url': {'type': 'string'}}),
    'doctor': _tool_schema(),
    'audit_network': _tool_schema(),
    'remove_privileged_helper': _tool_schema(),
    'probe_route': _tool_schema({
        'url': {'type': 'string'},
        'timeout': {'type': 'integer'},
        'read_bytes': {'type': 'integer'},
    }, ['url']),
    'server_metrics': _tool_schema(),
    'list_servers': _tool_schema(),
    'select_server': _tool_schema({'id': {'type': 'string'}}, ['id']),
    'set_system_proxy': _tool_schema({'enabled': {'type': 'boolean'}}, ['enabled']),
    'restore_network': _tool_schema(),
    'reapply_takeover': _tool_schema(),
    'ssh_exec': _tool_schema({
        'command': {'type': 'string'},
        'timeout_secs': {'type': 'integer'},
    }, ['command']),
    'guide': _tool_schema(),
}

for _tool in TOOLS:
    _tool['inputSchema'] = _TOOL_SCHEMAS[_tool['name']]


def call_tool(name, args):
    # 参数加严：多传的未知参数直接报错，不静默吞掉。
    # 教训：曾误传 policy='direct'（正确参数是 target），旧逻辑静默忽略并存成
    # 默认值 proxy，规则方向整个反了还不报错。
    schema = _TOOL_SCHEMAS.get(name)
    if schema is not None and isinstance(args, dict):
        allowed = set((schema.get('properties') or {}).keys())
        unknown = sorted(set(args.keys()) - allowed)
        if unknown:
            return {'error': f'未知参数: {unknown}。{name} 支持的参数: {sorted(allowed) or "（无参数）"}'}
    if name == 'status':
        running = mihomo_running()
        cfg = read_config()
        selected = cfg.get('selectedNode', '') if 'error' not in cfg else '?'
        # systemProxy 用【真实】系统状态（scutil），而非 config 里的意图字段，
        # 否则 App 重启后 config 仍是 true 但实际代理已关，会误导调用方。
        result = {'running': running, 'selectedNode': selected,
                  'systemProxy': system_proxy_enabled(),
                  # P1-3：如实暴露接管状态——未结账本（异常退出残留）与确认模式开关
                  'openLedger': ledger_open_session() is not None,
                  'confirmTakeover': bool(cfg.get('confirmTakeover')) if 'error' not in cfg else False,
                  'nodes': len(cfg.get('nodes', [])) if 'error' not in cfg else 0}
        # P1-4 漂移巡检：仅接管生效中（有未结账本且内核在跑）才抽验系统代理现值，
        # 返回被外部改离接管态的服务清单（None=无漂移/未接管）。probe_drift 自带
        # 守卫，未接管场景零额外开销。发现漂移 → AI 可调 reapply_takeover 归位。
        drift = probe_drift()
        if drift:
            result['drift'] = {'services': drift}
        # P0-1：verify=true 时做逐服务读回对账（默认关，避免轮询放大 networksetup 开销）。
        # 达标口径：系统代理开着 → 应精确指向 127.0.0.1:7891；关着 → 应全部 Enabled:No。
        # 与 set 路径同用 verify_system_proxy(services, want_port, expect_on)。
        if (args or {}).get('verify'):
            services = list_network_services()
            enabled = result['systemProxy']
            states = verify_system_proxy(services, 7891, enabled)
            mismatched = [st['service'] for st in states
                          if not (st['httpOn'] and st['httpsOn'] and st['socksOn']) or st['errors']]
            result['verify'] = {'allOk': bool(states) and not mismatched,
                                'mismatched': mismatched, 'services': states}
        return result
    elif name == 'start_proxy':
        if mihomo_running():
            # P1-A（与 Rust start_proxy 同语义）：接管生效中达标态恒为
            # "系统代理开着指向本程序端口"——TUN 不承载流量（auto-route=false
            # 红线冻结），系统代理是引擎唯一流量入口，入口关着 = 内核空转。
            # 旧逻辑按 systemProxy=false 反而"关掉多余系统代理"，把接管态归回
            # 空转态（用户看不到任何真实连接的根因），已废弃。
            actual = system_proxy_enabled()
            cfg0 = read_config()
            if not actual:
                try:
                    set_system_proxy(True)
                    if 'error' not in cfg0 and not cfg0.get('systemProxy'):
                        cfg0['systemProxy'] = True
                    if 'error' not in cfg0:
                        cfg0['userStopped'] = False  # P2-1：启动 = 期望运行，清停用仲裁标记
                    if 'error' not in cfg0:
                        try:
                            write_config(cfg0)
                        except Exception as e:
                            print(f'[start_proxy] WARN 意图落盘失败: {e}', file=sys.stderr)
                    return {'ok': True, 'message': '内核已在运行，已补开系统代理（接管入口）'}
                except Exception as e:
                    return {'ok': False, 'message': f'内核在运行但开系统代理失败: {e}'}
            # 入口已开：意图对齐落盘（纠正历史 false，与 Rust 侧一致）；P2-1 清停用标记
            if 'error' not in cfg0 and not cfg0.get('systemProxy'):
                cfg0['systemProxy'] = True
            if 'error' not in cfg0:
                cfg0['userStopped'] = False
            if 'error' not in cfg0:
                try:
                    write_config(cfg0)
                except Exception as e:
                    print(f'[start_proxy] WARN 意图落盘失败: {e}', file=sys.stderr)
            return {'ok': True, 'message': '代理已在运行'}
        # P1-2 显式接管入口：内核即将启动+可能改系统代理 = 改变系统状态，
        # 必须先记账（快照逐服务原值）再动手。失败路径结账，不留幽灵账本。
        try:
            ledger_begin('mcp:start_proxy')
        except Exception as e:
            print(f'[ledger] WARN begin 失败（本次接管不留账）: {e}', file=sys.stderr)
        try:
            regenerate_config()
            pid = start_mihomo()
        except Exception as e:
            try:
                ledger_settle(f'mcp:start_proxy 失败：{e}')
            except Exception as le:
                print(f'[ledger] WARN 启动失败后结账失败: {le}', file=sys.stderr)
            raise
        # P1-A（与 Rust start_proxy 同语义）：接管必开入口——内核起来后，
        # 系统代理（唯一流量入口）必须打开并逐服务对账达标，不看历史意图。
        # 不达标 = 半套秩序——宁可不启：停内核 + 按账本回放原值 + 结账 + 如实报错。
        # 已知不可逆项（cleanup 杀的第三方进程）绝不假装恢复。
        restore_note = ''
        try:
            result = set_system_proxy(True)
            if not result.get('allOk'):
                mismatch = result.get('mismatched') or []
                raise RuntimeError(
                    f"系统代理设置未全部达标：{'、'.join(mismatch) if mismatch else '原因见对账明细'}")
        except Exception as e:
            stop_mihomo()
            try:
                errs, had_ledger, had_procs = ledger_rollback(f'mcp:start_proxy 对账不达标，自动回滚：{e}')
                parts = ['内核已停止']
                parts.append('系统代理已按账本还原为接管前原值' if had_ledger
                             else '账本无未结账目，系统代理未能还原原值（请手动检查网络设置）')
                if errs:
                    parts.append('还原存在失败项：' + '；'.join(errs))
                if had_procs:
                    parts.append('接管时关闭的第三方代理进程不会自动复活')
                return {'ok': False, 'message': f'{e}。已自动回滚：' + '；'.join(parts), 'rolledBack': True}
            except Exception as le:
                restore_note = f'；且自动回滚异常：{le}'
            return {'ok': False, 'message': f'代理启动失败：{e}{restore_note}', 'rolledBack': False}
        # 入口开成功并达标：意图对齐落盘（纠正历史 false）
        cfg2 = read_config()
        if 'error' not in cfg2 and not cfg2.get('systemProxy'):
            cfg2['systemProxy'] = True
        if 'error' not in cfg2:
            cfg2['userStopped'] = False  # P2-1：启动 = 期望运行，清停用仲裁标记
        if 'error' not in cfg2:
            try:
                write_config(cfg2)
            except Exception as e:
                print(f'[start_proxy] WARN 意图落盘失败（不影响本次接管）: {e}', file=sys.stderr)
        return {'ok': True, 'pid': pid, 'message': '代理已启动，系统代理已开并通过对账（接管入口就绪）'}
    elif name == 'stop_proxy':
        stop_mihomo()
        # 与 Rust 侧 stop_proxy 保持一致：停内核后必须关系统代理，
        # 否则系统代理仍指向已关闭的 7891，导致用户断网。
        try:
            set_system_proxy(False)
        except Exception as e:
            return {'ok': False, 'message': f'内核已停止，但关系统代理失败: {e}'}
        # P2-1（v0.4.1 已修）：落盘 userStopped=true——App 看门狗读到此标记
        # 绝不 30 秒内复活内核（旧行为 PID 34747→40591 实锤"我停了它又活了"）。
        cfg = read_config()
        if 'error' not in cfg:
            cfg['userStopped'] = True
            try:
                write_config(cfg)
            except Exception as e:
                print(f'[stop_proxy] WARN userStopped 落盘失败: {e}', file=sys.stderr)
        # P1-2：正常停止 = 接管结束，结账（只结账不回滚，与 Rust 同语义）。
        try:
            ledger_settle('mcp:stop_proxy')
        except Exception as e:
            print(f'[ledger] WARN settle 失败: {e}', file=sys.stderr)
        return {'ok': True, 'message': '代理已停止（已落盘 userStopped 仲裁标记，App 看门狗不会复活内核）'}
    elif name == 'list_nodes':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        selected = cfg.get('selectedNode')
        return [{'name': n['name'], 'server': n['server'], 'port': n['port'],
                 'current': n['name'] == selected} for n in cfg.get('nodes', [])]
    elif name == 'switch_node':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        node_name = _str_arg(args, 'name')
        if not any(n['name'] == node_name for n in cfg.get('nodes', [])):
            return {'error': f'节点不存在: {node_name}'}
        cfg['selectedNode'] = node_name
        write_config(cfg)
        # 通过 mihomo API 直接切换 PROXY 组的选中节点，不重启、不弹授权框
        if mihomo_running():
            try:
                body = json.dumps({'name': node_name}).encode()
                req = urllib.request.Request('http://127.0.0.1:19091/proxies/PROXY', data=body, method='PUT')
                req.add_header('Content-Type', 'application/json')
                _add_auth(req)
                _OPENER.open(req, timeout=10)
            except Exception as e:
                return {'ok': False, 'message': f'配置已保存但切换失败: {e}'}
        return {'ok': True, 'message': f'已切换到节点 {node_name}'}
    elif name == 'list_apps':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        return [{'id': a['id'], 'mode': a.get('mode'), 'confirmed': a.get('confirmed')}
                for a in cfg.get('apps', [])]
    elif name == 'set_app_mode':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        app_id = _str_arg(args, 'id')
        mode = _str_arg(args, 'mode', 'direct')
        if not app_id:
            return {'error': '缺少 id 参数（要设置哪个 App）'}
        if mode not in ('proxy', 'direct'):
            return {'error': 'mode must be proxy or direct'}
        found = False
        for a in cfg.get('apps', []):
            if a['id'] == app_id:
                a['mode'] = mode
                a['confirmed'] = True
                found = True
        if not found:
            cfg['apps'].append({'id': app_id, 'mode': mode, 'confirmed': True, 'node': None})
        write_config(cfg)
        return _rule_reload_result(cfg, f'{app_id} -> {mode}')
    elif name == 'check_network':
        return check_network()
    elif name == 'check_update':
        return check_update(args)
    elif name == 'apply_update':
        return apply_update(args)
    elif name == 'list_domain_rules':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        return cfg.get('domainRules', [])
    elif name == 'add_domain_rule':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        domain = _str_arg(args, 'domain')
        target = _str_arg(args, 'target', 'proxy')
        if not domain:
            return {'error': 'domain is required'}
        # 参数校验（P1-5）：域名必须是合法域名/IP，且不含换行/逗号/引号等注入字符。
        # 旧实现不校验：localhost、含换行注入串都会"保存成功"（规则静默失效/注入）。
        if len(domain) > 253 or any(c in domain for c in '\n\r,"\''):
            return {'error': f'domain 含非法字符或过长（收到 {len(domain)} 字符）'}
        if _sanitize_rule_field(domain) != domain:
            return {'error': f'domain 含非法字符: {domain[:80]}（只允许字母数字和 .-_:*/#[]）'}
        if domain.lower() in ('localhost',) or domain.lower().endswith('.localhost'):
            return {'error': 'domain 不能是 localhost（本机回环不经过代理内核，规则无意义）'}
        # target 支持 proxy / direct / 节点名（走指定节点）
        if target not in ('proxy', 'direct'):
            node_names = [n.get('name') for n in cfg.get('nodes', [])]
            if target not in node_names:
                return {'error': f'target 必须是 proxy、direct 或已有节点名，现有节点：{node_names}'}
        rules = cfg.get('domainRules', [])
        reason = _str_arg(args, 'reason')
        for r in rules:
            if r['domain'] == domain:
                r['target'] = target
                if reason:
                    r['reason'] = reason
                break
        else:
            entry = {'domain': domain, 'target': target}
            if reason:
                entry['reason'] = reason
            rules.append(entry)
        cfg['domainRules'] = rules
        write_config(cfg)
        msg = f'域名规则已保存: {domain} -> {target}'
        if reason:
            msg += f'（{reason}）'
        return _rule_reload_result(cfg, msg)
    elif name == 'remove_domain_rule':
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        domain = _str_arg(args, 'domain')
        rules = cfg.get('domainRules', [])
        cfg['domainRules'] = [r for r in rules if r['domain'] != domain]
        write_config(cfg)
        return _rule_reload_result(cfg, f'域名规则已删除: {domain}')
    elif name == 'fetch_subscription':
        url = _str_arg(args, 'url')
        if not url:
            return {'error': 'url is required'}
        nodes = fetch_subscription_from_url(url)
        if isinstance(nodes, dict) and 'error' in nodes:
            return nodes
        cfg = read_config()
        if 'error' in cfg:
            return cfg
        existing = cfg.get('nodes', [])
        seen = {n['server'] + ':' + str(n['port']) for n in existing}
        added = 0
        for n in nodes:
            key = n['server'] + ':' + str(n['port'])
            if key not in seen:
                existing.append(n)
                seen.add(key)
                added += 1
        cfg['nodes'] = existing
        if not cfg.get('selectedNode') and existing:
            cfg['selectedNode'] = existing[0]['name']
        write_config(cfg)
        return {'ok': True, 'message': f'订阅拉取成功，新增 {added} 个节点，共 {len(existing)} 个'}
    elif name == 'test_node_delay':
        node_name = _str_arg(args, 'name')
        if not node_name:
            return {'error': 'name is required'}
        try:
            body = json.dumps({'name': node_name}).encode()
            # 组名与规则引用都用 _sanitize_node_name，这里也必须一致，否则延迟测试找不到组
            gname = _sanitize_node_name(str(node_name))
            req = urllib.request.Request('http://127.0.0.1:19091/proxies/NODE-' + urllib.parse.quote(gname) + '/delay?timeout=5000&url=http://www.gstatic.com/generate_204',
                                          data=body, method='GET')
            _add_auth(req)
            r = _OPENER.open(req, timeout=8)
            resp = json.loads(r.read())
            return {'node': node_name, 'delay_ms': resp.get('delay')}
        except urllib.error.HTTPError as e:
            # mihomo API 对不存在的组返回 404；对超时节点返回 500/503。
            # 原始 "HTTP Error 404: Not Found" 对智能体不友好，转成可行动的描述。
            if e.code == 404:
                return {'node': node_name, 'error': f'节点不存在: {node_name}（用 list_nodes 看可用节点）'}
            return {'node': node_name, 'error': f'测延迟失败（HTTP {e.code}）：节点可能不可用'}
        except Exception as e:
            return {'node': node_name, 'error': f'测延迟失败: {e}'}
    elif name == 'list_free_models':
        return list_free_models_impl(args)
    elif name == 'list_connections':
        # 参数健壮化：limit 可能是字符串/null/非法值（智能体传参不可控），
        # 直接 int() 会抛异常。兜底为默认 50，再夹到 1~500 防止超大值拖慢。
        try:
            limit = int((args or {}).get('limit', 50))
        except (TypeError, ValueError):
            limit = 50
        limit = max(1, min(limit, 500))
        try:
            return list_connections(limit)
        except Exception as e:
            if mihomo_running():
                return {'error': f'读取连接失败: {e}'}
            return {'error': '代理未运行'}
    elif name == 'node_health':
        return node_health()
    elif name == 'download_proxy':
        return download_proxy(args)
    elif name == 'doctor':
        return doctor()
    elif name == 'audit_network':
        return audit_network(args)
    elif name == 'remove_privileged_helper':
        return remove_privileged_helper()
    elif name == 'probe_route':
        return probe_route(args)
    elif name == 'server_metrics':
        return server_metrics(args)
    elif name == 'list_servers':
        # 与 Rust AppConfig::active_server 对齐：activeServerId 优先，缺省取首项
        cfg = read_config()
        if 'error' in cfg:
            return {'error': cfg['error']}
        servers = cfg.get('servers', [])
        active_id = cfg.get('activeServerId') or (servers[0].get('id') if servers else None)
        return {'activeServerId': active_id,
                'servers': [{'id': s.get('id'), 'name': s.get('name'),
                             'host': s.get('host'), 'port': s.get('port'),
                             'user': s.get('user'), 'auth': s.get('auth'),
                             'active': s.get('id') == active_id} for s in servers]}
    elif name == 'select_server':
        # 与 Rust 侧 select_ssh_server 行为对齐：切 activeServerId 并同步 ssh* 旧字段
        sid = _str_arg(args, 'id')
        if not sid:
            return {'error': 'id is required（用 list_servers 看可用 id）'}
        cfg = read_config()
        if 'error' in cfg:
            return {'error': cfg['error']}
        target = next((s for s in cfg.get('servers', []) if s.get('id') == sid), None)
        if not target:
            return {'error': f'服务器不存在: {sid}（用 list_servers 看可用 id）'}
        cfg['activeServerId'] = target['id']
        cfg['sshHost'] = target.get('host')
        cfg['sshPort'] = target.get('port', 22)
        cfg['sshUser'] = target.get('user', 'root')
        cfg['sshAuth'] = target.get('auth', 'password')
        cfg['sshPrivateKey'] = target.get('keyPath')
        write_config(cfg)
        return {'ok': True, 'message': f'已切换到服务器 {target.get("name")}（{target.get("host")}）',
                'activeServerId': target['id']}
    elif name == 'set_system_proxy':
        enabled = (args or {}).get('enabled')
        if not isinstance(enabled, bool):
            return {'error': 'enabled 必填（true/false），如 {"enabled":true}'}
        try:
            result = set_system_proxy(enabled)
        except Exception as e:
            return {'ok': False, 'error': str(e)}
        # 持久化意图到 config.systemProxy，与 Rust 侧 set_system_proxy 命令保持一致——
        # 否则 App 重启后按旧意图恢复，实际状态与配置脱节。
        cfg = read_config()
        if 'error' not in cfg:
            cfg['systemProxy'] = enabled
            write_config(cfg)
        # P0-1：透出逐服务对账结果，不再只报"开没开"
        return {'ok': True, 'systemProxy': system_proxy_enabled(),
                'allOk': result.get('allOk'), 'mismatched': result.get('mismatched'),
                'services': result.get('services')}
    elif name == 'restore_network':
        # P1-4 一键还原（与 Rust restore_network 同语义）：
        # 停内核 → 按账本 before 逐服务回放并结账 → 意图落盘 systemProxy=false。
        # 无账本不瞎写：退回"关系统代理回直连"并如实说明局限。
        try:
            stop_mihomo()
        except Exception as e:
            return {'ok': False, 'error': f'停止内核失败：{e}'}
        session = ledger_open_session()
        irreversible = [e.get('key') for e in (session or {}).get('entries', [])
                        if e.get('kind') == 'process']
        try:
            errs, had_ledger, _ = ledger_rollback('mcp:用户一键还原')
        except Exception as e:
            return {'ok': False, 'error': f'还原过程异常：{e}'}
        if had_ledger:
            message = '代理内核已停止；系统代理已按接管前账本逐服务还原'
            if errs:
                message += f'；{len(errs)} 项还原失败：' + '；'.join(errs)
            if irreversible:
                message += f'；{len(irreversible)} 个接管时关闭的第三方进程不会自动复活：' + '、'.join(irreversible)
        else:
            # 无账本 = 没有"接管前原值"可回，唯一诚实的动作是退回直连并说明局限
            notes = []
            try:
                r = set_system_proxy(False)
                if not r.get('allOk'):
                    notes.append('部分服务未能确认关闭：' + '、'.join(r.get('mismatched') or ['见明细']))
            except Exception as e:
                notes.append(f'关闭系统代理失败：{e}')
            message = '无未结接管账本（本机未发生过接管或账本已结），代理内核已停止，系统代理已关闭回直连'
            if notes:
                message += '；' + '；'.join(notes)
            message += '。若接管前的设置并非直连，请手动恢复'
            errs = []
        # 意图落盘：防止 App 启动联动又自动开系统代理覆盖还原结果（与 Rust 同语义）；
        # P2-1：还原 = 明确停用，落盘仲裁标记防 App 看门狗复活内核
        cfg = read_config()
        if 'error' not in cfg:
            cfg['systemProxy'] = False
            cfg['userStopped'] = True
            try:
                write_config(cfg)
            except Exception as e:
                message += f'；注意：还原意图落盘失败（下次 App 启动可能自动开系统代理）：{e}'
        return {'ok': True, 'restored': had_ledger, 'errors': errs,
                'irreversible': irreversible, 'message': message}
    elif name == 'reapply_takeover':
        # P1-4 漂移归位（与 Rust reapply_takeover 同语义）：接管生效中系统代理被
        # 外部改动后，恢复到【本次接管声明的秩序】并对账。前提双检：内核在跑
        # （对死端口设代理=亲手制造断网）、有未结账本（无账=系统代理不归本程序管，拒写）。
        # P1-A：接管生效中的达标态恒为"系统代理开着指向 7891"（TUN 不承载流量，
        # 系统代理是唯一入口），归位一律回开；旧"按意图分方向"会把接管态归回
        # 空转态，已废弃。
        if not mihomo_running():
            return {'ok': False, 'error': '代理内核未在运行，无法归位（请改用 start_proxy 重新接管）'}
        if ledger_open_session() is None:
            return {'ok': False, 'error': '无未结接管账本，系统代理当前不归本程序管辖，拒绝改写'}
        try:
            result = set_system_proxy(True)
        except Exception as e:
            return {'ok': False, 'error': f'归位失败：{e}'}
        return {'ok': True, 'systemProxy': system_proxy_enabled(),
                'allOk': result.get('allOk'), 'mismatched': result.get('mismatched'),
                'services': result.get('services')}
    elif name == 'ssh_exec':
        # 参数健壮化：timeout_secs 可能是字符串/非法值，直接 int() 会抛异常
        try:
            to = int((args or {}).get('timeout_secs', 15))
        except (TypeError, ValueError):
            to = 15
        to = max(1, min(to, 120))
        return ssh_exec(_str_arg(args, 'command'), to)
    elif name == 'guide':
        return {'guide': SERVER_INSTRUCTIONS}
    return {'error': f'unknown tool {name}'}


def list_free_models_impl(args):
    ledger = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                          'docs', 'free_models.json')
    if not os.path.exists(ledger):
        return {'error': '台账不存在，请先运行 scripts/openrouter_free_models.py 刷新'}
    with open(ledger) as f:
        snap = json.load(f)
    vendor = _str_arg(args, 'vendor') or None
    models = [m for m in snap.get('models', [])
              if not vendor or m['vendor'].lower() == vendor.lower()]
    return {
        'updated_at': snap.get('updated_at'),
        'free_count': len(models),
        'total_models': snap.get('total_models'),
        'models': models,
    }


def handle_message(msg):
    """处理单条 JSON-RPC 消息，返回响应 dict（无 id 的通知返回 None）。"""
    method = msg.get('method', '')
    msg_id = msg.get('id')
    if method == 'initialize':
        return {'jsonrpc': '2.0', 'id': msg_id, 'result': {
            'protocolVersion': '2025-06-18',
            'capabilities': {'tools': {}, 'instructions': {}},
            'instructions': SERVER_INSTRUCTIONS,
            'serverInfo': {'name': 'magic-agent', 'version': '0.1.0'}}}
    if method == 'tools/list':
        return {'jsonrpc': '2.0', 'id': msg_id, 'result': {'tools': TOOLS}}
    if method == 'tools/call':
        params = msg.get('params', {})
        tool_name = params.get('name', '')
        # 统一兜底：客户端可能传 "arguments": null（key 存在但值为 None），
        # 此时 params.get('arguments', {}) 会返回 None，导致 call_tool 内 args.get 崩溃。
        tool_args = params.get('arguments')
        if not isinstance(tool_args, dict):
            tool_args = {}
        try:
            result = call_tool(tool_name, tool_args)
        except Exception as e:
            result = {'error': str(e)}
        return {'jsonrpc': '2.0', 'id': msg_id,
                'result': {'content': [{'type': 'text', 'text': json.dumps(result, ensure_ascii=False)}]}}
    if method == 'notifications/initialized':
        return None
    return {'jsonrpc': '2.0', 'id': msg_id,
            'error': {'code': -32601, 'message': 'method not found'}}


def main():
    """入口：默认 stdio；`--http [port]` 启动本地 HTTP 桥接（供 WorkBuddy 等 HTTP MCP 客户端接入）。"""
    if '--http' in sys.argv:
        idx = sys.argv.index('--http')
        port = 19092
        if idx + 1 < len(sys.argv):
            try:
                port = int(sys.argv[idx + 1])
            except ValueError:
                pass
        serve_http(port)
        return
    # MCP stdio JSON-RPC 循环
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        resp = handle_message(msg)
        if resp is not None:
            print(json.dumps(resp, ensure_ascii=False), flush=True)


def create_http_server(port=19092):
    """创建本地 HTTP 桥接服务器；端口可注入，便于测试。"""
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    token = HTTP_BRIDGE_TOKEN or load_http_bridge_token()
    bound_ports = [port]

    class Handler(BaseHTTPRequestHandler):
        server_version = 'magic-agent-extension/0.1'

        def _origin_allowed(self):
            origin = self.headers.get('Origin')
            if origin is None:
                return True
            return origin in (
                f'http://127.0.0.1:{bound_ports[0]}',
                f'http://localhost:{bound_ports[0]}',
                f'http://[::1]:{bound_ports[0]}',
            )

        def _authenticated(self):
            return self.headers.get('Authorization', '') == 'Bearer ' + token

        def _request_authorized(self):
            # 无 Origin = 本机原生客户端（launchd/脚本/CLI）。浏览器请求一定带
            # Origin，必须同时通过来源白名单和 bearer 校验，保留 CSRF 防护。
            origin = self.headers.get('Origin')
            return origin is None or self._authenticated()

        def _send(self, code, body, ctype='application/json'):
            try:
                self.send_response(code)
                self.send_header('Content-Type', ctype)
                self.send_header('Content-Length', str(len(body)))
                origin = self.headers.get('Origin')
                if origin and self._origin_allowed():
                    self.send_header('Access-Control-Allow-Origin', origin)
                    self.send_header('Vary', 'Origin')
                    self.send_header('Access-Control-Allow-Headers', 'Content-Type, Authorization')
                    self.send_header('Access-Control-Allow-Methods', 'GET, POST, OPTIONS')
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def _read_body(self):
            try:
                length = int(self.headers.get('Content-Length', '0') or '0')
            except ValueError:
                return b''
            return self.rfile.read(length) if length > 0 else b''

        def do_OPTIONS(self):
            if not self._origin_allowed():
                self._send(403, b'forbidden origin', ctype='text/plain')
                return
            self._send(204, b'', ctype='text/plain')

        def do_GET(self):
            if not self._request_authorized():
                self._send(401, b'bearer token required', ctype='text/plain')
                return
            if self.path.rstrip('/') in ('', '/health', '/extension'):
                self._send(200, json.dumps({'ok': True, 'server': 'magic-agent'}).encode())
            else:
                self._send(404, b'not found', ctype='text/plain')

        def do_POST(self):
            if not self._origin_allowed():
                self._send(403, b'forbidden origin', ctype='text/plain')
                return
            if not self._request_authorized():
                self._send(401, b'bearer token required', ctype='text/plain')
                return
            if self.path.rstrip('/') not in ('/extension', '/rpc'):
                self._send(404, b'not found', ctype='text/plain')
                return
            raw = self._read_body()
            try:
                msg = json.loads(raw.decode('utf-8'))
            except (json.JSONDecodeError, UnicodeDecodeError):
                self._send(400, b'invalid json', ctype='text/plain')
                return
            resp = handle_message(msg)
            if resp is None:
                self._send(202, b'', ctype='text/plain')
                return
            self._send(200, json.dumps(resp, ensure_ascii=False).encode())

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(('127.0.0.1', port), Handler)
    bound_ports[0] = server.server_address[1]
    server.daemon_threads = True
    return server


def serve_http(port=19092):
    """本地 HTTP 桥接：把 HTTP 请求转成 JSON-RPC 调 handle_message。"""
    try:
        server = create_http_server(port)
    except OSError as e:
        print(f'[magic-agent-extension] 无法绑定 127.0.0.1:{port}: {e}', file=sys.stderr)
        sys.exit(1)
    print(f'[magic-agent-extension] HTTP bridge listening on http://127.0.0.1:{port}/extension', file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == '__main__':
    main()
