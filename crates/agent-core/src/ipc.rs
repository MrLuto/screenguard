//! The helper may request its own status only. Identity comes from the pipe peer,
//! never from a UID supplied in a message.
use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\ScreenGuard.Session.v1";
pub const MAX_FRAME: usize = 16384;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub remaining_seconds: Option<i64>,
    pub blocked: bool,
    pub online: bool,
    pub language: String,
    pub admin_url: String,
    pub proxy_port: Option<u16>,
    pub notification: Option<Notification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: u64,
    pub title: String,
    pub body: String,
}
