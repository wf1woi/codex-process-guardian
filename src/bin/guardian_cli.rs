use std::io::{self, Write};

use codex_process_guardian::{Guardian, find_entry, format_entry, open_log};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};

fn main() {
    let guardian = Guardian::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--list") {
        list_once(&guardian);
        return;
    }
    if args.first().map(String::as_str) == Some("--stop-watch") {
        stop_watch();
        return;
    }
    terminal_manager(&guardian);
}

fn stop_watch() {
    let name = codex_process_guardian::to_wide("Local\\CodexProcessGuardianRustStop");
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if event.is_null() {
            eprintln!("后台监控未运行。");
            std::process::exit(1);
        }
        SetEvent(event);
        CloseHandle(event);
    }
    println!("已请求后台监控安全退出。");
}

fn list_once(guardian: &Guardian) {
    match guardian.scan() {
        Ok(entries) => {
            for (index, entry) in entries.iter().enumerate() {
                println!("{}", format_entry(index, entry));
            }
        }
        Err(error) => eprintln!("扫描失败：{error}"),
    }
}

fn terminal_manager(guardian: &Guardian) {
    loop {
        print!("\x1B[2J\x1B[H");
        let entries = match guardian.scan() {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("扫描失败：{error}");
                return;
            }
        };
        println!("Codex Process Guardian - Rust 终端管理器\n");
        if entries.is_empty() {
            println!("当前没有可显示的 ChatGPT/Codex 工具进程。");
        }
        for (index, entry) in entries.iter().enumerate() {
            println!("{}", format_entry(index, entry));
        }
        println!("\n输入序号或 PID 结束进程；R 刷新；L 打开日志；Q 退出。");
        print!("请选择：");
        let _ = io::stdout().flush();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return;
        }
        let input = input.trim();
        if input.eq_ignore_ascii_case("q") {
            return;
        }
        if input.eq_ignore_ascii_case("r") {
            continue;
        }
        if input.eq_ignore_ascii_case("l") {
            open_log(&guardian.paths);
            continue;
        }
        let Ok(value) = input.parse::<u32>() else {
            continue;
        };
        let Some(entry) = find_entry(&entries, value) else {
            pause("未找到对应进程。");
            continue;
        };
        if !entry.learned_owned {
            pause("该进程未确认由 ChatGPT/Codex 启动，拒绝结束。");
            continue;
        }
        println!("\n⚠️ 危险操作检测！");
        println!("操作类型：结束 PID {} {}", entry.pid, entry.name);
        println!("影响范围：该进程及全部子进程；状态 {}", entry.status.as_str());
        println!("风险评估：可能中断正在执行的浏览器或工具调用。");
        print!("请输入“是”、“确认”或“继续”：");
        let _ = io::stdout().flush();
        let mut confirmation = String::new();
        let _ = io::stdin().read_line(&mut confirmation);
        if !matches!(confirmation.trim(), "是" | "确认" | "继续") {
            continue;
        }
        let allow_active = entry.status.as_str() == "Owned";
        match guardian.terminate(entry, allow_active) {
            Ok(count) => pause(&format!("已结束进程树，共 {count} 个进程。")),
            Err(error) => pause(&format!("结束失败：{error}")),
        }
    }
}

fn pause(message: &str) {
    println!("{message}\n按 Enter 继续...");
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}
