#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// v0.4.2：无界面入口。断电重启后 launchd 会以 --selfheal 拉起本二进制，
// 在**不打开窗口**的情况下清理残留的 PAC/全局代理——这是"重启后自动恢复网络"
// 的唯一保证（旧版自愈只写在 GUI 启动路径里，用户不开 App 就永远不执行）。
// 其余开关是给支持排障用的：不弹窗、不初始化 Tauri、直接打印结果。
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--selfheal") {
        std::process::exit(magic_agent_lib::selfheal());
    }
    if args.iter().any(|a| a == "--dns-check") {
        println!("{}", magic_agent_lib::dns_check_json());
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--repair-dns") {
        println!("{}", magic_agent_lib::dns_repair_text());
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--proxy-status") {
        println!("{}", magic_agent_lib::proxy_status_text());
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--install-selfheal") {
        match magic_agent_lib::install_selfheal_agent() {
            Ok(p) => {
                println!("已安装登录自愈 agent: {p}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("安装失败: {e}");
                std::process::exit(1);
            }
        }
    }
    magic_agent_lib::run()
}
