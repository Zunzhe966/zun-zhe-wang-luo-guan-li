//! 双通道更新（本地开发测试 / GitHub 公开发布）
//!
//! 为什么需要这个模块：
//! Tauri updater 的端点是在**编译期**从 `tauri.conf.json` 的 `plugins.updater.endpoints`
//! 固定进二进制的，运行时无法直接切换。但本项目要同时满足两类使用者：
//!
//! | 通道 | 端点 | 使用者 |
//! |---|---|---|
//! | 本地 `local` | `http://127.0.0.1:7878/latest.json` | 开发者自己（本机起 http.server 调试） |
//! | GitHub `github` | `https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json` | 真正的终端用户 |
//!
//! 两个通道指向**同一份**更新包与同一份签名（`release.sh` 一次构建产出），
//! 只是 latest.json 里写的下载地址不同 —— 因此不存在两边不同步的问题。
//!
//! 因此这里用 `UpdaterBuilder::endpoints()` 在**运行时**重建 Updater，
//! 并把用户选择的通道持久化到 config.json，重启后依然生效。
//!
//! 安全：两个端点都经过同一把 Ed25519 公钥（编译期 config 里的 pubkey）验签，
//! 通道切换只改变「去哪儿取 latest.json」，不改变「必须验签」这条铁律。

use std::sync::Arc;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

/// 本地开发测试通道：配套 `scripts/release.sh` 生成 feed +
/// `python3 -m http.server 7878` 起服务，仅监听 127.0.0.1。
pub const ENDPOINT_LOCAL: &str = "http://127.0.0.1:7878/latest.json";

/// GitHub 公开发布通道：从 Releases 的 latest 资产里取 latest.json。
/// 这是给真实用户用的；发版脚本把同一份 feed 上传到 Release 资产即可。
/// 用源码仓库本身承载 Release —— 源码与发布天然同源，避免两套东西不同步。
pub const ENDPOINT_GITHUB: &str =
    "https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json";

/// 缺省通道：面向真实用户，所以默认走 GitHub。
pub const DEFAULT_CHANNEL: &str = "github";

/// 把任意字符串规范成合法通道名，未知值一律回落到缺省（github），
/// 避免 config.json 被手改成乱七八糟的值后 UI 显示空白。
pub fn normalize_channel(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "local" | "dev" | "development" => "local".to_string(),
        _ => DEFAULT_CHANNEL.to_string(),
    }
}

/// 通道名 → 端点 URL
pub fn endpoint_for(channel: &str) -> &'static str {
    match normalize_channel(channel).as_str() {
        "local" => ENDPOINT_LOCAL,
        _ => ENDPOINT_GITHUB,
    }
}

/// P1-6 端点安全校验（v0.4.1）：`dangerousInsecureTransportProtocol` 编译期开关
/// 只为 local 本机调试通道存在（github 通道全 https）。运行时按通道强校验：
///   - local 端点必须是 loopback（127.0.0.1/localhost/::1）——防止该危险开关被
///     误用于外部 http 端点（中间人可篡改更新包）；
///   - github 端点必须是 https——明文传输更新包 = 可被替换。
pub fn assert_endpoint_allowed(url: &tauri::Url, channel: &str) -> Result<(), String> {
    let norm = normalize_channel(channel);
    if norm == "local" {
        match url.host_str() {
            Some(h) if h == "127.0.0.1" || h == "localhost" || h == "::1" => Ok(()),
            _ => Err(format!(
                "local 通道端点必须是本机 loopback（127.0.0.1），收到: {url}"
            )),
        }
    } else if url.scheme() == "https" {
        Ok(())
    } else {
        Err(format!(
            "github 通道端点必须是 https（危险开关只服务 local 本机通道），收到: {url}"
        ))
    }
}

/// 通道名 → 人类可读的展示名（UI 用）
pub fn channel_label(channel: &str) -> &'static str {
    match normalize_channel(channel).as_str() {
        "local" => "本地开发测试",
        _ => "GitHub 公开发布",
    }
}

/// 供前端读取的通道快照
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateChannelInfo {
    /// 规范化后的通道名：local | github
    pub channel: String,
    /// 该通道对应的端点 URL
    pub endpoint: String,
    /// 展示名
    pub label: String,
}

impl UpdateChannelInfo {
    pub fn from_channel(channel: &str) -> Self {
        let c = normalize_channel(channel);
        Self {
            endpoint: endpoint_for(&c).to_string(),
            label: channel_label(&c).to_string(),
            channel: c,
        }
    }
}

// 注意：这里必须用 crate::AppState（lib.rs 中 .manage(state) 注册的类型），
// 而不是直接用 AppConfig。lib.rs 实际注册的是 Arc<Mutex<AppState>>，
// 其中 AppState.config 才是 AppConfig。如果这里写 Arc<Mutex<AppConfig>>，
// 调用 app.state::<ConfigState>() 会 panic：state() called before manage()。
type ConfigState = Arc<Mutex<crate::AppState>>;

fn read_channel(state: &ConfigState) -> String {
    let guard = state.lock().unwrap_or_else(|e| e.into_inner());
    normalize_channel(guard.config.update_channel.as_deref().unwrap_or(DEFAULT_CHANNEL))
}

/// 读取当前更新通道（不触网，纯读配置）。
#[tauri::command]
pub fn get_update_channel(state: State<'_, ConfigState>) -> UpdateChannelInfo {
    UpdateChannelInfo::from_channel(&read_channel(state.inner()))
}

/// 切换更新通道并持久化到 config.json。
/// 返回切换后的通道信息，供前端立即刷新文案。
#[tauri::command]
pub fn set_update_channel(
    channel: String,
    state: State<'_, ConfigState>,
) -> Result<UpdateChannelInfo, String> {
    let normalized = normalize_channel(&channel);
    {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.config.update_channel = Some(normalized.clone());
        crate::config::save(&guard.config)?;
    }
    Ok(UpdateChannelInfo::from_channel(&normalized))
}

/// 当前 App 版本
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub available: bool,
    /// 新版本号（available=false 时为 None）
    pub version: Option<String>,
    /// 更新说明（notes）
    pub notes: Option<String>,
    /// 端点（用于 UI 展示，确认你到底在查哪个源）
    pub endpoint: String,
    pub channel: String,
}

/// 按当前通道检查更新（只检查，不下载安装）。
///
/// 之所以不让前端直接调 `plugin-updater` 的 `check()`：那个命令固定用编译期端点，
/// 无法跟随用户选择的通道变化。这里在运行时用 `endpoints()` 重建 Updater。
#[tauri::command]
pub async fn check_channel_update(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let state = app.state::<ConfigState>();
    let channel = read_channel(state.inner());
    let endpoint_str = endpoint_for(&channel);
    let url = tauri::Url::parse(endpoint_str).map_err(|e| format!("端点 URL 非法: {e}"))?;
    assert_endpoint_allowed(&url, &channel)?;

    // 运行时覆盖端点，其余（pubkey、dangerous 开关、target）沿用编译期配置
    let updater = app
        .updater_builder()
        .endpoints(vec![url])
        .map_err(|e| format!("端点校验失败: {e}"))?
        .build()
        .map_err(|e| format!("初始化更新器失败: {e}"))?;

    let current_version = app.package_info().version.to_string();

    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateCheckResult {
            current_version,
            available: true,
            version: Some(update.version.clone()),
            notes: update.body.clone(),
            endpoint: endpoint_str.to_string(),
            channel,
        }),
        Ok(None) => Ok(UpdateCheckResult {
            current_version,
            available: false,
            version: None,
            notes: None,
            endpoint: endpoint_str.to_string(),
            channel,
        }),
        Err(e) => Err(format!("检查更新失败（{endpoint_str}）: {e}")),
    }
}

/// 按当前通道下载并安装更新（含 Ed25519 验签 + 覆盖安装）。
///
/// 进度通过全局事件 `update-progress` 推送给前端（`listen` 监听）：
/// - `{ event: "progress", chunkLength, downloaded, contentLength }`
/// - `{ event: "finished" }`
///
/// 注意 macOS/Linux：安装完成后需调用 plugin-process 的 relaunch 重启。
#[tauri::command]
pub async fn install_channel_update(app: AppHandle) -> Result<(), String> {
    let state = app.state::<ConfigState>();
    let channel = read_channel(state.inner());
    let endpoint_str = endpoint_for(&channel);
    let url = tauri::Url::parse(endpoint_str).map_err(|e| format!("端点 URL 非法: {e}"))?;
    assert_endpoint_allowed(&url, &channel)?;

    let updater = app
        .updater_builder()
        .endpoints(vec![url])
        .map_err(|e| format!("端点校验失败: {e}"))?
        .build()
        .map_err(|e| format!("初始化更新器失败: {e}"))?;

    let update = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败: {e}"))?
        .ok_or_else(|| "已是最新版本，无需安装".to_string())?;

    // 让更新下载走网络管理自己的 mihomo，不走系统直连国际出口。
    // 否则几十兆 .app.tar.gz 直连 GitHub 慢得不可用——
    // 一个代理软件自己更新还慢，不可接受。
    //
    // 做法：临时给本进程注入 HTTP(S)_PROXY 指向 mihomo mixed-port（127.0.0.1:port），
    // 并加 NO_PROXY=127.0.0.1,localhost 防止 local 通道 feed 与 mihomo 控制请求被自己套住。
    // reqwest 在同进程内会读 env，所以能生效。
    let proxy_guard = scope_update_proxy_env(state.inner());

    let app_for_progress = app.clone();
    let app_for_finish = app.clone();
    let mut downloaded: usize = 0;

    let result = update
        .download_and_install(
            move |chunk_length, content_length| {
                downloaded += chunk_length;
                // AppHandle 的事件是全局广播，前端用 listen 接收即可，
                // 不必关心是哪个 window（本项目只有 main 一个窗口）。
                let _ = app_for_progress.emit(
                    "update-progress",
                    serde_json::json!({
                        "event": "progress",
                        "chunkLength": chunk_length,
                        "downloaded": downloaded,
                        "contentLength": content_length,
                    }),
                );
            },
            move || {
                let _ = app_for_finish.emit(
                    "update-progress",
                    serde_json::json!({ "event": "finished" }),
                );
            },
        )
        .await;

    // 不管成功失败都恢复 env，避免后续请求仍套代理
    drop(proxy_guard);

    result.map_err(|e| format!("下载/安装失败: {e}"))?;
    Ok(())
}

/// RAII 代理 env 守卫：构造时注入 HTTP_PROXY 等，析构时恢复原值。
/// 仅在 mihomo 在跑时才注入，否则返回 None（让请求走默认路径）。
struct ProxyEnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl Drop for ProxyEnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// 读取并设置更新下载用代理 env，返回 guard 用于结束时恢复。
/// mihomo 未在跑时返回 None，让更新走默认路径（避免指向不通的端口导致下载失败）。
fn scope_update_proxy_env(state: &ConfigState) -> Option<ProxyEnvGuard> {
    let (port, running) = {
        let g = state.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.mihomo.status();
        (m.port, m.running)
    };
    if !running {
        return None;
    }
    let proxy_url = format!("http://127.0.0.1:{port}");
    let keys = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ];
    let mut saved = Vec::with_capacity(keys.len() + 2);
    for k in keys {
        saved.push((k, std::env::var(k).ok()));
        std::env::set_var(k, &proxy_url);
    }
    // local 通道 feed 与 mihomo 控制接口都在 loopback，绝不能走代理
    saved.push(("NO_PROXY", std::env::var("NO_PROXY").ok()));
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost,::1");
    Some(ProxyEnvGuard { saved })
}
