use anyhow::{Result, ensure};
use std::os::windows::process::CommandExt;

/// The service runs a protected, installed updater script. No server-provided
/// URL, filename, command line or version is executed as administrator.
pub fn launch() -> Result<std::process::Child> {
    let script = std::env::current_exe()?.with_file_name("update.ps1");
    ensure!(script.exists(), "Updater is not installed");
    let powershell = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").ok_or_else(|| anyhow::anyhow!("SystemRoot missing"))?,
    )
    .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let child = std::process::Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .creation_flags(0x08000000)
        .spawn()?;
    Ok(child)
}
pub fn logs() -> Result<Vec<String>> {
    let mut paths = std::fs::read_dir(crate::config::data_dir()?.join("logs"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect::<Vec<_>>();
    paths.sort();
    let Some(path) = paths.last() else {
        return Ok(Vec::new());
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(str::to_owned)
        .collect())
}
