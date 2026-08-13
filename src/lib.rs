use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetProcessTimes, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, ReleaseMutex, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process_handle: HANDLE,
        process_information_class: u32,
        process_information: *mut c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;
const SCAN_MUTEX: &str = "Local\\CodexProcessGuardianRustScan";
const MAX_INTERVAL_SECONDS: u64 = 86_400;
const MAX_THRESHOLD_MINUTES: u64 = 43_200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Category {
    Direct,
    Browser,
    ToolServer,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::Browser => "Browser",
            Self::ToolServer => "ToolServer",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "Direct" => Some(Self::Direct),
            "Browser" => Some(Self::Browser),
            "ToolServer" => Some(Self::ToolServer),
            _ => None,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Direct => "Codex 核心",
            Self::Browser => "浏览器自动化",
            Self::ToolServer => "工具服务",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Owned,
    OwnedIdleBrowser,
    OwnedIdleTool,
    GracePeriod,
    Candidate,
    SuspectOnly,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owned => "Owned",
            Self::OwnedIdleBrowser => "OwnedIdleBrowser",
            Self::OwnedIdleTool => "OwnedIdleTool",
            Self::GracePeriod => "GracePeriod",
            Self::Candidate => "Candidate",
            Self::SuspectOnly => "SuspectOnly",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Owned => "正常运行",
            Self::OwnedIdleBrowser => "浏览器闲置",
            Self::OwnedIdleTool => "工具服务闲置",
            Self::GracePeriod => "等待清理",
            Self::Candidate => "建议清理",
            Self::SuspectOnly => "归属未确认",
        }
    }

    pub fn risk_name(&self) -> &'static str {
        match self {
            Self::Candidate => "高",
            Self::OwnedIdleBrowser | Self::OwnedIdleTool => "中",
            Self::GracePeriod | Self::SuspectOnly => "低",
            Self::Owned => "正常",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub command_line: String,
    pub started: u64,
    pub cpu_ticks: u64,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub identity: String,
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub command_line: String,
    pub category: Category,
    pub learned_owned: bool,
    pub first_seen: u64,
    pub last_seen: u64,
    pub last_activity: u64,
    pub cpu_ticks: u64,
    pub orphan_since: u64,
    pub status: Status,
    pub started: u64,
    pub owner_pid: u32,
    pub owner_name: String,
    pub relation_path: String,
    pub child_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct BatchTerminationResult {
    pub requested: usize,
    pub processed_roots: usize,
    pub terminated_processes: usize,
    pub skipped_overlaps: usize,
    pub failures: Vec<(u32, String)>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub interval_seconds: u64,
    pub grace_minutes: u64,
    pub owned_browser_idle_minutes: u64,
    pub owned_tool_idle_minutes: u64,
    pub action_terminate: bool,
    pub terminate_owned_idle_browser: bool,
    pub terminate_owned_idle_tool: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_seconds: 30,
            grace_minutes: 5,
            owned_browser_idle_minutes: 20,
            owned_tool_idle_minutes: 30,
            action_terminate: false,
            terminate_owned_idle_browser: false,
            terminate_owned_idle_tool: false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let mut config = Self::default();
        let Ok(text) = fs::read_to_string(path) else {
            return config;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "interval_seconds" => {
                    config.interval_seconds = value
                        .parse::<u64>()
                        .unwrap_or(30)
                        .clamp(5, MAX_INTERVAL_SECONDS)
                }
                "grace_minutes" => {
                    config.grace_minutes = value
                        .parse::<u64>()
                        .unwrap_or(5)
                        .clamp(1, MAX_THRESHOLD_MINUTES)
                }
                "owned_browser_idle_minutes" => {
                    config.owned_browser_idle_minutes = value
                        .parse::<u64>()
                        .unwrap_or(20)
                        .clamp(1, MAX_THRESHOLD_MINUTES)
                }
                "owned_tool_idle_minutes" => {
                    config.owned_tool_idle_minutes = value
                        .parse::<u64>()
                        .unwrap_or(30)
                        .clamp(1, MAX_THRESHOLD_MINUTES)
                }
                "action" => config.action_terminate = value.eq_ignore_ascii_case("terminate"),
                "terminate_owned_idle_browser" => {
                    config.terminate_owned_idle_browser = parse_bool(value)
                }
                "terminate_owned_idle_tool" => {
                    config.terminate_owned_idle_tool = parse_bool(value)
                }
                _ => {}
            }
        }
        config
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let data_dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("CodexProcessGuardian");
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            state: data_dir.join("state-rust.tsv"),
            log: data_dir.join("events-rust.log"),
            config: exe_dir.join("guardian.conf"),
            data_dir,
        }
    }
}

pub struct Guardian {
    pub paths: Paths,
    pub config: Config,
}

impl Guardian {
    pub fn new() -> Self {
        let paths = Paths::discover();
        let config = Config::load(&paths.config);
        Self { paths, config }
    }

    pub fn scan(&self) -> std::io::Result<Vec<Entry>> {
        let _scan_lock = ScanLock::acquire()?;
        fs::create_dir_all(&self.paths.data_dir)?;
        let now = unix_seconds();
        let processes = snapshot_processes()?;
        let by_id: HashMap<u32, &ProcessInfo> = processes.iter().map(|p| (p.pid, p)).collect();
        let children = build_children(&processes);
        let hosts: HashSet<u32> = processes
            .iter()
            .filter(|p| is_host(p))
            .map(|p| p.pid)
            .collect();
        let old_entries = read_state(&self.paths.state);
        let old_by_identity: HashMap<&str, &Entry> = old_entries
            .iter()
            .map(|entry| (entry.identity.as_str(), entry))
            .collect();
        let mut entries = Vec::new();

        for process in &processes {
            if is_host(process) {
                continue;
            }
            let Some(category) = classify(process) else {
                continue;
            };
            let identity = identity(process);
            let old = old_by_identity.get(identity.as_str()).copied();
            let ancestor_ids = ancestors(process, &by_id);
            let owner = ancestor_ids
                .iter()
                .find(|pid| hosts.contains(pid))
                .and_then(|pid| by_id.get(pid).copied());
            let owner_live = owner.is_some();
            let learned_owned = owner_live || old.is_some_and(|entry| entry.learned_owned);
            let first_seen = old.map_or(now, |entry| entry.first_seen);
            let mut last_activity = old.map_or(now, |entry| entry.last_activity);
            let cpu_ticks = if matches!(category, Category::Browser | Category::ToolServer) {
                process_tree_cpu_ticks(process.pid, &by_id, &children)
            } else {
                process.cpu_ticks
            };
            if matches!(category, Category::Browser | Category::ToolServer)
                && old.is_none_or(|entry| cpu_ticks.abs_diff(entry.cpu_ticks) >= 1_000_000)
            {
                last_activity = now;
            }

            let mut orphan_since = old.map_or(0, |entry| entry.orphan_since);
            let status = if !learned_owned {
                orphan_since = 0;
                Status::SuspectOnly
            } else if owner_live {
                orphan_since = 0;
                if category == Category::Browser
                    && now.saturating_sub(last_activity)
                        >= self.config.owned_browser_idle_minutes.saturating_mul(60)
                {
                    Status::OwnedIdleBrowser
                } else if category == Category::ToolServer
                    && now.saturating_sub(last_activity)
                        >= self.config.owned_tool_idle_minutes.saturating_mul(60)
                {
                    Status::OwnedIdleTool
                } else {
                    Status::Owned
                }
            } else {
                if orphan_since == 0 {
                    orphan_since = now;
                }
                if now.saturating_sub(orphan_since)
                    >= self.config.grace_minutes.saturating_mul(60)
                {
                    Status::Candidate
                } else {
                    Status::GracePeriod
                }
            };

            let entry = Entry {
                identity,
                pid: process.pid,
                parent_pid: process.parent_pid,
                name: process.name.clone(),
                command_line: process.command_line.clone(),
                category,
                learned_owned,
                first_seen,
                last_seen: now,
                last_activity,
                cpu_ticks,
                orphan_since,
                status,
                started: process.started,
                owner_pid: owner.map_or(0, |item| item.pid),
                owner_name: owner.map_or_else(String::new, |item| item.name.clone()),
                relation_path: relation_path(process, &ancestor_ids, &by_id, owner.map(|item| item.pid)),
                child_count: descendant_ids(process.pid, &children).len(),
            };
            if old.map(|item| item.status.as_str()) != Some(entry.status.as_str()) {
                let _ = self.log(&format!(
                    "status pid={} name={} status={} category={}",
                    entry.pid,
                    clean_field(&entry.name),
                    entry.status.as_str(),
                    entry.category.as_str()
                ));
            }
            entries.push(entry);
        }

        write_state(&self.paths.state, &entries)?;
        Ok(entries)
    }

    pub fn automatic_cleanup(&self, entries: &[Entry]) -> Vec<String> {
        if !self.config.action_terminate {
            return Vec::new();
        }
        let mut failures = Vec::new();
        for entry in entries {
            let allowed = entry.status == Status::Candidate
                || (entry.status == Status::OwnedIdleBrowser
                    && self.config.terminate_owned_idle_browser)
                || (entry.status == Status::OwnedIdleTool
                    && self.config.terminate_owned_idle_tool);
            if allowed && entry.learned_owned {
                if let Err(error) = self.terminate_internal(entry, false, false) {
                    failures.push(format!("pid={} error={}", entry.pid, clean_field(&error)));
                }
            }
        }
        failures
    }

    pub fn terminate(&self, entry: &Entry, allow_active: bool) -> Result<usize, String> {
        self.terminate_internal(entry, allow_active, true)
    }

    fn terminate_internal(
        &self,
        entry: &Entry,
        allow_active: bool,
        log_result: bool,
    ) -> Result<usize, String> {
        if !entry.learned_owned {
            return Err("未确认归属 ChatGPT/Codex，拒绝结束".into());
        }
        if entry.status == Status::Owned && !allow_active {
            return Err("进程仍处于活动宿主树中".into());
        }
        let processes = snapshot_processes().map_err(|error| error.to_string())?;
        let Some(current) = processes.iter().find(|p| p.pid == entry.pid) else {
            return Err("进程已经退出".into());
        };
        if identity(current) != entry.identity {
            return Err("PID 已复用，身份校验失败".into());
        }

        let children = build_children(&processes);
        let mut targets: Vec<(u32, u64)> = descendant_ids(entry.pid, &children)
            .into_iter()
            .filter_map(|pid| {
                processes
                    .iter()
                    .find(|process| process.pid == pid)
                    .map(|process| (pid, process.started))
            })
            .collect();
        targets.reverse();
        targets.push((entry.pid, current.started));
        let mut failed = Vec::new();
        for (pid, started) in &targets {
            unsafe {
                let handle = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                    0,
                    *pid,
                );
                if handle.is_null() {
                    if process_exists(*pid) {
                        failed.push(*pid);
                    }
                    continue;
                }
                if process_started_from_handle(handle) != Some(*started) {
                    CloseHandle(handle);
                    failed.push(*pid);
                    continue;
                }
                let ok = TerminateProcess(handle, 1);
                let wait = if ok != 0 {
                    WaitForSingleObject(handle, 2_000)
                } else {
                    WAIT_FAILED
                };
                CloseHandle(handle);
                if ok == 0 || wait != WAIT_OBJECT_0 {
                    failed.push(*pid);
                }
            }
        }
        failed.sort_unstable();
        failed.dedup();
        if !failed.is_empty() {
            let message = format!("结束失败，仍存活 PID: {failed:?}");
            if log_result {
                let _ = self.log(&format!("terminate_failed pid={} {message}", entry.pid));
            }
            return Err(message);
        }
        if log_result {
            let _ = self.log(&format!("terminated pid={} count={}", entry.pid, targets.len()));
        }
        Ok(targets.len())
    }

    pub fn terminate_batch(
        &self,
        entries: &[Entry],
        allow_active: bool,
    ) -> BatchTerminationResult {
        let mut result = BatchTerminationResult {
            requested: entries.len(),
            ..Default::default()
        };
        if entries.is_empty() {
            return result;
        }

        let processes = match snapshot_processes() {
            Ok(processes) => processes,
            Err(error) => {
                result.failures.push((0, format!("进程快照失败：{error}")));
                return result;
            }
        };
        let by_id: HashMap<u32, &ProcessInfo> = processes.iter().map(|item| (item.pid, item)).collect();
        let selected: HashSet<u32> = entries.iter().map(|entry| entry.pid).collect();
        let mut roots = Vec::new();
        for entry in entries {
            let is_overlap = by_id
                .get(&entry.pid)
                .map(|process| ancestors(process, &by_id).iter().any(|pid| selected.contains(pid)))
                .unwrap_or(false);
            if is_overlap {
                result.skipped_overlaps += 1;
            } else {
                roots.push(entry);
            }
        }

        result.processed_roots = roots.len();
        for entry in roots {
            match self.terminate(entry, allow_active) {
                Ok(count) => result.terminated_processes += count,
                Err(error) => result.failures.push((entry.pid, error)),
            }
        }
        result
    }

    pub fn log(&self, message: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.paths.data_dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.log)?;
        writeln!(file, "{} {}", unix_seconds(), message)
    }
}

struct ScanLock(HANDLE);

impl ScanLock {
    fn acquire() -> std::io::Result<Self> {
        unsafe {
            let name = to_wide(SCAN_MUTEX);
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let result = WaitForSingleObject(handle, INFINITE);
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                CloseHandle(handle);
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(handle))
        }
    }
}

impl Drop for ScanLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

impl Default for Guardian {
    fn default() -> Self {
        Self::new()
    }
}

pub fn snapshot_processes() -> std::io::Result<Vec<ProcessInfo>> {
    let mut result = Vec::with_capacity(256);
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) == 0 {
            let error = std::io::Error::last_os_error();
            CloseHandle(snapshot);
            return Err(error);
        }
        loop {
            let name = wide_z_to_string(&entry.szExeFile);
            let (command_line, started, cpu_ticks) =
                query_process(entry.th32ProcessID, is_relevant_process_name(&name));
            result.push(ProcessInfo {
                pid: entry.th32ProcessID,
                parent_pid: entry.th32ParentProcessID,
                name,
                command_line,
                started,
                cpu_ticks,
            });
            if Process32NextW(snapshot, &mut entry) == 0 {
                let error = GetLastError();
                if error != ERROR_NO_MORE_FILES {
                    CloseHandle(snapshot);
                    return Err(std::io::Error::from_raw_os_error(error as i32));
                }
                break;
            }
        }
        CloseHandle(snapshot);
    }
    Ok(result)
}

fn is_relevant_process_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "chatgpt.exe"
            | "codex.exe"
            | "codex-code-mode-host.exe"
            | "node_repl.exe"
            | "chrome.exe"
            | "msedge.exe"
            | "chromium.exe"
            | "headless_shell.exe"
            | "node.exe"
            | "python.exe"
            | "pythonw.exe"
            | "uvx.exe"
            | "npx.exe"
    )
}

unsafe fn query_process(pid: u32, include_details: bool) -> (String, u64, u64) {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return (String::new(), 0, 0);
    }
    let command_line = if include_details {
        unsafe { query_command_line(handle) }
    } else {
        String::new()
    };
    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return (command_line, 0, 0);
    }
    (
        command_line,
        filetime_to_u64(creation),
        if include_details {
            filetime_to_u64(kernel).saturating_add(filetime_to_u64(user))
        } else {
            0
        },
    )
}

unsafe fn process_started_from_handle(handle: HANDLE) -> Option<u64> {
    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        None
    } else {
        Some(filetime_to_u64(creation))
    }
}

unsafe fn query_command_line(handle: HANDLE) -> String {
    let mut required = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            PROCESS_COMMAND_LINE_INFORMATION,
            null_mut(),
            0,
            &mut required,
        )
    };
    if status != STATUS_INFO_LENGTH_MISMATCH || required == 0 || required > 1024 * 1024 {
        return String::new();
    }
    let mut buffer = vec![0u8; required as usize];
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            PROCESS_COMMAND_LINE_INFORMATION,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    if status < 0 || buffer.len() < size_of::<usize>() * 2 {
        return String::new();
    }
    let length = u16::from_ne_bytes([buffer[0], buffer[1]]) as usize;
    let base = buffer.as_ptr() as usize;
    let pointer_offset = if size_of::<usize>() == 8 { 8 } else { 4 };
    let pointer = if size_of::<usize>() == 8 {
        usize::from_ne_bytes(buffer[pointer_offset..pointer_offset + 8].try_into().unwrap())
    } else {
        u32::from_ne_bytes(buffer[pointer_offset..pointer_offset + 4].try_into().unwrap()) as usize
    };
    if pointer < base || pointer.saturating_add(length) > base.saturating_add(buffer.len()) {
        return String::new();
    }
    let offset = pointer - base;
    decode_utf16_bytes(&buffer[offset..offset + length])
}

fn decode_utf16_bytes(bytes: &[u8]) -> String {
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&words)
}

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn wide_z_to_string(value: &[u16]) -> String {
    let length = value.iter().position(|item| *item == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

fn identity(process: &ProcessInfo) -> String {
    format!("{}|{}", process.pid, process.started)
}

fn lower(process: &ProcessInfo) -> (String, String) {
    (
        process.name.to_ascii_lowercase(),
        process.command_line.to_ascii_lowercase(),
    )
}

fn is_host(process: &ProcessInfo) -> bool {
    let (name, command) = lower(process);
    if name == "codex.exe" && command.contains("app-server") {
        return true;
    }
    name == "chatgpt.exe"
        && !command.contains(" --type=")
        && (command.contains("openai.codex_")
            || command.contains("openai.chatgpt_")
            || command.contains("\\openai\\codex\\")
            || command.contains("\\openai\\chatgpt\\"))
}

fn classify(process: &ProcessInfo) -> Option<Category> {
    let (name, command) = lower(process);
    let direct_name = matches!(
        name.as_str(),
        "chatgpt.exe" | "codex.exe" | "codex-code-mode-host.exe" | "node_repl.exe"
    );
    let direct_marker = command.contains("openai.codex_")
        || command.contains("openai.chatgpt_")
        || command.contains("\\openai\\codex\\")
        || command.contains("\\openai\\chatgpt\\")
        || command.contains("\\codex\\runtimes\\")
        || command.contains("\\codex\\web\\codex");
    if direct_name && direct_marker {
        return Some(Category::Direct);
    }

    let browser = matches!(
        name.as_str(),
        "chrome.exe" | "msedge.exe" | "chromium.exe" | "headless_shell.exe"
    );
    let browser_marker = command.contains("--remote-debugging-port")
        || command.contains("--remote-debugging-pipe")
        || command.contains("playwright")
        || command.contains("puppeteer")
        || command.contains("codex")
        || command.contains("chatgpt");
    if browser && browser_marker {
        return Some(Category::Browser);
    }

    let runtime = matches!(
        name.as_str(),
        "node.exe" | "node_repl.exe" | "python.exe" | "pythonw.exe" | "uvx.exe" | "npx.exe"
    );
    let tool_marker = (command.contains("codegraph") && command.contains("serve --mcp"))
        || command.contains("@agentmemory\\mcp")
        || command.contains("@agentmemory/mcp")
        || command.contains("@modelcontextprotocol")
        || command.contains("playwright-mcp")
        || command.contains("\\openai\\codex\\runtimes\\cua_node\\")
        || (command.contains("kernel.js") && command.contains("--session-id"))
        || (command.contains("browser") && command.contains("--mcp"));
    if runtime && tool_marker {
        return Some(Category::ToolServer);
    }
    None
}

fn ancestors(process: &ProcessInfo, by_id: &HashMap<u32, &ProcessInfo>) -> Vec<u32> {
    let mut result = Vec::with_capacity(8);
    let mut visited = HashSet::new();
    let mut parent = process.parent_pid;
    let mut child_started = process.started;
    while parent != 0 && result.len() < 64 && visited.insert(parent) {
        let Some(item) = by_id.get(&parent) else {
            break;
        };
        if item.started == 0 || child_started == 0 || item.started > child_started {
            break;
        }
        result.push(parent);
        child_started = item.started;
        parent = item.parent_pid;
    }
    result
}

fn relation_path(
    process: &ProcessInfo,
    ancestors: &[u32],
    by_id: &HashMap<u32, &ProcessInfo>,
    owner_pid: Option<u32>,
) -> String {
    let mut parts = vec![format!("{} (PID {})", process.name, process.pid)];
    for pid in ancestors {
        let Some(item) = by_id.get(pid) else {
            break;
        };
        parts.push(format!("{} (PID {})", item.name, item.pid));
        if Some(*pid) == owner_pid {
            break;
        }
    }
    parts.reverse();
    parts.join("  →  ")
}

fn build_children(processes: &[ProcessInfo]) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let by_id: HashMap<u32, &ProcessInfo> = processes.iter().map(|process| (process.pid, process)).collect();
    for process in processes {
        let Some(parent) = by_id.get(&process.parent_pid) else {
            continue;
        };
        if parent.started != 0 && process.started != 0 && process.started >= parent.started {
            children.entry(process.parent_pid).or_default().push(process.pid);
        }
    }
    children
}

fn descendant_ids(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut result = Vec::new();
    let mut queue = VecDeque::from([root]);
    let mut visited = HashSet::from([root]);
    while let Some(parent) = queue.pop_front() {
        if let Some(items) = children.get(&parent) {
            for child in items {
                if visited.insert(*child) {
                    result.push(*child);
                    queue.push_back(*child);
                }
            }
        }
    }
    result
}

fn process_tree_cpu_ticks(
    root: u32,
    by_id: &HashMap<u32, &ProcessInfo>,
    children: &HashMap<u32, Vec<u32>>,
) -> u64 {
    descendant_ids(root, children)
        .into_iter()
        .chain([root])
        .filter_map(|pid| by_id.get(&pid))
        .fold(0u64, |total, process| total.saturating_add(process.cpu_ticks))
}

fn process_exists(pid: u32) -> bool {
    snapshot_processes()
        .map(|processes| processes.iter().any(|process| process.pid == pid))
        .unwrap_or(true)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn clean_field(value: &str) -> String {
    value.replace(['\t', '\r', '\n'], " ")
}

fn write_state(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = File::create(&temporary)?;
    writeln!(file, "v4")?;
    for entry in entries {
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean_field(&entry.identity),
            entry.pid,
            entry.parent_pid,
            clean_field(&entry.name),
            entry.category.as_str(),
            u8::from(entry.learned_owned),
            entry.first_seen,
            entry.last_seen,
            entry.last_activity,
            entry.cpu_ticks,
            entry.orphan_since,
            entry.status.as_str(),
            "",
            entry.started,
            entry.owner_pid,
            clean_field(&entry.owner_name),
            clean_field(&entry.relation_path),
            entry.child_count
        )?;
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let destination = to_wide(path.as_os_str());
    let replacement = to_wide(temporary.as_os_str());
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0 as REPLACE_FILE_FLAGS,
            null_mut(),
            null_mut(),
        )
    };
    if replaced == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn read_state(path: &Path) -> Vec<Entry> {
    let mut text = String::new();
    if File::open(path)
        .and_then(|mut file| file.read_to_string(&mut text))
        .is_err()
    {
        return Vec::new();
    }
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 13 {
                return None;
            }
            let category = Category::parse(fields[4])?;
            let status = match fields[11] {
                "Owned" => Status::Owned,
                "OwnedIdleBrowser" => Status::OwnedIdleBrowser,
                "OwnedIdleTool" => Status::OwnedIdleTool,
                "GracePeriod" => Status::GracePeriod,
                "Candidate" => Status::Candidate,
                "SuspectOnly" => Status::SuspectOnly,
                _ => return None,
            };
            Some(Entry {
                identity: fields[0].into(),
                pid: fields[1].parse().ok()?,
                parent_pid: fields[2].parse().ok()?,
                name: fields[3].into(),
                category,
                learned_owned: fields[5] == "1",
                first_seen: fields[6].parse().ok()?,
                last_seen: fields[7].parse().ok()?,
                last_activity: fields[8].parse().ok()?,
                cpu_ticks: fields[9].parse().ok()?,
                orphan_since: fields[10].parse().ok()?,
                status,
                command_line: fields[12].into(),
                started: fields.get(13).and_then(|value| value.parse().ok()).unwrap_or(0),
                owner_pid: fields.get(14).and_then(|value| value.parse().ok()).unwrap_or(0),
                owner_name: fields.get(15).copied().unwrap_or_default().into(),
                relation_path: fields.get(16).copied().unwrap_or_default().into(),
                child_count: fields.get(17).and_then(|value| value.parse().ok()).unwrap_or(0),
            })
        })
        .collect()
}

pub fn to_wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn open_log(paths: &Paths) {
    let _ = std::process::Command::new("notepad.exe").arg(&paths.log).spawn();
}

pub fn find_entry(entries: &[Entry], value: u32) -> Option<&Entry> {
    entries
        .get(value.saturating_sub(1) as usize)
        .or_else(|| entries.iter().find(|entry| entry.pid == value))
}

pub fn is_attention(entry: &Entry) -> bool {
    entry.status != Status::Owned
}

pub fn process_age_seconds(entry: &Entry) -> u64 {
    if entry.started == 0 {
        return 0;
    }
    const WINDOWS_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    let now_ticks = (unix_seconds().saturating_add(WINDOWS_TO_UNIX_SECONDS)) * 10_000_000;
    now_ticks.saturating_sub(entry.started) / 10_000_000
}

pub fn idle_seconds(entry: &Entry) -> u64 {
    if entry.last_activity == 0 {
        return 0;
    }
    unix_seconds().saturating_sub(entry.last_activity)
}

pub fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "未知".into();
    }
    if seconds >= 86_400 {
        format!("{}天{}小时", seconds / 86_400, (seconds % 86_400) / 3_600)
    } else if seconds >= 3_600 {
        format!("{}小时{}分", seconds / 3_600, (seconds % 3_600) / 60)
    } else if seconds >= 60 {
        format!("{}分{}秒", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}秒")
    }
}

pub fn format_entry(index: usize, entry: &Entry) -> String {
    format!(
        "{:>3}. PID {:<7} {:<24} {:<18} {}",
        index + 1,
        entry.pid,
        entry.name,
        entry.status.as_str(),
        entry.category.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_safe() {
        let config = Config::default();
        assert!(!config.action_terminate);
        assert!(!config.terminate_owned_idle_browser);
    }

    #[test]
    fn descendant_walk_returns_tree() {
        let make = |pid, parent_pid| ProcessInfo {
            pid,
            parent_pid,
            name: String::new(),
            command_line: String::new(),
            started: 1,
            cpu_ticks: 0,
        };
        let processes = [make(1, 0), make(2, 1), make(3, 2), make(4, 1)];
        let children = build_children(&processes);
        let ids = descendant_ids(1, &children);
        assert_eq!(ids, vec![2, 4, 3]);
    }

    #[test]
    fn browser_classification_requires_marker() {
        let process = ProcessInfo {
            pid: 1,
            parent_pid: 0,
            name: "chrome.exe".into(),
            command_line: "chrome.exe --remote-debugging-pipe".into(),
            started: 1,
            cpu_ticks: 0,
        };
        assert_eq!(classify(&process), Some(Category::Browser));
    }

    #[test]
    fn cua_runtime_is_classified_as_tool_server() {
        let process = ProcessInfo {
            pid: 1,
            parent_pid: 0,
            name: "node.exe".into(),
            command_line: r#"C:\Users\x\AppData\Local\OpenAI\Codex\runtimes\cua_node\bin\node.exe kernel.js --session-id abc"#.into(),
            started: 1,
            cpu_ticks: 0,
        };
        assert_eq!(classify(&process), Some(Category::ToolServer));
    }

    fn fake_entry() -> Entry {
        Entry {
            identity: "999999|1".into(),
            pid: 999999,
            parent_pid: 0,
            name: "node.exe".into(),
            command_line: String::new(),
            category: Category::ToolServer,
            learned_owned: false,
            first_seen: 0,
            last_seen: 0,
            last_activity: 0,
            cpu_ticks: 0,
            orphan_since: 0,
            status: Status::SuspectOnly,
            started: 1,
            owner_pid: 0,
            owner_name: String::new(),
            relation_path: String::new(),
            child_count: 0,
        }
    }

    #[test]
    fn terminate_rejects_unlearned_process_before_os_access() {
        let guardian = Guardian::new();
        let error = guardian.terminate(&fake_entry(), false).unwrap_err();
        assert!(error.contains("未确认归属"));
    }

    #[test]
    fn terminate_requires_explicit_active_permission() {
        let guardian = Guardian::new();
        let mut entry = fake_entry();
        entry.learned_owned = true;
        entry.status = Status::Owned;
        let error = guardian.terminate(&entry, false).unwrap_err();
        assert!(error.contains("活动宿主树"));
    }

    #[test]
    fn duration_is_compact_and_readable() {
        assert_eq!(format_duration(0), "未知");
        assert_eq!(format_duration(59), "59秒");
        assert_eq!(format_duration(61), "1分1秒");
        assert_eq!(format_duration(3_661), "1小时1分");
        assert_eq!(format_duration(90_000), "1天1小时");
    }

    #[test]
    fn relation_path_runs_from_owner_to_current_process() {
        let make = |pid, parent_pid, name: &str| ProcessInfo {
            pid,
            parent_pid,
            name: name.into(),
            command_line: String::new(),
            started: pid as u64,
            cpu_ticks: 0,
        };
        let processes = [
            make(10, 0, "Codex.exe"),
            make(20, 10, "launcher.exe"),
            make(30, 20, "node.exe"),
        ];
        let by_id: HashMap<u32, &ProcessInfo> =
            processes.iter().map(|process| (process.pid, process)).collect();
        let ancestor_ids = ancestors(&processes[2], &by_id);

        assert_eq!(
            relation_path(&processes[2], &ancestor_ids, &by_id, Some(10)),
            "Codex.exe (PID 10)  →  launcher.exe (PID 20)  →  node.exe (PID 30)"
        );
    }

    #[test]
    fn config_values_are_capped_to_safe_ranges() {
        let path = std::env::temp_dir().join(format!(
            "codex-guardian-config-test-{}.conf",
            std::process::id()
        ));
        fs::write(
            &path,
            "interval_seconds=18446744073709551615\ngrace_minutes=18446744073709551615\nowned_browser_idle_minutes=18446744073709551615\nowned_tool_idle_minutes=18446744073709551615\n",
        )
        .unwrap();
        let config = Config::load(&path);
        let _ = fs::remove_file(path);

        assert!(config.interval_seconds <= 86_400);
        assert!(config.grace_minutes <= 43_200);
        assert!(config.owned_browser_idle_minutes <= 43_200);
        assert!(config.owned_tool_idle_minutes <= 43_200);
    }

    #[test]
    fn persisted_state_does_not_store_command_line() {
        let path = std::env::temp_dir().join(format!(
            "codex-guardian-state-test-{}.tsv",
            std::process::id()
        ));
        let mut entry = fake_entry();
        entry.command_line = "node.exe --secret-value should-not-persist".into();
        write_state(&path, &[entry]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(path);

        assert!(!text.contains("secret-value"));
    }

    #[test]
    fn utf16_decoder_accepts_unaligned_byte_slices() {
        let words: Vec<u16> = "进程安全".encode_utf16().collect();
        let mut bytes = vec![0x7f];
        bytes.extend(words.iter().flat_map(|word| word.to_ne_bytes()));

        assert_eq!(decode_utf16_bytes(&bytes[1..]), "进程安全");
    }

    #[test]
    fn process_handle_creation_time_matches_snapshot_identity() {
        let pid = std::process::id();
        let processes = snapshot_processes().unwrap();
        let expected = processes
            .iter()
            .find(|process| process.pid == pid)
            .map(|process| process.started)
            .unwrap();
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            assert!(!handle.is_null());
            let actual = process_started_from_handle(handle);
            CloseHandle(handle);
            assert_eq!(actual, Some(expected));
        }
    }
}
