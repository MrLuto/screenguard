use agent_core::ipc::{PIPE_NAME, SessionStatus};
use anyhow::Result;
use std::{collections::HashMap, os::windows::io::AsRawHandle, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::ServerOptions,
    sync::{Mutex, Semaphore},
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
};

pub type StatusMap = Arc<Mutex<HashMap<(String, u32), SessionStatus>>>;
// No mutations are accepted from the helper. Frame is the literal "status\n".
pub async fn serve(state: StatusMap) -> Result<()> {
    let limit = Arc::new(Semaphore::new(32));
    let mut first = true;
    loop {
        let mut server = unsafe {
            let mut sd = std::ptr::null_mut();
            let sddl = crate::native::wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");
            anyhow::ensure!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut sd,
                    std::ptr::null_mut()
                ) != 0,
                "Pipe ACL failed"
            );
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let created = ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    PIPE_NAME,
                    (&attributes as *const SECURITY_ATTRIBUTES)
                        .cast_mut()
                        .cast(),
                );
            LocalFree(sd);
            created?
        };
        first = false;
        server.connect().await?;
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let mut request = [0; 7];
                server.read_exact(&mut request).await?;
                anyhow::ensure!(&request == b"status\n", "Invalid IPC request");
                let identity = crate::native::pipe_identity(server.as_raw_handle())?;
                let value = state
                    .lock()
                    .await
                    .get(&identity)
                    .cloned()
                    .unwrap_or_default();
                let mut frame = serde_json::to_vec(&value)?;
                frame.push(b'\n');
                server.write_all(&frame).await?;
                // Wait for acknowledgment before dropping a Windows pipe handle.
                let mut ack = [0];
                server.read_exact(&mut ack).await?;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        });
    }
}
