#!/bin/bash
# ─────────────────────────────────────────────────────────────────────────────
# PAC 白名单分流 + 失败自动直连（fail-open）原型
#
# 设计目标：**不再劫持整个网络**
#   1. 默认 DIRECT：不在白名单里的域名，浏览器/App 物理直连，全程不经过 mihomo。
#   2. 白名单命中：返回 "PROXY 127.0.0.1:7891; DIRECT"——PAC 标准的 fallback 链。
#      内核没起来 / 端口没人听 → 客户端自动退到 DIRECT，表现为「变慢」而不是「没网」。
#   3. 内核死了不用任何人去关开关：失败模式是 fail-open，不是 fail-closed。
#
# 对比 v0.4.1 的现状（系统级 HTTP/HTTPS/SOCKS 代理）：
#   现状 = 全局开关，任何一条连接先连 127.0.0.1:7891，连不上就 RST，
#          连 GEOSITE,cn → DIRECT 的规则都执行不到 → 90% 直连也一起死。
#
# 用法：
#   ./pac-failopen-proxy.sh on        # 生成 PAC + 起本地 PAC 服务 + 打开系统自动代理
#   ./pac-failopen-proxy.sh off       # 关自动代理 + 停 PAC 服务（回到纯直连）
#   ./pac-failopen-proxy.sh status    # 只读查看
#   ./pac-failopen-proxy.sh serve     # 只起 PAC 服务（不改系统设置，调试用）
#
# 环境变量：SERVICE（默认 Wi-Fi）、PROXY_PORT（默认 7891）、PAC_PORT（默认 18080）
# ─────────────────────────────────────────────────────────────────────────────
set -u

SERVICE="${SERVICE:-Wi-Fi}"
PROXY_PORT="${PROXY_PORT:-7891}"
PAC_PORT="${PAC_PORT:-18080}"
DIR="$HOME/Library/Application Support/pac-failopen"
PAC_FILE="$DIR/proxy.pac"
PAC_URL="http://127.0.0.1:$PAC_PORT/proxy.pac"
NS=/usr/sbin/networksetup

# 白名单：命中的域名走代理，其余一律直连。按需增删。
WHITELIST=(
  openai.com
  anthropic.com
  claude.ai
  github.com
  githubusercontent.com
  google.com
  gstatic.com
  youtube.com
  x.com
  twitter.com
  telegram.org
)

gen_pac() {
  mkdir -p "$DIR"
  {
    echo '// 自动生成，勿手改。改白名单请改 pac-failopen-proxy.sh 里的 WHITELIST。'
    echo 'function FindProxyForURL(url, host) {'
    echo '  host = host.toLowerCase();'
    printf '  var white = ['
    first=1
    for d in "${WHITELIST[@]}"; do
      [ $first -eq 1 ] || printf ','
      printf '"%s"' "$d"
      first=0
    done
    echo '];'
    echo '  for (var i = 0; i < white.length; i++) {'
    echo '    var d = white[i];'
    echo '    if (host === d || dnsDomainIs(host, "." + d)) {'
    echo "      return \"PROXY 127.0.0.1:$PROXY_PORT; DIRECT\";"
    echo '    }'
    echo '  }'
    echo '  return "DIRECT";'
    echo '}'
  } > "$PAC_FILE"
  echo "[pac] 已生成 ${PAC_FILE}（白名单 ${#WHITELIST[@]} 条）"
}

pac_pid() {
  # 注意：实际命令是 `python3 -m http.server <port> --bind 127.0.0.1 --directory <dir>`，
  # 「port 与 --directory 相邻」并不成立——旧模式因此匹配不到，off 时杀不掉服务。
  /usr/bin/pgrep -f "http\.server ${PAC_PORT}( |$)" 2>/dev/null | head -1
}

serve_pac() {
  if [ -n "$(pac_pid)" ]; then
    echo "[pac] 服务已在跑（PID $(pac_pid)）"
    return 0
  fi
  gen_pac
  nohup /usr/bin/python3 -m http.server "$PAC_PORT" --bind 127.0.0.1 --directory "$DIR" \
    >/dev/null 2>&1 &
  sleep 1
  if curl -fsS --max-time 3 "$PAC_URL" >/dev/null 2>&1; then
    echo "[pac] 服务就绪 $PAC_URL"
  else
    echo "[pac] 启动失败：$PAC_URL 不可达" >&2
    return 1
  fi
}

stop_pac() {
  local p
  p="$(pac_pid)"
  if [ -n "$p" ]; then
    kill "$p" 2>/dev/null
    echo "[pac] 已停止服务（PID ${p}）"
  else
    echo "[pac] 服务未在运行"
  fi
}

case "${1:-status}" in
  on)
    serve_pac || exit 1
    "$NS" -setautoproxyurl "$SERVICE" "$PAC_URL" || exit 1
    "$NS" -setautoproxystate "$SERVICE" on || exit 1
    echo "[pac] 已为「${SERVICE}」打开自动代理。非白名单流量现在物理直连。"
    "$NS" -getautoproxyurl "$SERVICE"
    ;;
  off)
    "$NS" -setautoproxystate "$SERVICE" off
    echo "[pac] 已关闭「${SERVICE}」自动代理（系统回到纯直连）"
    stop_pac
    ;;
  serve)
    serve_pac
    ;;
  status)
    echo "--- 系统自动代理（${SERVICE}）---"
    "$NS" -getautoproxyurl "$SERVICE" 2>&1
    echo "--- 全局视图 ---"
    /usr/sbin/scutil --proxy 2>&1 | grep -Ei "ProxyAutoConfig|HTTPEnable|SOCKSEnable|HTTPPort" || true
    echo "--- PAC 服务 ---"
    if [ -n "$(pac_pid)" ]; then echo "running PID $(pac_pid)  $PAC_URL"; else echo "not running"; fi
    echo "--- 白名单 ---"
    printf '  %s\n' "${WHITELIST[@]}"
    ;;
  *)
    sed -n '2,26p' "$0"
    exit 1
    ;;
esac
