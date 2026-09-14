//! Small, owned Win32 boundary. Raw buffers are freed by the allocator that
//! supplied them; tokens/process handles never cross the helper IPC boundary.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    net::SocketAddr,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    NetworkManagement::{IpHelper::*, NetManagement::*},
    Networking::WinSock::AF_INET,
    Security::Authorization::*,
    Security::*,
    System::{Environment::*, Pipes::*, RemoteDesktop::*, Threading::*},
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn check(ok: i32) -> Result<()> {
    if ok == 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}
pub struct Handle(pub HANDLE);
// Owned kernel handles may be moved between worker threads.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub struct LocalAlloc(pub *mut c_void);
impl Drop for LocalAlloc {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
pub unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}
pub fn sid_string(sid: PSID) -> Result<String> {
    unsafe {
        let mut ptr = null_mut();
        check(ConvertSidToStringSidW(sid, &mut ptr))?;
        let _guard = LocalAlloc(ptr.cast());
        Ok(from_wide(ptr))
    }
}
pub fn token_sid(token: HANDLE) -> Result<String> {
    unsafe {
        let mut len = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        ensure!(len > 0, "No token user");
        let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        check(GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        ))?;
        sid_string((*(buf.as_ptr().cast::<TOKEN_USER>())).User.Sid)
    }
}
pub fn process_sid(pid: u32) -> Result<String> {
    unsafe {
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        ensure!(!process.0.is_null(), "Cannot inspect process");
        let mut token = null_mut();
        check(OpenProcessToken(process.0, TOKEN_QUERY, &mut token))?;
        let token = Handle(token);
        token_sid(token.0)
    }
}
pub fn require_admin() -> Result<()> {
    unsafe {
        let mut token = null_mut();
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        let token = Handle(token);
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut len = 0;
        check(GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        ))?;
        ensure!(
            elevation.TokenIsElevated != 0,
            "Run with administrator rights"
        );
        Ok(())
    }
}
pub fn ensure_service_stopped() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if let Ok(svc) = mgr.open_service("ScreenGuard", ServiceAccess::QUERY_STATUS) {
        ensure!(
            svc.query_status()?.current_state == ServiceState::Stopped,
            "Stop ScreenGuard before resetting pairing"
        );
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Account {
    pub sid: String,
    pub name: String,
    pub display_name: String,
}
pub fn accounts() -> Result<Vec<Account>> {
    unsafe {
        let mut result = Vec::new();
        let mut resume = 0;
        loop {
            let mut buf = null_mut();
            let mut count = 0;
            let mut total = 0;
            let rc = NetUserEnum(
                null(),
                0,
                FILTER_NORMAL_ACCOUNT,
                &mut buf,
                MAX_PREFERRED_LENGTH,
                &mut count,
                &mut total,
                &mut resume,
            );
            ensure!(rc == 0 || rc == ERROR_MORE_DATA, "NetUserEnum failed: {rc}");
            if count > 0 {
                for entry in std::slice::from_raw_parts(buf.cast::<USER_INFO_0>(), count as usize) {
                    let mut detail = null_mut();
                    if NetUserGetInfo(null(), entry.usri0_name, 23, &mut detail) == 0 {
                        let u = &*detail.cast::<USER_INFO_23>();
                        if u.usri23_flags & UF_ACCOUNTDISABLE == 0
                            && let Ok(sid) = sid_string(u.usri23_user_sid)
                        {
                            let name = from_wide(u.usri23_name);
                            let full = from_wide(u.usri23_full_name);
                            result.push(Account {
                                sid,
                                display_name: if full.is_empty() { name.clone() } else { full },
                                name,
                            });
                        }
                        NetApiBufferFree(detail.cast());
                    }
                }
            }
            if !buf.is_null() {
                NetApiBufferFree(buf.cast());
            }
            if rc == 0 {
                break;
            }
        }
        Ok(result)
    }
}
#[derive(Clone, Debug)]
pub struct Session {
    pub id: u32,
    pub sid: String,
    pub active: bool,
    pub locked: bool,
}
pub fn sessions(idle_seconds: u64) -> Result<Vec<Session>> {
    unsafe {
        let mut ptr = null_mut();
        let mut count = 0;
        check(WTSEnumerateSessionsW(
            null_mut(),
            0,
            1,
            &mut ptr,
            &mut count,
        ))?;
        let mut result = Vec::new();
        if count > 0 {
            for session in std::slice::from_raw_parts(ptr, count as usize) {
                if session.SessionId == 0 {
                    continue;
                }
                let mut token = null_mut();
                if WTSQueryUserToken(session.SessionId, &mut token) == 0 {
                    continue;
                }
                let token = Handle(token);
                let Ok(sid) = token_sid(token.0) else {
                    continue;
                };
                let mut buffer = null_mut();
                let mut bytes = 0;
                // Unknown lock/idle state counts conservatively, not as free time.
                let mut locked = false;
                let mut idle = false;
                if WTSQuerySessionInformationW(
                    null_mut(),
                    session.SessionId,
                    WTSSessionInfoEx,
                    &mut buffer,
                    &mut bytes,
                ) != 0
                {
                    if bytes as usize >= size_of::<WTSINFOEXW>() {
                        let info = &*buffer.cast::<WTSINFOEXW>();
                        if info.Level == 1 {
                            let data = info.Data.WTSInfoExLevel1;
                            locked = data.SessionFlags == WTS_SESSIONSTATE_LOCK as i32;
                            idle = data.LastInputTime > 0
                                && (data.CurrentTime - data.LastInputTime).max(0) as u64
                                    / 10_000_000
                                    >= idle_seconds;
                        }
                    }
                    WTSFreeMemory(buffer.cast());
                }
                result.push(Session {
                    id: session.SessionId,
                    sid,
                    active: session.State == WTSActive && !locked && !idle,
                    locked,
                });
            }
        }
        WTSFreeMemory(ptr.cast());
        Ok(result)
    }
}
pub fn logoff(id: u32) -> Result<()> {
    unsafe { check(WTSLogoffSession(null_mut(), id, 0)) }
}
pub fn start_helper(id: u32) -> Result<Handle> {
    unsafe {
        let exe = std::env::current_exe()?.with_file_name("screenguard-tray-windows.exe");
        let exe = wide(exe.to_str().context("Invalid executable path")?);
        let mut token = null_mut();
        check(WTSQueryUserToken(id, &mut token))?;
        let token = Handle(token);
        let mut environment = null_mut();
        check(CreateEnvironmentBlock(&mut environment, token.0, 0))?;
        let mut startup: STARTUPINFOW = zeroed();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut desktop = wide(r"winsta0\default");
        startup.lpDesktop = desktop.as_mut_ptr();
        let mut process: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessAsUserW(
            token.0,
            exe.as_ptr(),
            null_mut(),
            null(),
            null(),
            0,
            CREATE_UNICODE_ENVIRONMENT,
            environment,
            null(),
            &startup,
            &mut process,
        );
        let error = std::io::Error::last_os_error();
        DestroyEnvironmentBlock(environment);
        if ok == 0 {
            return Err(error.into());
        }
        CloseHandle(process.hThread);
        Ok(Handle(process.hProcess))
    }
}
pub fn running(handle: &Handle) -> bool {
    unsafe { WaitForSingleObject(handle.0, 0) == WAIT_TIMEOUT }
}
pub fn pipe_identity(handle: HANDLE) -> Result<(String, u32)> {
    unsafe {
        let mut pid = 0;
        let mut session = 0;
        check(GetNamedPipeClientProcessId(handle, &mut pid))?;
        check(GetNamedPipeClientSessionId(handle, &mut session))?;
        Ok((process_sid(pid)?, session))
    }
}
pub fn socket_owner(peer: SocketAddr, local: SocketAddr) -> Result<String> {
    unsafe {
        let mut size = 0;
        GetExtendedTcpTable(
            null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        ensure!(
            size >= size_of::<MIB_TCPTABLE_OWNER_PID>() as u32,
            "No TCP owner table"
        );
        let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        ensure!(
            GetExtendedTcpTable(
                data.as_mut_ptr().cast(),
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0
            ) == 0,
            "TCP owner lookup failed"
        );
        let table = &*data.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        for row in std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) {
            if u16::from_be(row.dwLocalPort as u16) == peer.port()
                && u16::from_be(row.dwRemotePort as u16) == local.port()
                && row.dwLocalAddr.to_ne_bytes() == [127, 0, 0, 1]
                && row.dwRemoteAddr.to_ne_bytes() == [127, 0, 0, 1]
            {
                return process_sid(row.dwOwningPid);
            }
        }
        anyhow::bail!("TCP peer not found")
    }
}
// Unbiased interrupt time excludes sleep/hibernation, unlike wall-clock time.
pub fn awake_seconds() -> u64 {
    let mut time = 0;
    unsafe {
        windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(&mut time);
    }
    time / 10_000_000
}
pub fn disconnect(id: u32) -> Result<()> {
    unsafe { check(WTSDisconnectSession(null_mut(), id, 0)) }
}

/// Adopt a helper from a previous service instance rather than repeatedly
/// launching processes that immediately exit on the session mutex.
pub fn existing_helper(session: u32) -> Option<Handle> {
    unsafe {
        let expected = std::env::current_exe()
            .ok()?
            .with_file_name("screenguard-tray-windows.exe");
        let mut ptr = null_mut();
        let mut count = 0;
        if WTSEnumerateProcessesW(null_mut(), 0, 1, &mut ptr, &mut count) == 0 {
            return None;
        }
        let mut found = None;
        if count > 0 {
            for process in std::slice::from_raw_parts(ptr, count as usize) {
                if process.SessionId != session
                    || !from_wide(process.pProcessName)
                        .eq_ignore_ascii_case("screenguard-tray-windows.exe")
                {
                    continue;
                }
                let handle = Handle(OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | 0x00100000,
                    0,
                    process.ProcessId,
                ));
                if handle.0.is_null() {
                    continue;
                }
                let mut path = vec![0u16; 32768];
                let mut len = path.len() as u32;
                if QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut len) != 0
                    && String::from_utf16_lossy(&path[..len as usize])
                        .eq_ignore_ascii_case(&expected.to_string_lossy())
                {
                    found = Some(handle);
                    break;
                }
            }
        }
        WTSFreeMemory(ptr.cast());
        found
    }
}
