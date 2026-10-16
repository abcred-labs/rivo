use super::{
    validate_action, ComputerAction, ComputerBackend, ComputerError, MouseButton, Screenshot,
    UiNode, WindowInfo, MAX_SCREENSHOT_BYTES,
};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetActiveWindow, SetFocus, INPUT, INPUT_0, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow,
    GetSystemMetrics, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SetCursorPos, SetForegroundWindow, SetWindowPos, ShowWindow, SwitchToThisWindow, ASFW_ANY,
    HWND_TOP, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
};

#[derive(Debug, Default)]
pub struct WindowsComputerBackend;

#[async_trait]
impl ComputerBackend for WindowsComputerBackend {
    async fn execute(&self, action: ComputerAction) -> Result<Value, ComputerError> {
        validate_action(&action)?;
        match action {
            ComputerAction::ListWindows => serde_json::to_value(list_windows())
                .map_err(|e| ComputerError::Backend(e.to_string())),
            ComputerAction::FocusWindow { window_id } => {
                focus_window(&window_id)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::MouseMove { x, y } => {
                guard_sensitive_foreground()?;
                mouse_move(x, y)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::UiSnapshot {
                window_id,
                max_nodes,
            } => serde_json::to_value(ui_snapshot(&window_id, max_nodes)?)
                .map_err(|e| ComputerError::Backend(e.to_string())),
            ComputerAction::Screenshot { window_id } => {
                serde_json::to_value(capture(window_id.as_deref())?)
                    .map_err(|e| ComputerError::Backend(e.to_string()))
            }
            ComputerAction::Click { x, y, button } => {
                guard_sensitive_foreground()?;
                click(x, y, button, false)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::DoubleClick { x, y, button } => {
                guard_sensitive_foreground()?;
                click(x, y, button, true)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                guard_sensitive_foreground()?;
                drag_with(from_x, from_y, to_x, to_y, |event| match event {
                    DragEvent::Move(x, y) => mouse_move(x, y),
                    DragEvent::Button(down) => mouse_button(MouseButton::Left, down),
                })?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::Scroll { x, y, delta } => {
                guard_sensitive_foreground()?;
                mouse_move(x, y)?;
                send_mouse(MOUSEEVENTF_WHEEL, delta)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::Type { text } => {
                guard_sensitive_foreground()?;
                type_text(&text)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::Key { key, modifiers } => {
                guard_sensitive_foreground()?;
                send_key(&key, &modifiers)?;
                Ok(json!({"ok": true}))
            }
            ComputerAction::Wait { timeout_ms } => {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms));
                Ok(json!({"waited_ms": timeout_ms}))
            }
        }
    }
}

fn ui_snapshot(window_id: &str, max_nodes: usize) -> Result<Value, ComputerError> {
    let hwnd = parse_hwnd(window_id)?;
    let snapshot = uia_snapshot_com(hwnd, window_id, max_nodes)?;
    Ok(json!(snapshot))
}

const MAX_UIA_DEPTH: usize = 16;
const MAX_UIA_TEXT_CHARS: usize = 512;

#[cfg(windows)]
fn uia_snapshot_com(hwnd: HWND, window_id: &str, max_nodes: usize) -> Result<Value, ComputerError> {
    use windows::Win32::Foundation::HWND as WindowsHwnd;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};

    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if !init.is_ok() && init.0 != -2147417850 {
        return Err(ComputerError::Backend(format!(
            "UI Automation COM initialization failed (HRESULT 0x{:08x})",
            init.0 as u32
        )));
    }
    let uninitialize = init.is_ok();
    let result = (|| {
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|error| {
                ComputerError::Backend(format!("UI Automation unavailable: {error}"))
            })?
        };
        let root = unsafe {
            automation
                .ElementFromHandle(WindowsHwnd(hwnd))
                .map_err(|error| {
                    ComputerError::Backend(format!("UI Automation root unavailable: {error}"))
                })?
        };
        let walker = unsafe {
            automation.ControlViewWalker().map_err(|error| {
                ComputerError::Backend(format!("UI Automation tree walker unavailable: {error}"))
            })?
        };
        let mut nodes = Vec::with_capacity(max_nodes.min(64));
        let mut pending: Vec<(IUIAutomationElement, usize, String)> =
            vec![(root, 0, format!("{window_id}:0"))];
        let mut truncated = false;
        while let Some((element, depth, runtime_id)) = pending.pop() {
            if nodes.len() >= max_nodes {
                truncated = true;
                break;
            }
            nodes.push(uia_node_from_element(&element, runtime_id.clone()));
            if depth >= MAX_UIA_DEPTH {
                truncated = true;
                continue;
            }
            let mut children = Vec::new();
            let mut child = unsafe { walker.GetFirstChildElement(&element).ok() };
            while let Some(element) = child {
                if nodes.len() + pending.len() + children.len() >= max_nodes {
                    truncated = true;
                    break;
                }
                children.push(element.clone());
                child = unsafe { walker.GetNextSiblingElement(&element).ok() };
            }
            for (index, child) in children.into_iter().enumerate().rev() {
                pending.push((child, depth + 1, format!("{runtime_id}/{index}")));
            }
        }
        Ok(json!({"nodes": nodes, "truncated": truncated}))
    })();
    if uninitialize {
        unsafe { CoUninitialize() };
    }
    result
}

#[cfg(not(windows))]
fn uia_snapshot_com(
    _hwnd: HWND,
    _window_id: &str,
    _max_nodes: usize,
) -> Result<Value, ComputerError> {
    Err(ComputerError::Unsupported)
}

#[cfg(windows)]
fn uia_node_from_element(
    element: &windows::Win32::UI::Accessibility::IUIAutomationElement,
    runtime_id: String,
) -> UiNode {
    use windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID;
    let name = unsafe { element.CurrentName().ok() }
        .and_then(|value| String::try_from(value).ok())
        .map(|value| truncate_uia_text(&value))
        .unwrap_or_default();
    let control_type = unsafe { element.CurrentControlType().ok() }
        .map(|value: UIA_CONTROLTYPE_ID| control_type_name(value.0))
        .unwrap_or("unknown")
        .to_owned();
    let enabled = unsafe { element.CurrentIsEnabled().ok() }
        .map(|value| value == true)
        .unwrap_or(true);
    let password = unsafe { element.CurrentIsPassword().ok() }
        .map(|value| value == true)
        .unwrap_or(false);
    let bounds = unsafe { element.CurrentBoundingRectangle().ok() }
        .map(|rect| [rect.left, rect.top, rect.right, rect.bottom])
        .unwrap_or([0, 0, 0, 0]);
    UiNode {
        runtime_id,
        name,
        control_type,
        bounds,
        enabled,
        password,
    }
}

fn truncate_uia_text(value: &str) -> String {
    value.chars().take(MAX_UIA_TEXT_CHARS).collect()
}

fn control_type_name(value: i32) -> &'static str {
    match value {
        50000 => "button",
        50002 => "calendar",
        50004 => "edit",
        50014 => "window",
        50020 => "text",
        _ => "unknown",
    }
}

fn list_windows() -> Vec<WindowInfo> {
    unsafe extern "system" fn callback(hwnd: HWND, data: LPARAM) -> i32 {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut title = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        if len == 0 {
            return 1;
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return 1;
        }
        let mut class_name = [0u16; 256];
        let class_len =
            unsafe { GetClassNameW(hwnd, class_name.as_mut_ptr(), class_name.len() as i32) };
        let mut process_id = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut process_id);
        }
        let title_text = String::from_utf16_lossy(&title[..len as usize]);
        let class_text = String::from_utf16_lossy(&class_name[..class_len.max(0) as usize]);
        let sensitive = is_sensitive_window(&title_text, &class_text);
        let windows = unsafe { &mut *(data as *mut Vec<WindowInfo>) };
        windows.push(WindowInfo {
            id: format!("0x{:x}", hwnd as usize),
            title: title_text,
            process: process_name(process_id),
            process_id,
            class_name: class_text,
            sensitive,
            bounds: [rect.left, rect.top, rect.right, rect.bottom],
            foreground: hwnd == unsafe { GetForegroundWindow() },
        });
        1
    }
    let mut windows = Vec::new();
    unsafe {
        EnumWindows(Some(callback), &mut windows as *mut _ as LPARAM);
    }
    windows.truncate(100);
    windows
}

fn process_name(process_id: u32) -> String {
    if process_id == 0 {
        return "unknown".into();
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return "unknown".into();
    }
    let mut path = vec![0u16; 32_768];
    let mut length = path.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) } != 0;
    unsafe {
        CloseHandle(process);
    }
    if !ok || length == 0 || length as usize > path.len() {
        return "unknown".into();
    }
    String::from_utf16_lossy(&path[..length as usize])
        .rsplit(['\\', '/'])
        .find(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

fn is_sensitive_window(title: &str, class_name: &str) -> bool {
    let haystack = format!("{title} {class_name}").to_ascii_lowercase();
    [
        "password",
        "passwd",
        "credential",
        "sign in",
        "log in",
        "login",
        "uac",
        "secure desktop",
        "安全桌面",
        "two-factor",
        "verification code",
        "验证码",
        "密码",
    ]
    .iter()
    .any(|marker| haystack.contains(marker))
}

fn guard_sensitive_foreground() -> Result<(), ComputerError> {
    if secure_desktop_active() {
        return Err(ComputerError::Denied(
            "当前输入桌面是安全桌面或无法验证，桌面输入已拒绝".into(),
        ));
    }
    if foreground_is_high_integrity_or_unknown() {
        return Err(ComputerError::Denied(
            "无法确认前台进程完整性级别或其为高完整性进程，桌面输入已拒绝".into(),
        ));
    }
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return Err(ComputerError::Denied("没有可验证的前台窗口".into()));
    }
    let mut title = [0u16; 512];
    let title_len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
    let mut class_name = [0u16; 256];
    let class_len =
        unsafe { GetClassNameW(hwnd, class_name.as_mut_ptr(), class_name.len() as i32) };
    let title = String::from_utf16_lossy(&title[..title_len.max(0) as usize]);
    let class_name = String::from_utf16_lossy(&class_name[..class_len.max(0) as usize]);
    if is_sensitive_window(&title, &class_name) {
        return Err(ComputerError::Denied(
            "前台窗口可能包含密码、凭据或安全桌面内容，桌面输入已拒绝".into(),
        ));
    }
    if focused_control_is_password() {
        return Err(ComputerError::Denied(
            "前台控件标记为密码输入，桌面输入已拒绝".into(),
        ));
    }
    Ok(())
}

fn secure_desktop_active() -> bool {
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return true;
    }
    let mut name = [0u16; 128];
    let mut required = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr() as *mut core::ffi::c_void,
            (name.len() * std::mem::size_of::<u16>()) as u32,
            &mut required,
        ) != 0
    };
    unsafe {
        CloseDesktop(desktop);
    }
    if !ok {
        return true;
    }
    let length = name
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(name.len());
    let name = String::from_utf16_lossy(&name[..length]).to_ascii_lowercase();
    name.contains("secure") || name.contains("winlogon")
}

fn foreground_is_high_integrity_or_unknown() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return true;
    }
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    if process_id == 0 {
        return true;
    }
    let process = unsafe {
        windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            process_id,
        )
    };
    if process.is_null() {
        return true;
    }
    let mut token = std::ptr::null_mut();
    let opened = unsafe {
        windows_sys::Win32::System::Threading::OpenProcessToken(process, TOKEN_QUERY, &mut token)
    } != 0;
    unsafe {
        CloseHandle(process);
    }
    if !opened || token.is_null() {
        return true;
    }
    let mut required = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        unsafe { CloseHandle(token) };
        return true;
    }
    let mut buffer = vec![0u8; required as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            required,
            &mut required,
        ) != 0
    };
    let high = if ok {
        let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
        if label.Label.Sid.is_null() {
            true
        } else {
            let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
            if count == 0 {
                true
            } else {
                unsafe { *GetSidSubAuthority(label.Label.Sid, (count - 1) as u32) >= 0x3000 }
            }
        }
    } else {
        true
    };
    unsafe {
        CloseHandle(token);
    }
    high
}

#[cfg(windows)]
fn focused_control_is_password() -> bool {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if !init.is_ok() && init.0 != -2147417850 {
        return true;
    }
    let uninitialize = init.is_ok();
    let result = (|| {
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()? };
        let focused = unsafe { automation.GetFocusedElement().ok()? };
        Some(unsafe { focused.CurrentIsPassword().ok()? == true })
    })()
    .unwrap_or(true);
    if uninitialize {
        unsafe { CoUninitialize() };
    }
    result
}

#[cfg(not(windows))]
fn focused_control_is_password() -> bool {
    true
}

fn parse_hwnd(id: &str) -> Result<HWND, ComputerError> {
    let value = id.strip_prefix("0x").unwrap_or(id);
    usize::from_str_radix(value, if id.starts_with("0x") { 16 } else { 10 })
        .map(|value| value as HWND)
        .map_err(|_| ComputerError::Invalid("invalid window id".into()))
}
fn focus_window(id: &str) -> Result<(), ComputerError> {
    let hwnd = parse_hwnd(id)?;
    let current_thread = unsafe { GetCurrentThreadId() };
    let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
    let attached_target = target_thread != 0
        && target_thread != current_thread
        && unsafe { AttachThreadInput(current_thread, target_thread, 1) } != 0;
    let attached_foreground = foreground_thread != 0
        && foreground_thread != current_thread
        && foreground_thread != target_thread
        && unsafe { AttachThreadInput(current_thread, foreground_thread, 1) } != 0;
    let mut focused = false;
    let mut actually_foreground = false;
    // Windows may defer foreground changes while another process owns the
    // foreground lock. Retry briefly after the normal activation sequence so
    // a just-launched application does not fail nondeterministically.
    for _ in 0..5 {
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
            ShowWindow(hwnd, SW_RESTORE);
            BringWindowToTop(hwnd);
            SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            SwitchToThisWindow(hwnd, 1);
            focused |= SetForegroundWindow(hwnd) != 0;
            SetActiveWindow(hwnd);
            SetFocus(hwnd);
            actually_foreground = GetForegroundWindow() == hwnd;
        }
        if actually_foreground {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if attached_target {
        unsafe {
            AttachThreadInput(current_thread, target_thread, 0);
        }
    }
    if attached_foreground {
        unsafe {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
    }
    if !focused && !actually_foreground {
        Err(ComputerError::Denied("cannot focus window".into()))
    } else {
        Ok(())
    }
}
fn mouse_move(x: i32, y: i32) -> Result<(), ComputerError> {
    if unsafe { SetCursorPos(x, y) } == 0 {
        Err(ComputerError::Backend("SetCursorPos failed".into()))
    } else {
        Ok(())
    }
}
fn mouse_button(button: MouseButton, down: bool) -> Result<(), ComputerError> {
    let flags = match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };
    send_mouse(flags, 0)
}
fn send_mouse(flags: u32, data: i32) -> Result<(), ComputerError> {
    let input = INPUT {
        r#type: 0,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    if unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) } != 1 {
        Err(ComputerError::Backend("SendInput failed".into()))
    } else {
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
enum DragEvent {
    Move(i32, i32),
    Button(bool),
}

fn drag_with(
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    mut send: impl FnMut(DragEvent) -> Result<(), ComputerError>,
) -> Result<(), ComputerError> {
    send(DragEvent::Move(from_x, from_y))?;
    send(DragEvent::Button(true))?;
    std::thread::sleep(std::time::Duration::from_millis(30));
    let moved = send(DragEvent::Move(to_x, to_y));
    std::thread::sleep(std::time::Duration::from_millis(30));
    // Release even when the destination move fails; do not leave a held button.
    let released = send(DragEvent::Button(false));
    moved.and(released)
}

fn click(x: i32, y: i32, button: MouseButton, twice: bool) -> Result<(), ComputerError> {
    mouse_move(x, y)?;
    mouse_button(button, true)?;
    mouse_button(button, false)?;
    if twice {
        mouse_button(button, true)?;
        mouse_button(button, false)?;
    }
    Ok(())
}
fn type_text(text: &str) -> Result<(), ComputerError> {
    for unit in text.encode_utf16() {
        let down = INPUT {
            r#type: 1,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up = INPUT {
            r#type: 1,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        if unsafe { SendInput(1, &down, std::mem::size_of::<INPUT>() as i32) } != 1
            || unsafe { SendInput(1, &up, std::mem::size_of::<INPUT>() as i32) } != 1
        {
            return Err(ComputerError::Backend("SendInput text failed".into()));
        }
    }
    Ok(())
}
fn send_key(key: &str, modifiers: &[String]) -> Result<(), ComputerError> {
    let mut modifier_codes = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        modifier_codes.push(vk_code(modifier).ok_or_else(|| {
            ComputerError::Invalid(format!("unsupported key modifier: {modifier}"))
        })?);
    }
    let key_code = vk_code(key);
    for code in &modifier_codes {
        send_vk(*code, false)?;
    }
    if let Some(code) = key_code {
        send_vk(code, false)?;
        send_vk(code, true)?;
    } else if modifiers.is_empty() && key.chars().count() == 1 {
        type_text(key)?;
    } else {
        return Err(ComputerError::Invalid(format!("unsupported key: {key}")));
    }
    for code in modifier_codes.iter().rev() {
        send_vk(*code, true)?;
    }
    Ok(())
}

fn vk_code(key: &str) -> Option<u16> {
    // Provider payloads use several equivalent spellings (for example
    // `ARROWUP`, `arrow_up`, `CMD`, and `CTRL`). Normalize separators and
    // case here so the backend remains protocol-agnostic.
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    match normalized.as_str() {
        "shift" => Some(0x10),
        "ctrl" | "control" => Some(0x11),
        "alt" | "option" => Some(0x12),
        "win" | "meta" | "command" | "cmd" | "super" => Some(0x5b),
        "enter" | "return" => Some(0x0d),
        "tab" => Some(0x09),
        "escape" | "esc" => Some(0x1b),
        "backspace" => Some(0x08),
        "delete" | "del" => Some(0x2e),
        "insert" | "ins" => Some(0x2d),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" | "pgup" => Some(0x21),
        "pagedown" | "pgdn" => Some(0x22),
        "left" | "arrowleft" => Some(0x25),
        "up" | "arrowup" => Some(0x26),
        "right" | "arrowright" => Some(0x27),
        "down" | "arrowdown" => Some(0x28),
        "space" => Some(0x20),
        "capslock" => Some(0x14),
        "numlock" => Some(0x90),
        "scrolllock" => Some(0x91),
        "printscreen" | "prtsc" => Some(0x2c),
        "pause" | "break" => Some(0x13),
        "f1" => Some(0x70),
        "f2" => Some(0x71),
        "f3" => Some(0x72),
        "f4" => Some(0x73),
        "f5" => Some(0x74),
        "f6" => Some(0x75),
        "f7" => Some(0x76),
        "f8" => Some(0x77),
        "f9" => Some(0x78),
        "f10" => Some(0x79),
        "f11" => Some(0x7a),
        "f12" => Some(0x7b),
        "f13" => Some(0x7c),
        "f14" => Some(0x7d),
        "f15" => Some(0x7e),
        "f16" => Some(0x7f),
        "f17" => Some(0x80),
        "f18" => Some(0x81),
        "f19" => Some(0x82),
        "f20" => Some(0x83),
        "f21" => Some(0x84),
        "f22" => Some(0x85),
        "f23" => Some(0x86),
        "f24" => Some(0x87),
        _ => {
            let bytes = normalized.as_bytes();
            (bytes.len() == 1 && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()))
                .then_some(bytes[0] as u16)
        }
    }
}

fn send_vk(code: u16, up: bool) -> Result<(), ComputerError> {
    let input = INPUT {
        r#type: 1,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: code,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    if unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) } != 1 {
        Err(ComputerError::Backend("SendInput key failed".into()))
    } else {
        Ok(())
    }
}

fn capture(window_id: Option<&str>) -> Result<Screenshot, ComputerError> {
    let hwnd = window_id
        .map(parse_hwnd)
        .transpose()?
        .unwrap_or(std::ptr::null_mut());
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if !hwnd.is_null() && unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(ComputerError::Backend("GetWindowRect failed".into()));
    }
    let (x, y, width, height) = if hwnd.is_null() {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        }
    } else {
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    };
    if width <= 0 || height <= 0 {
        return Err(ComputerError::Backend(
            "screenshot target has empty bounds".into(),
        ));
    }
    let pixel_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| ComputerError::Invalid("screenshot dimensions overflow".into()))?;
    // The public limit applies to the encoded observation. A raw 1920x1080
    // BGRA buffer is slightly larger than 8 MiB even when the PNG is small;
    // keep a separate bounded working-buffer limit so ordinary displays work.
    if pixel_bytes > 64 * 1024 * 1024 {
        return Err(ComputerError::Denied(
            "screenshot exceeds 8 MiB limit".into(),
        ));
    }
    // Window coordinates returned by GetWindowRect are desktop coordinates.
    // Capture them from the desktop DC; using GetDC(hwnd) with those coordinates
    // reads outside the window's client surface and produces an all-black PNG.
    let desktop = std::ptr::null_mut();
    let hdc = unsafe { GetDC(desktop) };
    if hdc.is_null() {
        return Err(ComputerError::Backend("GetDC desktop failed".into()));
    }
    let mem = unsafe { CreateCompatibleDC(hdc) };
    let bitmap = unsafe { CreateCompatibleBitmap(hdc, width, height) };
    if mem.is_null() || bitmap.is_null() {
        unsafe {
            if !mem.is_null() {
                DeleteDC(mem);
            }
            if !bitmap.is_null() {
                DeleteObject(bitmap as _);
            }
            ReleaseDC(desktop, hdc);
        }
        return Err(ComputerError::Backend("GDI allocation failed".into()));
    }
    unsafe {
        SelectObject(mem, bitmap as _);
        let captured = if hwnd.is_null() {
            BitBlt(mem, 0, 0, width, height, hdc, x, y, SRCCOPY)
        } else if GetForegroundWindow() == hwnd {
            // The visible desktop is the most faithful source for the active
            // browser window, including GPU-composited page contents.
            BitBlt(mem, 0, 0, width, height, hdc, x, y, SRCCOPY)
        } else if PrintWindow(hwnd, mem, 2) != 0 {
            1
        } else {
            0
        };
        if captured == 0 {
            DeleteObject(bitmap as _);
            DeleteDC(mem);
            ReleaseDC(desktop, hdc);
            return Err(ComputerError::Backend(
                "window rendering failed; target was not foreground for desktop capture".into(),
            ));
        }
    }
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut info = BITMAPINFO {
        bmiHeader: header,
        bmiColors: [Default::default()],
    };
    let rows = unsafe {
        GetDIBits(
            mem,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr() as _,
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        DeleteObject(bitmap as _);
        DeleteDC(mem);
        ReleaseDC(desktop, hdc);
    }
    if rows == 0 {
        return Err(ComputerError::Backend("GetDIBits failed".into()));
    }
    if pixels
        .chunks_exact(4)
        .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
    {
        return Err(ComputerError::Backend(
            "screenshot contains only black pixels".into(),
        ));
    }
    // GDI returns BGRA; convert to RGBA before handing the image to Provider
    // adapters. PNG is accepted by the native Computer Use wire formats and
    // avoids exposing a Windows-specific BMP assumption at the boundary.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ComputerError::Backend(format!("PNG header failed: {error}")))?;
        writer
            .write_image_data(&pixels)
            .map_err(|error| ComputerError::Backend(format!("PNG encode failed: {error}")))?;
    }
    if encoded.len() > MAX_SCREENSHOT_BYTES {
        return Err(ComputerError::Denied(
            "encoded screenshot exceeds 8 MiB limit".into(),
        ));
    }
    Ok(Screenshot {
        media_type: "image/png".into(),
        bytes_base64: base64::engine::general_purpose::STANDARD.encode(encoded),
        width: width as u32,
        height: height as u32,
        window_id: window_id.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn live_fixture_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn drag_holds_button_until_destination_and_releases_on_failure() {
        let mut events = Vec::new();
        drag_with(1, 2, 3, 4, |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            events,
            vec![
                DragEvent::Move(1, 2),
                DragEvent::Button(true),
                DragEvent::Move(3, 4),
                DragEvent::Button(false)
            ]
        );
        let mut events = Vec::new();
        let result = drag_with(1, 2, 3, 4, |event| {
            let failed = event == DragEvent::Move(3, 4);
            events.push(event);
            if failed {
                Err(ComputerError::Backend("destination failed".into()))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(events.last(), Some(&DragEvent::Button(false)));
        let mut events = Vec::new();
        assert!(drag_with(1, 2, 3, 4, |event| {
            let failed = event == DragEvent::Button(true);
            events.push(event);
            if failed {
                Err(ComputerError::Backend("press failed".into()))
            } else {
                Ok(())
            }
        })
        .is_err());
        assert_eq!(events, vec![DragEvent::Move(1, 2), DragEvent::Button(true)]);
    }

    #[test]
    fn key_aliases_cover_common_provider_spellings() {
        assert_eq!(vk_code("ARROW_UP"), Some(0x26));
        assert_eq!(vk_code("ctrl"), Some(0x11));
        assert_eq!(vk_code("CMD"), Some(0x5b));
        assert_eq!(vk_code("page-down"), Some(0x22));
        assert_eq!(vk_code("Insert"), Some(0x2d));
        assert_eq!(vk_code("PrintScreen"), Some(0x2c));
        assert_eq!(vk_code("F24"), Some(0x87));
    }

    #[test]
    fn native_backend_can_enumerate_visible_windows_without_input() {
        let windows = list_windows();
        assert!(windows.len() <= 100);
        assert!(windows.iter().all(|window| !window.id.is_empty()));
        assert_ne!(process_name(std::process::id()), "unknown");
    }

    #[test]
    fn native_backend_screenshot_is_png_or_structured_failure() {
        let windows = list_windows();
        let result = windows
            .first()
            .map(|window| capture(Some(&window.id)))
            .unwrap_or_else(|| capture(None));
        match result {
            Ok(screenshot) => {
                assert_eq!(screenshot.media_type, "image/png");
                assert!(!screenshot.bytes_base64.is_empty());
            }
            Err(ComputerError::Backend(_) | ComputerError::Denied(_)) => {}
            Err(error) => panic!("unexpected screenshot error: {error}"),
        }
    }

    #[test]
    fn native_backend_uia_root_snapshot_is_bounded_or_structured_failure() {
        let windows = list_windows();
        let Some(window) = windows.first() else {
            return;
        };
        match ui_snapshot(&window.id, 10) {
            Ok(value) => {
                assert!(value["nodes"].as_array().is_some());
                assert!(value["nodes"].as_array().unwrap().len() <= 10);
            }
            Err(ComputerError::Backend(_) | ComputerError::Denied(_)) => {}
            Err(error) => panic!("unexpected UIA error: {error}"),
        }
    }

    #[test]
    fn sensitive_window_markers_are_fail_closed_for_input() {
        assert!(is_sensitive_window("Enter password", "Edit"));
        assert!(is_sensitive_window("安全桌面", "#32770"));
        assert!(!is_sensitive_window("Untitled - Notepad", "Notepad"));
    }

    #[test]
    #[ignore = "real desktop fixture; run explicitly with RIVO_COMPUTER_LIVE_FIXTURE=1"]
    fn live_notepad_fixture_executes_guarded_input_and_png_observation() {
        if std::env::var("RIVO_COMPUTER_LIVE_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        let _guard = live_fixture_lock()
            .lock()
            .expect("live fixture lock poisoned");
        let before = list_windows()
            .into_iter()
            .map(|window| window.id)
            .collect::<std::collections::HashSet<_>>();
        let mut child = std::process::Command::new("notepad.exe")
            .spawn()
            .expect("notepad.exe must be available on Windows preview host");
        let runtime = tokio::runtime::Runtime::new().expect("fixture runtime");
        let result = runtime.block_on(async {
            let window = {
                let mut found = None;
                for _ in 0..15 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    found = list_windows().into_iter().find(|window| {
                        !before.contains(&window.id)
                            && window.title.to_ascii_lowercase().contains("notepad")
                    });
                    if found.is_some() {
                        break;
                    }
                }
                found.ok_or_else(|| {
                    ComputerError::Backend(format!(
                        "new notepad window for pid {} not found",
                        child.id()
                    ))
                })?
            };
            let backend = WindowsComputerBackend;
            backend
                .execute(ComputerAction::FocusWindow {
                    window_id: window.id.clone(),
                })
                .await?;
            backend
                .execute(ComputerAction::Type {
                    text: "Rivo Computer Use fixture".into(),
                })
                .await?;
            let hwnd = parse_hwnd(&window.id)?;
            let mut before_drag = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut before_drag) } == 0 {
                return Err(ComputerError::Backend(
                    "fixture GetWindowRect failed".into(),
                ));
            }
            let mut caption = None;
            for y_offset in [12, 24, 36, 48] {
                for x_offset in [80, 160, 240, 320, 400] {
                    let x = before_drag.left + x_offset;
                    let y = before_drag.top + y_offset;
                    let point = ((y as u32 & 0xffff) << 16) | (x as u32 & 0xffff);
                    if unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
                            hwnd,
                            0x0084,
                            0,
                            point as isize,
                        )
                    } == 2
                    {
                        caption = Some((x, y));
                        break;
                    }
                }
                if caption.is_some() {
                    break;
                }
            }
            let (from_x, from_y) = caption.ok_or_else(|| {
                ComputerError::Backend("fixture has no draggable titlebar".into())
            })?;
            backend
                .execute(ComputerAction::Drag {
                    from_x,
                    from_y,
                    to_x: from_x + 60,
                    to_y: from_y + 40,
                })
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let mut after_drag = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut after_drag) } == 0
                || (after_drag.left == before_drag.left && after_drag.top == before_drag.top)
            {
                return Err(ComputerError::Backend(
                    "fixture drag did not move the Notepad window".into(),
                ));
            }
            println!(
                "Notepad titlebar drag moved window by {},{}",
                after_drag.left - before_drag.left,
                after_drag.top - before_drag.top
            );
            let screenshot = backend
                .execute(ComputerAction::Screenshot {
                    window_id: Some(window.id),
                })
                .await?;
            if screenshot["media_type"] != "image/png"
                || screenshot["bytes_base64"]
                    .as_str()
                    .is_none_or(str::is_empty)
            {
                return Err(ComputerError::Backend(
                    "notepad fixture did not return PNG screenshot".into(),
                ));
            }
            backend
                .execute(ComputerAction::Key {
                    key: "F4".into(),
                    modifiers: vec!["alt".into()],
                })
                .await?;
            Ok::<(), ComputerError>(())
        });
        let _ = child.kill();
        let _ = child.wait();
        result.expect("live notepad Computer Use fixture");
    }

    #[test]
    #[ignore = "real desktop fixture; run explicitly with RIVO_COMPUTER_LIVE_FIXTURE=1"]
    fn live_notepad_fixture_reports_bounded_uia_tree() {
        if std::env::var("RIVO_COMPUTER_LIVE_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        let _guard = live_fixture_lock()
            .lock()
            .expect("live fixture lock poisoned");
        let before = list_windows()
            .into_iter()
            .map(|window| window.id)
            .collect::<std::collections::HashSet<_>>();
        let mut child = std::process::Command::new("notepad.exe")
            .spawn()
            .expect("notepad.exe must be available on Windows preview host");
        let runtime = tokio::runtime::Runtime::new().expect("fixture runtime");
        let result = runtime.block_on(async {
            let window = {
                let mut found = None;
                for _ in 0..15 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    found = list_windows().into_iter().find(|window| {
                        !before.contains(&window.id)
                            && window.title.to_ascii_lowercase().contains("notepad")
                    });
                    if found.is_some() {
                        break;
                    }
                }
                found
                    .ok_or_else(|| ComputerError::Backend("new notepad window not found".into()))?
            };
            let value = ui_snapshot(&window.id, 64)?;
            let nodes = value["nodes"]
                .as_array()
                .ok_or_else(|| ComputerError::Backend("UIA nodes are not an array".into()))?;
            assert!(
                !nodes.is_empty(),
                "UIA snapshot must include the root element"
            );
            assert!(nodes.len() <= 64, "UIA snapshot exceeded max_nodes");
            assert!(value["truncated"].is_boolean());
            println!("notepad UIA nodes: {}", nodes.len());
            Ok::<(), ComputerError>(())
        });
        let _ = child.kill();
        let _ = child.wait();
        result.expect("live notepad UIA fixture");
    }
}
