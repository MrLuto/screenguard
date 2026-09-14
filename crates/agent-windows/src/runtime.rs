use crate::{config::Config, native};
use agent_core::{
    db::{Db, ServerConnection},
    ipc::{Notification, SessionStatus},
    policy::LockState,
};
use anyhow::{Context, Result};
use common::{
    messages::*,
    models::{EnforceAction, LocalUser, UsageEntry},
    protocol::WssMessage,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

enum Event {
    Connected(mpsc::Sender<WssMessage>),
    Disconnected,
    Message(ServerMessage),
}

pub async fn run(mut stop: watch::Receiver<bool>) -> Result<()> {
    let cfg = crate::config::load()?;
    let path = crate::config::db_path()?;
    let db = Db::open(Some(path.to_str().context("Invalid database path")?))?;
    let requested = cfg
        .cloud_account
        .as_ref()
        .map(|a| a.trim().to_ascii_lowercase());
    if let Some(old) = db.get_server_connection()?
        && requested.is_some()
        && requested
            != old
                .cloud_account
                .as_ref()
                .map(|a| a.trim().to_ascii_lowercase())
    {
        db.wipe_for_rebind()?;
    }
    let statuses: crate::ipc::StatusMap = Arc::new(Mutex::new(HashMap::new()));
    let mut ipc_task = tokio::spawn(crate::ipc::serve(statuses.clone()));
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let network_cfg = cfg.clone();
    let network = tokio::spawn(async move { network_loop(network_cfg, events_tx).await });
    let result = main_loop(
        &cfg,
        &db,
        statuses.clone(),
        &mut events_rx,
        &mut stop,
        &mut ipc_task,
    )
    .await;
    // Helpers see an explicit empty policy before IPC goes away and restore the
    // proxy configuration. WFP dynamic objects disappear when main_loop exits.
    statuses.lock().await.clear();
    tokio::time::sleep(Duration::from_secs(2)).await;
    network.abort();
    ipc_task.abort();
    result
}

async fn network_loop(cfg: Config, tx: mpsc::Sender<Event>) -> Result<()> {
    let mut delay = 5;
    loop {
        let started = std::time::Instant::now();
        let result = network_once(&cfg, &tx).await;
        if tx.send(Event::Disconnected).await.is_err() {
            return Ok(());
        }
        if let Err(e) = result {
            tracing::warn!("Server connection: {e:#}");
        }
        if started.elapsed() > Duration::from_secs(30) {
            delay = 5;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(300);
    }
}
async fn network_once(cfg: &Config, tx: &mpsc::Sender<Event>) -> Result<()> {
    let path = crate::config::db_path()?;
    let db = Db::open(Some(path.to_str().context("Invalid DB path")?))?;
    let connection = match db.get_server_connection()? {
        Some(c) => c,
        None => {
            let url = if cfg.cloud_account.is_some() {
                Some(cfg.cloud_url.clone())
            } else {
                agent_core::discovery::resolve_server_url(cfg.server_url.as_deref()).await?
            };
            let url = url.context("No server discovered")?;
            let mut stream = tokio::time::timeout(Duration::from_secs(15), connect_async(&url))
                .await??
                .0;
            use rand::Rng;
            let code: String = (0..6)
                .map(|_| {
                    b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[rand::thread_rng().gen_range(0..32)] as char
                })
                .collect();
            tracing::info!("PAIRING CODE: {code}");
            let request = PairingRequest {
                machine_id: db.machine_identity()?,
                hostname: hostname(),
                pairing_code: code,
                cloud_account: cfg.cloud_account.clone(),
            };
            stream
                .send(Message::Text(
                    WssMessage::new(MSG_PAIRING_REQUEST, &request)?.to_json()?,
                ))
                .await?;
            let accepted = tokio::time::timeout(Duration::from_secs(300), async {
                while let Some(msg) = stream.next().await {
                    match msg? {
                        Message::Text(text) => {
                            let env = WssMessage::from_json(&text)?;
                            if env.msg_type == MSG_PAIRING_ACCEPTED {
                                return env.parse_payload::<PairingAccepted>().map_err(Into::into);
                            }
                        }
                        Message::Ping(p) => stream.send(Message::Pong(p)).await?,
                        Message::Close(_) => break,
                        _ => (),
                    }
                }
                anyhow::bail!("Pairing connection closed")
            })
            .await??;
            let c = ServerConnection {
                server_url: url,
                auth_token: accepted.auth_token,
                agent_id: accepted.agent_id,
                cloud_account: cfg.cloud_account.clone(),
            };
            db.save_server_connection(&c)?;
            c
        }
    };
    let mut request = connection.server_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", connection.auth_token).parse()?,
    );
    let stream = tokio::time::timeout(Duration::from_secs(15), connect_async(request))
        .await??
        .0;
    let (mut write, mut read) = stream.split();
    let (out, mut rx) = mpsc::channel::<WssMessage>(32);
    tx.send(Event::Connected(out)).await?;
    let mut last_received = tokio::time::Instant::now();
    loop {
        tokio::select! {
            message = rx.recv() => {
                let Some(message) = message else { return Ok(()); };
                tokio::time::timeout(Duration::from_secs(10), write.send(Message::Text(message.to_json()?))).await??;
            },
            message = tokio::time::timeout_at(last_received + Duration::from_secs(60), read.next()) => {
                last_received = tokio::time::Instant::now();
                match message?.context("Server closed connection")?? {
                    Message::Text(text) => {
                        let message = agent_core::ws_client::parse_server_message(&text)?;
                        let unpair = matches!(message, ServerMessage::Unpair);
                        if unpair { db.reset_pairing()?; }
                        tx.send(Event::Message(message)).await?;
                        if unpair { return Ok(()); }
                    },
                    Message::Ping(p) => write.send(Message::Pong(p)).await?,
                    Message::Close(_) => return Ok(()),
                    _ => (),
                }
            }
        }
    }
}
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into())
}
fn hello(db: &Db, cfg: &Config, filtering: bool) -> Result<WssMessage> {
    let mut capabilities = vec!["platform:windows".into(), "arch:x86_64".into()];
    if filtering {
        capabilities.push("web_filter".into());
    }
    let cloud_account = db
        .get_server_connection()?
        .and_then(|c| c.cloud_account)
        .or(cfg.cloud_account.clone());
    Ok(WssMessage::new(
        MSG_AGENT_HELLO,
        &AgentHello {
            machine_id: db.machine_identity()?,
            hostname: hostname(),
            timezone: iana_time_zone::get_timezone()
                .context("Cannot determine Windows timezone")?,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            last_config_version: 0,
            capabilities,
            cloud_account,
        },
    )?)
}
fn users(db: &Db) -> Result<Vec<LocalUser>> {
    native::accounts()?
        .into_iter()
        .map(|a| {
            Ok(LocalUser {
                local_uid: db.uid_for_sid(&a.sid)?,
                username: a.name,
                display_name: a.display_name,
            })
        })
        .collect()
}
fn enqueue(out: &mut Option<mpsc::Sender<WssMessage>>, message: WssMessage) {
    if out.as_ref().is_some_and(|tx| tx.try_send(message).is_err()) {
        *out = None;
    }
}
fn notify(
    notices: &mut HashMap<u32, Notification>,
    uid: u32,
    title: String,
    body: String,
    sequence: &mut u64,
) {
    *sequence += 1;
    notices.insert(
        uid,
        Notification {
            id: *sequence,
            title: title.chars().take(128).collect(),
            body: body.chars().take(2048).collect(),
        },
    );
}
async fn apply_filter(filter: &mut Option<crate::filter::Filter>, db: &Db) -> Result<()> {
    if let Some(filter) = filter {
        let mut configs = Vec::new();
        for uid in db.get_managed_uids()? {
            if let Some(sid) = db.sid_for_uid(uid)? {
                configs.push((uid, sid, db.get_cached_blocked_domains(uid)?));
            }
        }
        filter.apply(configs).await?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn main_loop(
    cfg: &Config,
    db: &Db,
    statuses: crate::ipc::StatusMap,
    events: &mut mpsc::Receiver<Event>,
    stop: &mut watch::Receiver<bool>,
    ipc: &mut tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    // If configured filtering cannot be installed, fail visibly so service
    // recovery retries. Never advertise a capability that failed to start.
    let mut filter = if cfg.web_filter {
        Some(crate::filter::Filter::new()?)
    } else {
        None
    };
    apply_filter(&mut filter, db).await?;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut out = None;
    let mut remote: HashMap<u32, (i64, EnforceAction, u64)> = HashMap::new();
    let mut locks: HashMap<u32, LockState> = HashMap::new();
    let mut notices = HashMap::new();
    let mut sequence = chrono::Utc::now().timestamp_millis() as u64;
    let mut warnings: HashMap<u32, HashSet<u32>> = HashMap::new();
    let mut helpers: HashMap<u32, native::Handle> = HashMap::new();
    let mut previous_active = HashSet::new();
    let mut deltas: HashMap<u32, u32> = HashMap::new();
    let mut date = chrono::Local::now().date_naive();
    let mut previous_tick = native::awake_seconds();
    let mut last_heartbeat = previous_tick;
    let mut current_timezone = iana_time_zone::get_timezone()?;
    let mut last_scan = previous_tick;
    let mut known_users = users(db)?;
    let mut lock_started: HashMap<u32, u64> = HashMap::new();
    let mut updater: Option<std::process::Child> = None;
    let mut offline_since = previous_tick;
    let mut ttl_warned = false;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            result = &mut *ipc => { result??; anyhow::bail!("Session IPC stopped unexpectedly"); },
            Some(event) = events.recv() => {
                match event {
                    Event::Connected(tx) => {
                        out = Some(tx); remote.clear(); deltas.clear();
                        enqueue(&mut out, hello(db, cfg, filter.is_some())?);
                        let usage = db.get_unsynced_usage()?.into_iter().filter_map(|(uid,date,total)| {
                            Some(UsageEntry { local_uid: uid, date: date.parse().ok()?, used_seconds: total })
                        }).collect::<Vec<_>>();
                        enqueue(&mut out, WssMessage::new(MSG_USAGE_SYNC, &UsageSync { usage })?);
                        known_users = users(db)?;
                        enqueue(&mut out, WssMessage::new(MSG_USER_LIST_UPDATE, &UserListUpdate { users: known_users.clone(), removed_uids: Vec::new() })?);
                        last_heartbeat = 0;
                        tracing::info!("Connected; cumulative usage synchronized");
                    },
                    Event::Disconnected => { out = None; remote.clear(); offline_since = native::awake_seconds(); ttl_warned = false; },
                    Event::Message(message) => match message {
                        ServerMessage::ConfigPush(push) => {
                            let old: HashMap<_,_> = push.users.iter().map(|u| Ok((u.local_uid, db.get_cached_adjustment(u.local_uid, &date.to_string())?))).collect::<Result<_>>()?;
                            db.apply_config_push(&push.users)?; db.save_config_version(push.config_version)?;
                            // A policy change invalidates the earlier online allowance.
                            remote.clear();
                            apply_filter(&mut filter, db).await?;
                            for u in push.users {
                                let delta = u.adjustments_today - old.get(&u.local_uid).copied().unwrap_or(0);
                                if delta != 0 {
                                    let remaining = (agent_core::policy::remaining(db, u.local_uid, chrono::Local::now().naive_local())? / 60).min(i32::MAX as i64) as i32;
                                    let body = if delta > 0 { agent_core::i18n::notif_added_body(&u.language, delta, remaining, u.adjustment_message.as_deref()) }
                                        else { agent_core::i18n::notif_reduced_body(&u.language, -delta, remaining, u.adjustment_message.as_deref()) };
                                    notify(&mut notices, u.local_uid, agent_core::i18n::notif_schedule_title(&u.language).into(), body, &mut sequence);
                                } else {
                                    notify(&mut notices, u.local_uid, agent_core::i18n::notif_schedule_title(&u.language).into(), agent_core::i18n::notif_schedule_updated(&u.language).into(), &mut sequence);
                                }
                                warnings.remove(&u.local_uid);
                            }
                        },
                        ServerMessage::RemainingUpdate(update) => {
                            for e in update.users { remote.insert(e.local_uid, (i64::from(e.remaining_minutes) * 60, e.enforce, native::awake_seconds())); }
                        },
                        ServerMessage::NotifyUser(n) => notify(&mut notices, n.local_uid, n.summary, n.body, &mut sequence),
                        ServerMessage::LockNow(n) => {
                            // Server follows with updated policy; keep a local deny if
                            // the connection drops between the command and config_push.
                            db.force_lock_today(n.local_uid, &date.to_string())?;
                            remote.insert(n.local_uid, (0, EnforceAction::Lock, native::awake_seconds()));
                        },
                        ServerMessage::ConfigReload => enqueue(&mut out, hello(db, cfg, filter.is_some())?),
                        ServerMessage::FetchLogs => enqueue(&mut out, WssMessage::new(MSG_LOG_RESPONSE, &LogResponse { lines: crate::update::logs()? })?),
                        ServerMessage::UpdateAgent => {
                            if cfg.allow_remote_update && updater.is_none() {
                                match crate::update::launch() { Ok(child) => updater = Some(child), Err(e) => tracing::error!("Updater launch failed: {e}") }
                            }
                            else { tracing::warn!("Remote update disabled or already requested"); }
                        },
                        ServerMessage::Unpair => { out = None; remote.clear(); },
                        _ => (),
                    }
                }
            },
            _ = ticker.tick() => {
                if let Some(child) = updater.as_mut() && let Some(exit) = child.try_wait()? {
                    tracing::info!("Updater exited: {exit}"); updater = None;
                }
                let now = native::awake_seconds(); let wall = chrono::Local::now();
                let elapsed = now.saturating_sub(previous_tick).min(5); previous_tick = now;
                let sessions = native::sessions(cfg.idle_seconds)?;
                let managed: HashSet<u32> = db.get_managed_uids()?.into_iter().collect();
                let mut active = HashSet::new(); let mut user_sessions: HashMap<u32, Vec<&native::Session>> = HashMap::new();
                for session in &sessions {
                    let uid = db.uid_for_sid(&session.sid)?;
                    if !managed.contains(&uid) { continue; }
                    if session.active { active.insert(uid); }
                    user_sessions.entry(uid).or_default().push(session);
                }
                // Attribute the last interval to the previous day's active user.
                // A suspended interval contributes zero because the clock excludes sleep.
                for uid in previous_active.intersection(&managed) {
                    db.add_usage_seconds(*uid, &date.to_string(), elapsed)?;
                    *deltas.entry(*uid).or_default() += elapsed as u32;
                }
                previous_active = active.clone();
                if wall.date_naive() != date {
                    date = wall.date_naive(); remote.clear(); deltas.clear(); warnings.clear();
                    // Snapshot replaces the final previous-day delta on the server.
                    if out.is_some() {
                        let usage = db.get_unsynced_usage()?.into_iter().filter_map(|(uid,d,total)| Some(UsageEntry { local_uid: uid, date: d.parse().ok()?, used_seconds: total })).collect();
                        enqueue(&mut out, WssMessage::new(MSG_USAGE_SYNC, &UsageSync { usage })?);
                    }
                }
                if out.is_none() && !ttl_warned && now.saturating_sub(offline_since) >= cfg.cache_ttl_hours.saturating_mul(3600) {
                    tracing::warn!("Policy cache TTL exceeded; continuing cached enforcement"); ttl_warned = true;
                }
                helpers.retain(|id, handle| sessions.iter().any(|s| s.id == *id) && native::running(handle));
                for session in &sessions {
                    if let std::collections::hash_map::Entry::Vacant(entry) = helpers.entry(session.id) {
                        if let Some(handle) = native::existing_helper(session.id) { entry.insert(handle); continue; }
                        match native::start_helper(session.id) { Ok(handle) => { entry.insert(handle); }, Err(e) => tracing::warn!("Cannot start helper in session {}: {e}", session.id) }
                    }
                }
                let mut state = HashMap::new();
                for uid in &managed {
                    let settings = db.get_cached_enforcement(*uid)?;
                    let local = agent_core::policy::remaining(db, *uid, wall.naive_local())?;
                    let seconds = remote.get(uid).filter(|(_,_,at)| out.is_some() && now.saturating_sub(*at) < 30)
                        .map(|(remaining, action, _)| if *action == EnforceAction::Lock { 0 } else { *remaining })
                        .unwrap_or(local);
                    let blocked = seconds <= 0;
                    if blocked {
                        if !lock_started.contains_key(uid) {
                            lock_started.insert(*uid, now);
                            notify(&mut notices, *uid, agent_core::i18n::notif_lock_title(&settings.language).into(), agent_core::i18n::notif_lock_body(&settings.language).into(), &mut sequence);
                        }
                    } else { lock_started.remove(uid); }
                    let terminate = locks.entry(*uid).or_default().update(blocked, settings.preserve_tasks_on_lock, u64::from(settings.lockout_grace_minutes) * 60 + 4, now);
                    if seconds > 0 && seconds != i64::MAX {
                        let warned = warnings.entry(*uid).or_default();
                        for threshold in &settings.warning_thresholds {
                            if seconds / 60 > i64::from(*threshold) { warned.remove(threshold); }
                        }
                        let mut crossed = false;
                        for threshold in settings.warning_thresholds.iter().filter(|m| seconds / 60 <= i64::from(**m)) { crossed |= warned.insert(*threshold); }
                        if crossed {
                            notify(&mut notices, *uid, agent_core::i18n::notif_warning_title(&settings.language).into(), agent_core::i18n::notif_warning_body(&settings.language, (seconds/60) as i32), &mut sequence);
                        }
                    }
                    for session in user_sessions.get(uid).into_iter().flatten() {
                        if terminate { if let Err(e) = native::logoff(session.id) { tracing::warn!("Logoff failed: {e}"); } }
                        else if blocked && !session.locked && now.saturating_sub(*lock_started.get(uid).unwrap_or(&now)) >= 4 {
                            // Service-side fallback preserves applications, even if a user
                            // kills or suspends their helper before it can lock the desktop.
                            if let Err(e) = native::disconnect(session.id) { tracing::warn!("Session disconnect failed: {e}"); }
                        }
                    }
                    let admin_url = cfg.webui_url.clone().unwrap_or_else(|| db.get_server_connection().ok().flatten().map(|c| c.server_url).or(cfg.server_url.clone()).unwrap_or_default().replace("wss://", "https://").replace("ws://", "http://").trim_end_matches("/ws").into());
                    for session in user_sessions.get(uid).into_iter().flatten() {
                        state.insert((session.sid.clone(), session.id), SessionStatus { remaining_seconds: (seconds != i64::MAX).then_some(seconds), blocked, online: out.is_some(), language: settings.language.clone(), admin_url: admin_url.clone(), proxy_port: filter.as_ref().and_then(|f| f.port(*uid)), notification: notices.get(uid).cloned() });
                    }
                }
                locks.retain(|uid,_| managed.contains(uid)); lock_started.retain(|uid,_| managed.contains(uid));
                *statuses.lock().await = state;
                if now.saturating_sub(last_scan) >= cfg.user_scan_interval {
                    let timezone = iana_time_zone::get_timezone()?;
                    if timezone != current_timezone {
                        current_timezone = timezone; remote.clear();
                        enqueue(&mut out, hello(db, cfg, filter.is_some())?);
                    }
                    let next = users(db)?;
                    let removed_uids = known_users.iter().filter(|u| !next.iter().any(|n| n.local_uid == u.local_uid)).map(|u| u.local_uid).collect();
                    enqueue(&mut out, WssMessage::new(MSG_USER_LIST_UPDATE, &UserListUpdate { users: next.clone(), removed_uids })?);
                    known_users = next; last_scan = now;
                }
                if now.saturating_sub(last_heartbeat) >= cfg.heartbeat_interval {
                    let users = managed.iter().map(|uid| HeartbeatUser { local_uid: *uid, active_seconds_since_last: deltas.get(uid).copied().unwrap_or(0), idle: !active.contains(uid), session_count: user_sessions.get(uid).map_or(0, |s| s.len() as u32) }).collect();
                    enqueue(&mut out, WssMessage::new(MSG_HEARTBEAT, &Heartbeat { users })?);
                    deltas.clear(); last_heartbeat = now;
                }
            }
        }
    }
    Ok(())
}
