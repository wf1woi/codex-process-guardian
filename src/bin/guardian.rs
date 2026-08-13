#![windows_subsystem = "windows"]

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::time::Duration;

use codex_process_guardian::{
    Entry, Guardian, Status, format_duration, idle_seconds, is_attention, open_log,
    process_age_seconds, to_wide,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HWND, LPARAM, LRESULT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET, DeleteObject, FF_DONTCARE,
    FW_NORMAL, HFONT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, CreateMutexW, CreateProcessW, EVENT_MODIFY_STATE, OpenEventW,
    PROCESS_INFORMATION, ReleaseMutex, STARTUPINFOW, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_TABLE: i32 = 100;
const ID_DETAILS: i32 = 101;
const ID_REFRESH: i32 = 102;
const ID_START: i32 = 103;
const ID_STOP: i32 = 104;
const ID_TERMINATE: i32 = 105;
const ID_LOG: i32 = 106;
const ID_ATTENTION: i32 = 107;
const ID_STATUS: i32 = 108;
const ID_SEARCH: i32 = 109;
const ID_SEARCH_LABEL: i32 = 110;
const ID_SELECT_ATTENTION: i32 = 111;
const ID_SELECT_VISIBLE: i32 = 112;
const ID_CLEAR_CHECKS: i32 = 113;
const ID_HINT: i32 = 114;
const TIMER_REFRESH: usize = 1;
const WATCH_MUTEX: &str = "Local\\CodexProcessGuardianRustWatch";
const STOP_EVENT: &str = "Local\\CodexProcessGuardianRustStop";
const STATE_IMAGE_UNCHECKED: u32 = 1 << 12;
const STATE_IMAGE_CHECKED: u32 = 2 << 12;

struct AppState {
    guardian: Guardian,
    all_entries: Vec<Entry>,
    visible_entries: Vec<Entry>,
    checked: HashSet<String>,
    attention_only: bool,
    table: HWND,
    details: HWND,
    status: HWND,
    search: HWND,
    font: HFONT,
    scan_error: Option<String>,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
    static TABLE_UPDATING: Cell<bool> = const { Cell::new(false) };
}

fn main() {
    if std::env::args().any(|arg| arg == "--watch") {
        watch_loop();
    } else {
        gui_main();
    }
}

fn watch_loop() {
    unsafe {
        let mutex_name = to_wide(WATCH_MUTEX);
        let mutex = CreateMutexW(null(), 1, mutex_name.as_ptr());
        if mutex.is_null() || GetLastError() == 183 {
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return;
        }
        let event_name = to_wide(STOP_EVENT);
        let stop_event = CreateEventW(null(), 1, 0, event_name.as_ptr());
        if stop_event.is_null() {
            ReleaseMutex(mutex);
            CloseHandle(mutex);
            return;
        }
        let guardian = Guardian::new();
        let mut last_error = String::new();
        loop {
            let current_error = match guardian.scan() {
                Ok(entries) => guardian.automatic_cleanup(&entries).join(" | "),
                Err(error) => format!("scan_failed error={error}"),
            };
            if current_error != last_error {
                if current_error.is_empty() {
                    if !last_error.is_empty() {
                        let _ = guardian.log("watch_recovered");
                    }
                } else {
                    let _ = guardian.log(&format!("watch_error {current_error}"));
                }
                last_error = current_error;
            }
            let timeout_ms = u32::try_from(
                guardian.config.interval_seconds.saturating_mul(1000),
            )
            .unwrap_or(u32::MAX - 1);
            let wait = WaitForSingleObject(stop_event, timeout_ms);
            if wait == WAIT_OBJECT_0 || wait == WAIT_FAILED {
                break;
            }
        }
        ReleaseMutex(mutex);
        CloseHandle(stop_event);
        CloseHandle(mutex);
    }
}

fn gui_main() {
    unsafe {
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        InitCommonControlsEx(&controls);

        let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
        let class_name = to_wide("CodexGuardianRustWindowV2");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            lpszClassName: class_name.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&class);
        let title = to_wide("Codex Process Guardian - 进程关系与批量管理");
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1380,
            820,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        );
        if window.is_null() {
            return;
        }
        let mut message: MSG = zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => unsafe {
            create_controls(window);
            refresh_scan(window);
            SetTimer(window, TIMER_REFRESH, 15_000, None);
            0
        },
        WM_SIZE => unsafe {
            layout(window);
            0
        },
        WM_TIMER if wparam == TIMER_REFRESH => unsafe {
            refresh_scan(window);
            0
        },
        WM_COMMAND => unsafe {
            let id = (wparam & 0xffff) as i32;
            let notify = ((wparam >> 16) & 0xffff) as u32;
            match id {
                ID_REFRESH => refresh_scan(window),
                ID_START => start_watch(window),
                ID_STOP => stop_watch(window),
                ID_TERMINATE => terminate_checked(window),
                ID_LOG => with_state(|state| open_log(&state.guardian.paths)),
                ID_SELECT_ATTENTION => check_matching(true, false),
                ID_SELECT_VISIBLE => check_matching(false, true),
                ID_CLEAR_CHECKS => clear_checks(),
                ID_ATTENTION => {
                    with_state_mut(|state| {
                        state.attention_only = SendMessageW(
                            GetDlgItem(window, ID_ATTENTION),
                            BM_GETCHECK,
                            0,
                            0,
                        ) == BST_CHECKED as isize;
                    });
                    rebuild_table(window);
                }
                ID_SEARCH if notify == EN_CHANGE => rebuild_table(window),
                _ => {}
            }
            0
        },
        WM_NOTIFY => unsafe {
            let header = &*(lparam as *const NMHDR);
            if header.idFrom == ID_TABLE as usize
                && (header.code == LVN_ITEMCHANGED || header.code == NM_CLICK)
                && !table_updating()
            {
                sync_checks_from_table();
                show_details();
                update_summary();
            }
            0
        },
        WM_DESTROY => unsafe {
            KillTimer(window, TIMER_REFRESH);
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().take() {
                    let font = state.borrow().font;
                    if !font.is_null() {
                        DeleteObject(font as _);
                    }
                }
            });
            PostQuitMessage(0);
            0
        },
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn create_controls(window: HWND) {
    let button = to_wide("BUTTON");
    let edit = to_wide("EDIT");
    let static_class = to_wide("STATIC");

    create_control(window, &button, "刷新", ID_REFRESH, BS_PUSHBUTTON as u32, 0);
    create_control(window, &button, "启动监控", ID_START, BS_PUSHBUTTON as u32, 0);
    create_control(window, &button, "停止监控", ID_STOP, BS_PUSHBUTTON as u32, 0);
    create_control(window, &button, "打开日志", ID_LOG, BS_PUSHBUTTON as u32, 0);
    create_control(
        window,
        &button,
        "勾选需关注",
        ID_SELECT_ATTENTION,
        BS_PUSHBUTTON as u32,
        0,
    );
    create_control(
        window,
        &button,
        "全选当前",
        ID_SELECT_VISIBLE,
        BS_PUSHBUTTON as u32,
        0,
    );
    create_control(
        window,
        &button,
        "清空勾选",
        ID_CLEAR_CHECKS,
        BS_PUSHBUTTON as u32,
        0,
    );
    create_control(
        window,
        &button,
        "批量结束已勾选",
        ID_TERMINATE,
        BS_DEFPUSHBUTTON as u32,
        0,
    );
    let attention = create_control(
        window,
        &button,
        "只看需关注",
        ID_ATTENTION,
        BS_AUTOCHECKBOX as u32,
        0,
    );
    SendMessageW(attention, BM_SETCHECK, BST_CHECKED as usize, 0);
    create_control(window, &static_class, "搜索：", ID_SEARCH_LABEL, 0, 0);
    let search = create_control(
        window,
        &edit,
        "",
        ID_SEARCH,
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
    );
    create_control(
        window,
        &static_class,
        "提示：勾选父进程会包含子进程；父子重复勾选会自动去重。",
        ID_HINT,
        0,
        0,
    );

    let table = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        WC_LISTVIEWW,
        null(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | WS_HSCROLL | LVS_REPORT | LVS_SHOWSELALWAYS,
        0,
        0,
        100,
        100,
        window,
        ID_TABLE as usize as _,
        null_mut(),
        null_mut(),
    );
    SendMessageW(
        table,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        (LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER) as isize,
    );
    create_columns(table);

    let details = create_control(
        window,
        &edit,
        "选择一行查看进程关系、运行信息和完整命令行。",
        ID_DETAILS,
        ES_MULTILINE as u32 | ES_READONLY as u32 | WS_VSCROLL,
        WS_EX_CLIENTEDGE,
    );
    let status = create_control(window, &static_class, "正在扫描...", ID_STATUS, 0, 0);
    let font = create_ui_font();
    if !font.is_null() {
        for id in [
            ID_TABLE,
            ID_DETAILS,
            ID_REFRESH,
            ID_START,
            ID_STOP,
            ID_TERMINATE,
            ID_LOG,
            ID_ATTENTION,
            ID_STATUS,
            ID_SEARCH,
            ID_SEARCH_LABEL,
            ID_SELECT_ATTENTION,
            ID_SELECT_VISIBLE,
            ID_CLEAR_CHECKS,
            ID_HINT,
        ] {
            SendMessageW(GetDlgItem(window, id), WM_SETFONT, font as usize, 1);
        }
    }
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(Rc::new(RefCell::new(AppState {
            guardian: Guardian::new(),
            all_entries: Vec::new(),
            visible_entries: Vec::new(),
            checked: HashSet::new(),
            attention_only: true,
            table,
            details,
            status,
            search,
            font,
            scan_error: None,
        })));
    });
    layout(window);
}

unsafe fn create_ui_font() -> HFONT {
    let face = to_wide("Microsoft YaHei UI");
    CreateFontW(
        -16,
        0,
        0,
        0,
        FW_NORMAL as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        FF_DONTCARE as u32,
        face.as_ptr(),
    )
}

unsafe fn create_columns(table: HWND) {
    let columns = [
        ("风险", 62),
        ("状态", 118),
        ("PID", 76),
        ("进程", 165),
        ("类型", 125),
        ("归属宿主", 170),
        ("父 PID", 78),
        ("子进程", 72),
        ("运行时长", 110),
        ("闲置时长", 110),
    ];
    for (index, (title, width)) in columns.iter().enumerate() {
        let mut title = to_wide(title);
        let mut column: LVCOLUMNW = zeroed();
        column.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
        column.pszText = title.as_mut_ptr();
        column.cx = *width;
        column.fmt = if matches!(index, 2 | 6 | 7) {
            LVCFMT_RIGHT
        } else {
            LVCFMT_LEFT
        };
        SendMessageW(
            table,
            LVM_INSERTCOLUMNW,
            index,
            &column as *const LVCOLUMNW as isize,
        );
    }
}

unsafe fn create_control(
    window: HWND,
    class: &[u16],
    text: &str,
    id: i32,
    style: u32,
    ex_style: u32,
) -> HWND {
    let text = to_wide(text);
    CreateWindowExW(
        ex_style,
        class.as_ptr(),
        text.as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
        0,
        0,
        100,
        28,
        window,
        id as usize as _,
        null_mut(),
        null_mut(),
    )
}

unsafe fn layout(window: HWND) {
    let mut rect: RECT = zeroed();
    GetClientRect(window, &mut rect);
    let width = rect.right.max(920);
    let height = rect.bottom.max(600);
    let margin = 10;

    MoveWindow(GetDlgItem(window, ID_STATUS), margin, 8, width - 20, 24, 1);
    MoveWindow(GetDlgItem(window, ID_SEARCH_LABEL), margin, 39, 64, 26, 1);
    MoveWindow(GetDlgItem(window, ID_SEARCH), margin + 58, 37, 280, 28, 1);
    MoveWindow(GetDlgItem(window, ID_ATTENTION), margin + 350, 39, 120, 26, 1);

    let mut x = margin;
    for (id, button_width) in [
        (ID_REFRESH, 72),
        (ID_START, 92),
        (ID_STOP, 92),
        (ID_LOG, 82),
        (ID_SELECT_ATTENTION, 108),
        (ID_SELECT_VISIBLE, 92),
        (ID_CLEAR_CHECKS, 92),
        (ID_TERMINATE, 142),
    ] {
        MoveWindow(GetDlgItem(window, id), x, 72, button_width, 30, 1);
        x += button_width + 8;
    }
    MoveWindow(GetDlgItem(window, ID_HINT), x + 4, 77, (width - x - 14).max(120), 22, 1);

    let table_top = 110;
    let details_height = 210.min(height / 3);
    let table_height = height - table_top - details_height - 20;
    MoveWindow(
        GetDlgItem(window, ID_TABLE),
        margin,
        table_top,
        width - 20,
        table_height,
        1,
    );
    MoveWindow(
        GetDlgItem(window, ID_DETAILS),
        margin,
        table_top + table_height + 8,
        width - 20,
        details_height,
        1,
    );
}

unsafe fn refresh_scan(window: HWND) {
    sync_checks_from_table();
    let succeeded = STATE.with(|slot| {
        let state = slot.borrow();
        let Some(state) = state.as_ref() else {
            return false;
        };
        let mut state = state.borrow_mut();
        match state.guardian.scan() {
        Ok(entries) => {
            let valid: HashSet<&str> = entries.iter().map(|entry| entry.identity.as_str()).collect();
            state.checked.retain(|identity| valid.contains(identity.as_str()));
            state.all_entries = entries;
            state.scan_error = None;
            true
        }
        Err(error) => {
            state.scan_error = Some(error.to_string());
            false
        }
        }
    });
    if succeeded {
        rebuild_table(window);
    } else {
        update_summary();
    }
}

unsafe fn rebuild_table(_window: HWND) {
    TABLE_UPDATING.with(|updating| updating.set(true));
    with_state_mut(|state| {
        let search = window_text(state.search).to_ascii_lowercase();
        state.visible_entries = state
            .all_entries
            .iter()
            .filter(|entry| !state.attention_only || is_attention(entry))
            .filter(|entry| matches_search(entry, &search))
            .cloned()
            .collect();
        state.visible_entries.sort_by_key(|entry| {
            (
                status_order(&entry.status),
                entry.owner_pid,
                entry.name.to_ascii_lowercase(),
                entry.pid,
            )
        });

        SendMessageW(state.table, LVM_DELETEALLITEMS, 0, 0);
        for (row, entry) in state.visible_entries.iter().enumerate() {
            let values = row_values(entry);
            insert_row(state.table, row, &values, state.checked.contains(&entry.identity));
        }
    });
    TABLE_UPDATING.with(|updating| updating.set(false));
    update_summary();
}

fn table_updating() -> bool {
    TABLE_UPDATING.with(Cell::get)
}

fn matches_search(entry: &Entry, search: &str) -> bool {
    if search.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {} {} {} {} {}",
        entry.pid,
        entry.name,
        entry.category.display_name(),
        entry.status.display_name(),
        entry.owner_name,
        entry.parent_pid,
        entry.relation_path,
        entry.command_line
    )
    .to_ascii_lowercase();
    haystack.contains(search)
}

fn status_order(status: &Status) -> u8 {
    match status {
        Status::Candidate => 0,
        Status::OwnedIdleBrowser | Status::OwnedIdleTool => 1,
        Status::GracePeriod => 2,
        Status::SuspectOnly => 3,
        Status::Owned => 4,
    }
}

fn row_values(entry: &Entry) -> Vec<String> {
    vec![
        entry.status.risk_name().into(),
        entry.status.display_name().into(),
        entry.pid.to_string(),
        entry.name.clone(),
        entry.category.display_name().into(),
        if entry.owner_pid == 0 {
            "未确认".into()
        } else {
            format!("{} ({})", entry.owner_name, entry.owner_pid)
        },
        entry.parent_pid.to_string(),
        entry.child_count.to_string(),
        format_duration(process_age_seconds(entry)),
        format_duration(idle_seconds(entry)),
    ]
}

unsafe fn insert_row(table: HWND, row: usize, values: &[String], checked: bool) {
    let mut first = to_wide(&values[0]);
    let mut item: LVITEMW = zeroed();
    item.mask = LVIF_TEXT | LVIF_PARAM;
    item.iItem = row as i32;
    item.iSubItem = 0;
    item.pszText = first.as_mut_ptr();
    item.lParam = row as isize;
    SendMessageW(table, LVM_INSERTITEMW, 0, &item as *const LVITEMW as isize);
    for (column, value) in values.iter().enumerate().skip(1) {
        let mut text = to_wide(value);
        let mut sub_item: LVITEMW = zeroed();
        sub_item.iSubItem = column as i32;
        sub_item.pszText = text.as_mut_ptr();
        SendMessageW(
            table,
            LVM_SETITEMTEXTW,
            row,
            &sub_item as *const LVITEMW as isize,
        );
    }
    set_check_state(table, row, checked);
}

unsafe fn set_check_state(table: HWND, row: usize, checked: bool) {
    let mut item: LVITEMW = zeroed();
    item.stateMask = LVIS_STATEIMAGEMASK;
    item.state = if checked {
        STATE_IMAGE_CHECKED
    } else {
        STATE_IMAGE_UNCHECKED
    };
    SendMessageW(
        table,
        LVM_SETITEMSTATE,
        row,
        &item as *const LVITEMW as isize,
    );
}

unsafe fn is_row_checked(table: HWND, row: usize) -> bool {
    let state = SendMessageW(
        table,
        LVM_GETITEMSTATE,
        row,
        LVIS_STATEIMAGEMASK as isize,
    ) as u32;
    state & LVIS_STATEIMAGEMASK == STATE_IMAGE_CHECKED
}

unsafe fn sync_checks_from_table() {
    with_state_mut(|state| {
        for (row, entry) in state.visible_entries.iter().enumerate() {
            if is_row_checked(state.table, row) {
                state.checked.insert(entry.identity.clone());
            } else {
                state.checked.remove(&entry.identity);
            }
        }
    });
}

unsafe fn check_matching(attention_only: bool, all_visible: bool) {
    TABLE_UPDATING.with(|updating| updating.set(true));
    with_state_mut(|state| {
        for (row, entry) in state.visible_entries.iter().enumerate() {
            let should_check = all_visible
                || (attention_only && entry.learned_owned && is_attention(entry));
            if should_check {
                state.checked.insert(entry.identity.clone());
                set_check_state(state.table, row, true);
            }
        }
    });
    TABLE_UPDATING.with(|updating| updating.set(false));
    update_summary();
}

unsafe fn clear_checks() {
    TABLE_UPDATING.with(|updating| updating.set(true));
    with_state_mut(|state| {
        state.checked.clear();
        for row in 0..state.visible_entries.len() {
            set_check_state(state.table, row, false);
        }
    });
    TABLE_UPDATING.with(|updating| updating.set(false));
    update_summary();
}

unsafe fn show_details() {
    with_state(|state| {
        let selected = SendMessageW(state.table, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
        if selected < 0 {
            return;
        }
        let Some(entry) = state.visible_entries.get(selected as usize) else {
            return;
        };
        let text = to_wide(format!(
            "进程：{}    PID：{}    父 PID：{}    子进程：{}\r\n状态：{}（风险：{}）    类型：{}    已确认归属：{}\r\n归属宿主：{}\r\n进程关系：{}\r\n运行时长：{}    闲置时长：{}\r\n\r\n完整命令行：\r\n{}",
            entry.name,
            entry.pid,
            entry.parent_pid,
            entry.child_count,
            entry.status.display_name(),
            entry.status.risk_name(),
            entry.category.display_name(),
            if entry.learned_owned { "是" } else { "否" },
            if entry.owner_pid == 0 {
                "未确认".into()
            } else {
                format!("{} (PID {})", entry.owner_name, entry.owner_pid)
            },
            if entry.relation_path.is_empty() { "未知" } else { &entry.relation_path },
            format_duration(process_age_seconds(entry)),
            format_duration(idle_seconds(entry)),
            entry.command_line
        ));
        SetWindowTextW(state.details, text.as_ptr());
    });
}

unsafe fn update_summary() {
    with_state(|state| {
        if let Some(error) = &state.scan_error {
            let text = to_wide(format!(
                "扫描失败：{} ｜ 表格显示上次成功快照（{} 项）｜ 后台监控：{}",
                error,
                state.all_entries.len(),
                if watch_running() { "运行中" } else { "未运行" }
            ));
            SetWindowTextW(state.status, text.as_ptr());
            return;
        }
        let total = state.all_entries.len();
        let attention = state.all_entries.iter().filter(|entry| is_attention(entry)).count();
        let actionable = state
            .all_entries
            .iter()
            .filter(|entry| entry.learned_owned && is_attention(entry))
            .count();
        let text = to_wide(format!(
            "全部 {} 项 ｜ 当前显示 {} 项 ｜ 需关注 {} 项（可处理 {}）｜ 已勾选 {} 项 ｜ 后台监控：{}",
            total,
            state.visible_entries.len(),
            attention,
            actionable,
            state.checked.len(),
            if watch_running() { "运行中" } else { "未运行" }
        ));
        SetWindowTextW(state.status, text.as_ptr());
    });
}

unsafe fn terminate_checked(window: HWND) {
    sync_checks_from_table();
    let selected = STATE.with(|slot| {
        let state = slot.borrow();
        let state = state.as_ref()?.borrow();
        Some(
            state
                .all_entries
                .iter()
                .filter(|entry| state.checked.contains(&entry.identity))
                .cloned()
                .collect::<Vec<_>>(),
        )
    });
    let Some(selected) = selected else {
        return;
    };
    if selected.is_empty() {
        message(window, "请先勾选一个或多个进程。", "提示", MB_OK | MB_ICONINFORMATION);
        return;
    }
    let unowned = selected.iter().filter(|entry| !entry.learned_owned).count();
    let active = selected.iter().filter(|entry| entry.status == Status::Owned).count();
    let child_total: usize = selected.iter().map(|entry| entry.child_count).sum();
    let preview = selected
        .iter()
        .take(8)
        .map(|entry| format!("PID {}  {}  {}", entry.pid, entry.name, entry.status.display_name()))
        .collect::<Vec<_>>()
        .join("\r\n");
    let remaining = selected.len().saturating_sub(8);
    let body = format!(
        "⚠️ 危险操作检测！\r\n操作类型：批量结束已勾选的进程树\r\n影响范围：勾选 {} 项，关联子进程约 {} 项；其中正常运行 {} 项、归属未确认 {} 项。\r\n风险评估：可能中断正在执行的浏览器或工具调用；归属未确认项会被安全拒绝；父子重复选择会自动去重。\r\n\r\n{}{}\r\n\r\n请确认是否继续？",
        selected.len(),
        child_total,
        active,
        unowned,
        preview,
        if remaining > 0 { format!("\r\n……另有 {remaining} 项") } else { String::new() }
    );
    if message(window, &body, "确认批量结束", MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }

    let result = STATE.with(|slot| {
        let state = slot.borrow();
        let state = state.as_ref()?.borrow();
        Some(state.guardian.terminate_batch(&selected, true))
    });
    let Some(result) = result else {
        return;
    };
    let failures = result
        .failures
        .iter()
        .take(10)
        .map(|(pid, error)| format!("PID {pid}: {error}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let summary = format!(
        "批量操作完成。\r\n请求：{} 项\r\n实际根进程：{} 项\r\n因父子重叠跳过：{} 项\r\n已结束进程：{} 个\r\n失败：{} 项{}",
        result.requested,
        result.processed_roots,
        result.skipped_overlaps,
        result.terminated_processes,
        result.failures.len(),
        if failures.is_empty() { String::new() } else { format!("\r\n\r\n{failures}") }
    );
    message(
        window,
        &summary,
        "批量操作结果",
        MB_OK | if result.failures.is_empty() { MB_ICONINFORMATION } else { MB_ICONWARNING },
    );
    with_state_mut(|state| state.checked.clear());
    refresh_scan(window);
}

unsafe fn start_watch(window: HWND) {
    if watch_running() {
        message(window, "后台监控已经在运行。", "提示", MB_OK | MB_ICONINFORMATION);
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = to_wide(format!("\"{}\" --watch", exe.display()));
    let mut startup: STARTUPINFOW = zeroed();
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = zeroed();
    let ok = CreateProcessW(
        null(),
        command.as_mut_ptr(),
        null(),
        null(),
        0,
        CREATE_NO_WINDOW,
        null(),
        null(),
        &startup,
        &mut process,
    );
    if ok != 0 {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
        std::thread::sleep(Duration::from_millis(150));
        update_summary();
    }
}

unsafe fn stop_watch(window: HWND) {
    let event_name = to_wide(STOP_EVENT);
    let event = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
    if event.is_null() {
        message(window, "后台监控未运行。", "提示", MB_OK | MB_ICONINFORMATION);
        return;
    }
    if message(
        window,
        "⚠️ 危险操作检测！\r\n操作类型：停止后台监控\r\n影响范围：停止新的遗留进程检测\r\n风险评估：关闭后不再自动扫描。\r\n\r\n请确认是否继续？",
        "确认停止监控",
        MB_YESNO | MB_ICONWARNING,
    ) == IDYES
    {
        SetEvent(event);
        std::thread::sleep(Duration::from_millis(150));
        update_summary();
    }
    CloseHandle(event);
}

unsafe fn watch_running() -> bool {
    let name = to_wide(WATCH_MUTEX);
    let handle = CreateMutexW(null(), 0, name.as_ptr());
    if handle.is_null() {
        return false;
    }
    let already_exists = GetLastError() == 183;
    CloseHandle(handle);
    already_exists
}

unsafe fn window_text(window: HWND) -> String {
    let length = GetWindowTextLengthW(window);
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let read = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
    String::from_utf16_lossy(&buffer[..read.max(0) as usize])
}

fn with_state(function: impl FnOnce(&AppState)) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            function(&state.borrow());
        }
    });
}

fn with_state_mut(function: impl FnOnce(&mut AppState)) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            function(&mut state.borrow_mut());
        }
    });
}

unsafe fn message(window: HWND, body: &str, title: &str, flags: MESSAGEBOX_STYLE) -> i32 {
    let body = to_wide(body);
    let title = to_wide(title);
    MessageBoxW(window, body.as_ptr(), title.as_ptr(), flags)
}
