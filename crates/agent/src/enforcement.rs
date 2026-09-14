use anyhow::Result;
use chrono::Local;
use common::models::EnforceAction;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::Db;

pub async fn evaluate_enforcement(
    uid: u32,
    db: &Arc<Mutex<Db>>,
    online: bool,
) -> Result<EnforceAction> {
    let db = db.lock().await;

    if online {
        if let Some(remaining) = db.get_server_remaining(uid)? {
            return Ok(match remaining.enforce.as_str() {
                "warn" => EnforceAction::Warn,
                "lock" => EnforceAction::Lock,
                _ => EnforceAction::Allow,
            });
        }
        // Server hasn't sent remaining yet — allow (benefit of the doubt on startup).
        return Ok(EnforceAction::Allow);
    }

    // Offline: calculate locally.
    offline_evaluate(uid, &db)
}

fn offline_evaluate(uid: u32, db: &Db) -> Result<EnforceAction> {
    agent_core::policy::evaluate(db, uid, Local::now().naive_local())
}

#[derive(Debug, PartialEq, Eq)]
enum LockBehavior {
    TerminateAfterGrace,
    PreserveAndRelock,
}

fn lock_behavior(preserve_tasks_on_lock: bool) -> LockBehavior {
    if preserve_tasks_on_lock {
        LockBehavior::PreserveAndRelock
    } else {
        LockBehavior::TerminateAfterGrace
    }
}

/// Execute a lock for a UID, preserving or terminating the session according to cached config.
///
/// `locked_uids` is the same set the caller inserted `uid` into to arm this lock. It is
/// re-checked after the grace-period sleep: if the block was lifted in the meantime (admin
/// granted time, schedule window opened, midnight usage reset, preserve re-enabled, ...) the
/// heartbeat/RemainingUpdate handler already removed `uid` from it, and this call must not
/// blindly kill a session the user is now legitimately using just because one still exists.
pub async fn execute_lock(
    uid: u32,
    db: &Arc<Mutex<Db>>,
    locked_uids: &Arc<Mutex<HashSet<u32>>>,
) -> Result<bool> {
    let (session_ids, grace_minutes, language, preserve_tasks) = {
        let db = db.lock().await;
        let sessions = db.get_all_session_ids(uid)?;
        let enforcement = db.get_cached_enforcement(uid)?;
        (sessions, enforcement.lockout_grace_minutes, enforcement.language, enforcement.preserve_tasks_on_lock)
    };

    if session_ids.is_empty() {
        return Ok(true);
    }

    // Final warning before the screen locks.
    let _ = crate::dbus::send_desktop_notification(
        uid,
        crate::i18n::notif_lock_title(&language),
        crate::i18n::notif_lock_body(&language),
    ).await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    tracing::info!("Locking sessions for uid={uid}: {:?}", session_ids);
    crate::dbus::lock_sessions(&session_ids).await?;

    if lock_behavior(preserve_tasks) == LockBehavior::PreserveAndRelock {
        tracing::info!("Keeping sessions running while uid={uid} remains locked");
        return Ok(false);
    }

    // Wait grace period, then terminate any still-active sessions — but only if the
    // block is still in effect.
    tokio::time::sleep(std::time::Duration::from_secs(grace_minutes as u64 * 60)).await;

    if !locked_uids.lock().await.contains(&uid) {
        tracing::info!(
            "uid={uid}: grace period elapsed but enforcement was lifted in the meantime \
             — not terminating"
        );
        return Ok(true);
    }

    let still_active = {
        let db = db.lock().await;
        db.get_all_session_ids(uid)?
    };

    if !still_active.is_empty() {
        tracing::warn!("Sessions still active after grace period for uid={uid}, terminating");
        crate::dbus::terminate_sessions(&still_active).await?;
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{lock_behavior, LockBehavior};

    #[test]
    fn preserve_off_terminates_after_grace() {
        assert_eq!(lock_behavior(false), LockBehavior::TerminateAfterGrace);
    }

    #[test]
    fn preserve_on_keeps_session_armed_for_relocking() {
        assert_eq!(lock_behavior(true), LockBehavior::PreserveAndRelock);
    }
}

/// Midnight handler: called when the calendar date changes.
/// Resets daily usage and locks any sessions that fall outside the new day's schedule.
pub async fn handle_midnight(
    db: &Arc<Mutex<Db>>,
    locked_uids: &Arc<Mutex<HashSet<u32>>>,
) -> Result<()> {
    let yesterday = (Local::now() - chrono::Duration::days(1)).date_naive();
    let uids = {
        let db = db.lock().await;
        db.get_managed_uids()?
    };

    for uid in &uids {
        let db_guard = db.lock().await;
        let _ = db_guard.reset_usage_for_date(*uid, &yesterday);
    }
    tracing::info!("Midnight: reset daily usage counters for {} users", uids.len());

    // Lock any sessions that fall outside the new day's allowed schedule window.
    for uid in uids {
        let action = evaluate_enforcement(uid, db, false).await?;
        if action == EnforceAction::Lock {
            let is_new = locked_uids.lock().await.insert(uid);
            if is_new {
                tracing::info!("Midnight: locking uid={uid} (outside schedule for new day)");
                let db = db.clone();
                let locked_uids = locked_uids.clone();
                tokio::spawn(async move {
                    let rearm = match execute_lock(uid, &db, &locked_uids).await {
                        Ok(rearm) => rearm,
                        Err(e) => {
                            tracing::error!("Midnight lock failed for uid={uid}: {e}");
                            true
                        }
                    };
                    if rearm {
                        locked_uids.lock().await.remove(&uid);
                    }
                });
            }
        }
    }

    Ok(())
}
