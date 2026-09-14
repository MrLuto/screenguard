//! Time-dependent policy with an explicit clock, shared by both operating systems.
use crate::db::Db;
use anyhow::Result;
use chrono::{Datelike, NaiveDateTime, NaiveTime};
use common::models::EnforceAction;

pub fn remaining(db: &Db, uid: u32, now: NaiveDateTime) -> Result<i64> {
    if db.manually_locked(uid, &now.date().to_string())? {
        return Ok(0);
    }
    let weekday = now.weekday().num_days_from_monday() as u8;
    let schedules = db.get_cached_schedules(uid)?;
    let window = if schedules.is_empty() {
        i64::MAX
    } else {
        schedules
            .iter()
            .filter(|s| s.day_of_week == weekday)
            .filter_map(|s| {
                let start = NaiveTime::parse_from_str(&s.start_time, "%H:%M").ok()?;
                let end = NaiveTime::parse_from_str(&s.end_time, "%H:%M").ok()?;
                (now.time() >= start && now.time() <= end).then(|| (end - now.time()).num_seconds())
            })
            .max()
            .unwrap_or(0)
    };
    let limit = db
        .get_cached_daily_limits(uid)?
        .iter()
        .find(|l| l.day_of_week == weekday)
        .map(|l| l.allowed_minutes);
    let allowance = match limit {
        Some(minutes) => {
            (i64::from(minutes)
                + i64::from(db.get_cached_adjustment(uid, &now.date().to_string())?))
                * 60
                - db.get_usage_seconds(uid, &now.date().to_string())? as i64
        }
        None => i64::MAX,
    };
    Ok(window.min(allowance).max(0))
}

pub fn evaluate(db: &Db, uid: u32, now: NaiveDateTime) -> Result<EnforceAction> {
    let seconds = remaining(db, uid, now)?;
    if seconds <= 0 {
        return Ok(EnforceAction::Lock);
    }
    let settings = db.get_cached_enforcement(uid)?;
    Ok(
        if settings
            .warning_thresholds
            .iter()
            .any(|m| seconds / 60 <= i64::from(*m))
        {
            EnforceAction::Warn
        } else {
            EnforceAction::Allow
        },
    )
}

/// Grace timers are owned by the policy loop, not detached tasks. Lifting a
/// block cancels its deadline, including a block lifted and reapplied quickly.
#[derive(Default, Debug)]
pub struct LockState {
    deadline: Option<u64>,
    blocked: bool,
    preserve: bool,
}
impl LockState {
    pub fn update(&mut self, blocked: bool, preserve: bool, grace_secs: u64, now: u64) -> bool {
        if !blocked {
            self.deadline = None;
            self.blocked = false;
            self.preserve = preserve;
            return false;
        }
        if !self.blocked || self.preserve != preserve {
            self.deadline = (!preserve).then(|| now.saturating_add(grace_secs));
        }
        self.blocked = true;
        self.preserve = preserve;
        self.deadline.is_some_and(|d| now >= d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_cancels_old_logoff_and_reblock_has_new_deadline() {
        let mut state = LockState::default();
        assert!(!state.update(true, false, 60, 0));
        assert!(!state.update(false, false, 60, 59));
        assert!(!state.update(true, false, 60, 60));
        assert!(!state.update(true, false, 60, 119));
        assert!(state.update(true, false, 60, 120));
    }
    #[test]
    fn preserve_never_logs_off_and_switching_arms_grace() {
        let mut state = LockState::default();
        assert!(!state.update(true, true, 0, 0));
        assert!(!state.update(true, true, 0, 1000));
        assert!(!state.update(true, false, 60, 1000));
        assert!(state.update(true, false, 60, 1060));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use common::models::{DailyLimit, Schedule, UserConfig, UserStatus};
    fn config() -> UserConfig {
        UserConfig {
            local_uid: 1,
            profile_id: uuid::Uuid::new_v4(),
            status: UserStatus::Managed,
            schedules: vec![Schedule {
                day_of_week: 0,
                start_time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                end_time: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
            }],
            daily_limits: vec![DailyLimit {
                day_of_week: 0,
                allowed_minutes: 60,
            }],
            adjustments_today: 0,
            adjustment_message: None,
            lockout_grace_minutes: 5,
            preserve_tasks_on_lock: false,
            warning_thresholds_minutes: vec![15, 5, 1],
            language: "en".into(),
            blocked_domains: vec![],
        }
    }
    fn at(time: &str) -> NaiveDateTime {
        format!("2026-09-14T{time}").parse().unwrap()
    }
    #[test]
    fn offline_limits_usage_and_schedule_intersection() {
        let db = Db::open(Some(":memory:")).unwrap();
        db.apply_config_push(&[config()]).unwrap();
        assert_eq!(remaining(&db, 1, at("08:59:00")).unwrap(), 0);
        assert_eq!(remaining(&db, 1, at("09:00:00")).unwrap(), 3600);
        db.add_usage_seconds(1, "2026-09-14", 3000).unwrap();
        assert_eq!(remaining(&db, 1, at("10:00:00")).unwrap(), 600);
        assert_eq!(
            evaluate(&db, 1, at("10:00:00")).unwrap(),
            EnforceAction::Warn
        );
        assert_eq!(remaining(&db, 1, at("17:59:30")).unwrap(), 30);
        assert_eq!(remaining(&db, 1, at("18:00:01")).unwrap(), 0);
    }
    #[test]
    fn manual_lock_is_dated_and_superseded_by_new_policy() {
        let db = Db::open(Some(":memory:")).unwrap();
        let mut cfg = config();
        cfg.schedules.clear();
        cfg.daily_limits.clear();
        db.apply_config_push(&[cfg.clone()]).unwrap();
        db.force_lock_today(1, "2026-09-14").unwrap();
        assert_eq!(remaining(&db, 1, at("10:00:00")).unwrap(), 0);
        let tomorrow = "2026-09-15T10:00:00".parse().unwrap();
        assert_eq!(remaining(&db, 1, tomorrow).unwrap(), i64::MAX);
        db.apply_config_push(&[cfg]).unwrap();
        assert_eq!(remaining(&db, 1, at("10:00:00")).unwrap(), i64::MAX);
    }
}
