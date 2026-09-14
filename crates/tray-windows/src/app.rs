use agent_core::ipc::{MAX_FRAME, PIPE_NAME, SessionStatus};
use anyhow::{Result, ensure};
use std::{
    io::{Read, Write},
    mem::{size_of, zeroed},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    Networking::WinInet::*,
    System::{LibraryLoader::*, Pipes::*, Services::*, Threading::*},
    UI::{Shell::*, WindowsAndMessaging::*},
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn copy_text<const N: usize>(out: &mut [u16; N], text: &str) {
    out.fill(0);
    for (a, b) in out.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *a = b;
    }
}
fn check(ok: i32) -> Result<()> {
    ensure!(
        ok != 0,
        "Windows error: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn status() -> Result<SessionStatus> {
    use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(PIPE_NAME)?;
    // Querying a SYSTEM token is not permitted for standard users. Instead,
    // compare the pipe peer with the PID registered by the Service Control
    // Manager, whose service registration is writable only by administrators.
    unsafe {
        struct ServiceHandle(SC_HANDLE);
        impl Drop for ServiceHandle {
            fn drop(&mut self) {
                unsafe {
                    CloseServiceHandle(self.0);
                }
            }
        }
        let mut pid = 0;
        check(GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid))?;
        let manager = ServiceHandle(OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT));
        ensure!(!manager.0.is_null(), "Cannot open service manager");
        let service = ServiceHandle(OpenServiceW(
            manager.0,
            wide("ScreenGuard").as_ptr(),
            SERVICE_QUERY_STATUS,
        ));
        ensure!(!service.0.is_null(), "ScreenGuard is not installed");
        let mut info: SERVICE_STATUS_PROCESS = zeroed();
        let mut bytes = 0;
        check(QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut info as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut bytes,
        ))?;
        ensure!(
            info.dwProcessId == pid && info.dwCurrentState == SERVICE_RUNNING,
            "Pipe peer is not the running ScreenGuard service"
        );
    }
    pipe.write_all(b"status\n")?;
    let mut bytes = Vec::new();
    loop {
        ensure!(bytes.len() < MAX_FRAME, "IPC frame too large");
        let mut b = [0];
        pipe.read_exact(&mut b)?;
        if b[0] == b'\n' {
            break;
        }
        bytes.push(b[0]);
    }
    pipe.write_all(b"\n")?;
    Ok(serde_json::from_slice(&bytes)?)
}

struct WindowState {
    status: Arc<Mutex<SessionStatus>>,
    notification: u64,
    taskbar_created: u32,
}
const STATUS_CHANGED: u32 = WM_APP + 1;
const TRAY_CALLBACK: u32 = WM_APP + 2;
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
        if !ptr.is_null() {
            let state = &mut *ptr;
            if msg == STATUS_CHANGED || msg == state.taskbar_created {
                let current = state.status.lock().unwrap().clone();
                let mut icon: NOTIFYICONDATAW = zeroed();
                icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                icon.hWnd = hwnd;
                icon.uID = 1;
                icon.uCallbackMessage = TRAY_CALLBACK;
                icon.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
                icon.hIcon = LoadIconW(
                    null_mut(),
                    if current.blocked {
                        IDI_ERROR
                    } else {
                        IDI_SHIELD
                    },
                );
                let time = current
                    .remaining_seconds
                    .map(|s| format!("{}h {:02}m", s.max(0) / 3600, s.max(0) / 60 % 60))
                    .unwrap_or_else(|| "—".into());
                let tip = if current.blocked {
                    agent_core::tray_i18n::tooltip_locked(&current.language).to_owned()
                } else if current.remaining_seconds.is_some() {
                    agent_core::tray_i18n::tooltip_remaining(&current.language, &time)
                } else {
                    agent_core::tray_i18n::tooltip_unlimited(&current.language).to_owned()
                };
                let tip = format!(
                    "ScreenGuard: {tip}{}",
                    if current.online { "" } else { " (offline)" }
                );
                copy_text(&mut icon.szTip, &tip);
                if let Some(n) = &current.notification
                    && n.id != state.notification
                {
                    state.notification = n.id;
                    icon.uFlags |= NIF_INFO;
                    icon.dwInfoFlags = NIIF_INFO;
                    copy_text(&mut icon.szInfoTitle, &n.title);
                    copy_text(&mut icon.szInfo, &n.body);
                }
                if msg == state.taskbar_created || Shell_NotifyIconW(NIM_MODIFY, &icon) == 0 {
                    Shell_NotifyIconW(NIM_ADD, &icon);
                }
                if current.blocked {
                    windows_sys::Win32::System::Shutdown::LockWorkStation();
                }
                return 0;
            }
            if msg == TRAY_CALLBACK && (l as u32 == WM_RBUTTONUP || l as u32 == WM_LBUTTONUP) {
                let menu = CreatePopupMenu();
                AppendMenuW(
                    menu,
                    MF_STRING,
                    1,
                    wide("Open ScreenGuard administration").as_ptr(),
                );
                let mut point: POINT = zeroed();
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let selected = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null(),
                );
                DestroyMenu(menu);
                if selected == 1 {
                    let url = state.status.lock().unwrap().admin_url.clone();
                    if url.starts_with("https://") || url.starts_with("http://") {
                        ShellExecuteW(
                            hwnd,
                            wide("open").as_ptr(),
                            wide(&url).as_ptr(),
                            null(),
                            null(),
                            SW_SHOWNORMAL,
                        );
                    }
                }
                return 0;
            }
        }
        if msg == WM_DESTROY {
            let mut icon: NOTIFYICONDATAW = zeroed();
            icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = hwnd;
            icon.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &icon);
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}

// Backup is per-user and durable, so a killed helper or an upgrade can restore
// the original PAC/proxy configuration. It never contains privileged settings.
#[derive(serde::Serialize, serde::Deserialize)]
struct ProxyConfig {
    flags: u32,
    proxy: String,
    bypass: String,
    pac: String,
}
fn backup_path() -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA missing"))?,
    )
    .join("ScreenGuard")
    .join("proxy-backup.json"))
}
fn proxy_options(flags: u32, strings: &mut [Vec<u16>; 3]) -> [INTERNET_PER_CONN_OPTIONW; 4] {
    [
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_FLAGS,
            Value: INTERNET_PER_CONN_OPTIONW_0 { dwValue: flags },
        },
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_PROXY_SERVER,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                pszValue: strings[0].as_mut_ptr(),
            },
        },
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_PROXY_BYPASS,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                pszValue: strings[1].as_mut_ptr(),
            },
        },
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_AUTOCONFIG_URL,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                pszValue: strings[2].as_mut_ptr(),
            },
        },
    ]
}
fn option_list(options: &mut [INTERNET_PER_CONN_OPTIONW; 4]) -> INTERNET_PER_CONN_OPTION_LISTW {
    INTERNET_PER_CONN_OPTION_LISTW {
        dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
        pszConnection: null_mut(),
        dwOptionCount: 4,
        dwOptionError: 0,
        pOptions: options.as_mut_ptr(),
    }
}
fn query_proxy() -> Result<ProxyConfig> {
    unsafe {
        let mut strings = [wide(""), wide(""), wide("")];
        let mut opts = proxy_options(0, &mut strings);
        for opt in &mut opts {
            opt.Value.pszValue = null_mut();
        }
        let mut list = option_list(&mut opts);
        let mut len = list.dwSize;
        check(InternetQueryOptionW(
            null_mut(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&mut list as *mut INTERNET_PER_CONN_OPTION_LISTW).cast(),
            &mut len,
        ))?;
        let mut values = Vec::new();
        for opt in &opts[1..] {
            let p = opt.Value.pszValue;
            let mut n = 0;
            if !p.is_null() {
                while *p.add(n) != 0 {
                    n += 1;
                }
            }
            values.push(if p.is_null() {
                String::new()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
            });
            GlobalFree(p.cast());
        }
        Ok(ProxyConfig {
            flags: opts[0].Value.dwValue,
            proxy: values[0].clone(),
            bypass: values[1].clone(),
            pac: values[2].clone(),
        })
    }
}
fn set_proxy(value: &ProxyConfig) -> Result<()> {
    unsafe {
        let mut strings = [wide(&value.proxy), wide(&value.bypass), wide(&value.pac)];
        let mut opts = proxy_options(value.flags, &mut strings);
        let mut list = option_list(&mut opts);
        check(InternetSetOptionW(
            null_mut(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&mut list as *mut INTERNET_PER_CONN_OPTION_LISTW).cast(),
            list.dwSize,
        ))?;
        InternetSetOptionW(null_mut(), INTERNET_OPTION_SETTINGS_CHANGED, null_mut(), 0);
        InternetSetOptionW(null_mut(), INTERNET_OPTION_REFRESH, null_mut(), 0);
        Ok(())
    }
}
fn apply_proxy(port: Option<u16>) -> Result<()> {
    let path = backup_path()?;
    if let Some(port) = port {
        if !path.exists() {
            let old = query_proxy()?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            let temp = path.with_extension("tmp");
            std::fs::write(&temp, serde_json::to_vec(&old)?)?;
            std::fs::rename(temp, &path)?;
        }
        set_proxy(&ProxyConfig {
            flags: PROXY_TYPE_PROXY,
            proxy: format!("127.0.0.1:{port}"),
            bypass: String::new(),
            pac: String::new(),
        })?;
    } else if path.exists() {
        let old: ProxyConfig = serde_json::from_slice(&std::fs::read(&path)?)?;
        set_proxy(&old)?;
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn run() -> Result<()> {
    unsafe {
        if std::env::args().any(|a| a == "--restore-proxy") {
            return apply_proxy(None);
        }
        // Session-local mutex prevents duplicate helpers after service recovery.
        let mutex = Handle(CreateMutexW(
            null(),
            1,
            wide(r"Local\ScreenGuard.SessionHelper").as_ptr(),
        ));
        if mutex.0.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        let instance = GetModuleHandleW(null());
        let class_name = wide("ScreenGuardTray");
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(wndproc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        ensure!(RegisterClassW(&class) != 0, "RegisterClass failed");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("ScreenGuard").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        ensure!(!hwnd.is_null(), "CreateWindow failed");
        let shared = Arc::new(Mutex::new(SessionStatus::default()));
        let state = Box::into_raw(Box::new(WindowState {
            status: shared.clone(),
            notification: 0,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        let hwnd_value = hwnd as usize;
        std::thread::spawn(move || {
            let mut failures = 0;
            let mut port = None;
            loop {
                match status() {
                    Ok(value) => {
                        failures = 0;
                        if (value.proxy_port != port || value.proxy_port.is_some())
                            && apply_proxy(value.proxy_port).is_ok()
                        {
                            port = value.proxy_port;
                        }
                        // Handle durable backup from a previous helper even if both
                        // the new and old in-memory port values are None.
                        if value.proxy_port.is_none() {
                            let _ = apply_proxy(None);
                        }
                        *shared.lock().unwrap() = value;
                    }
                    Err(_) => {
                        failures += 1;
                        if failures >= 5 {
                            let _ = apply_proxy(None);
                            port = None;
                            *shared.lock().unwrap() = SessionStatus::default();
                        }
                    }
                }
                PostMessageW(hwnd_value as HWND, STATUS_CHANGED, 0, 0);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(state));
    }
    Ok(())
}
