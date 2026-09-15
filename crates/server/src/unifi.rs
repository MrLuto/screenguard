//! Local UniFi Integration API. Credentials stay in a mounted secret, never in REST responses.
use crate::{db, state::AppState};
use anyhow::{Context, Result, bail};
use chrono::{Datelike, Utc};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, RwLock};
use uuid::Uuid;

#[derive(Default)]
pub struct Runtime {
    pub snapshot: RwLock<Value>,
    pub wake: Notify,
    pub lock: Mutex<()>,
}

struct Config {
    base: String,
    site: Option<Uuid>,
    source: Option<Uuid>,
    destination: Option<Uuid>,
    enforce: bool,
    protected: HashSet<String>,
    client: Client,
}
impl Config {
    fn load() -> Result<Option<Self>> {
        let Ok(base) = std::env::var("SCREENGUARD_UNIFI_URL") else {
            return Ok(None);
        };
        let url = reqwest::Url::parse(&base)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!("UniFi URL must be an HTTPS origin without credentials or path")
        }
        let key = std::fs::read_to_string(
            std::env::var("SCREENGUARD_UNIFI_API_KEY_FILE")
                .context("SCREENGUARD_UNIFI_API_KEY_FILE is required")?,
        )?;
        let mut header = reqwest::header::HeaderValue::from_str(key.trim())?;
        header.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("X-API-KEY", header);
        let mut builder = Client::builder()
            .default_headers(headers)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10));
        if let Ok(path) = std::env::var("SCREENGUARD_UNIFI_CA_FILE") {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
        }
        let id = |name| -> Result<Option<Uuid>> {
            std::env::var(name)
                .ok()
                .map(|s| Uuid::parse_str(&s).context(name))
                .transpose()
        };
        let protected = std::env::var("SCREENGUARD_UNIFI_PROTECTED_MACS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(normalize_mac)
            .collect::<Result<_>>()?;
        Ok(Some(Self {
            base: base.trim_end_matches('/').into(),
            site: id("SCREENGUARD_UNIFI_SITE_ID")?,
            source: id("SCREENGUARD_UNIFI_SOURCE_ZONE_ID")?,
            destination: id("SCREENGUARD_UNIFI_DESTINATION_ZONE_ID")?,
            enforce: std::env::var("SCREENGUARD_UNIFI_ENFORCE").as_deref() == Ok("true"),
            protected,
            client: builder.build()?,
        }))
    }
    async fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = self.client.request(
            method,
            format!("{}/proxy/network/integration/v1{}", self.base, path),
        );
        if let Some(body) = body {
            req = req.json(body);
        }
        let response = req.send().await.context("UniFi connection failed")?;
        let code = response.status();
        if !code.is_success() {
            bail!("UniFi HTTP {code} for {path}; check API support and permissions")
        }
        if code == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        if response.content_length().unwrap_or(0) > 4 * 1024 * 1024 {
            bail!("UniFi response too large")
        }
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
    async fn list(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for _ in 0..100 {
            let response = self
                .request(
                    Method::GET,
                    &format!("{path}?offset={}&limit=200", out.len()),
                    None,
                )
                .await?;
            let data = response["data"]
                .as_array()
                .context("UniFi list response has no data array")?;
            let n = data.len();
            out.extend(data.iter().cloned());
            if n == 0
                || response["totalCount"]
                    .as_u64()
                    .is_some_and(|total| out.len() as u64 >= total)
            {
                return Ok(out);
            }
        }
        bail!("UniFi pagination limit exceeded; incomplete snapshot discarded")
    }
}

pub fn normalize_mac(value: &str) -> Result<String> {
    let mac = value.trim().to_ascii_lowercase().replace('-', ":");
    let parts: Vec<_> = mac.split(':').collect();
    if parts.len() != 6
        || parts
            .iter()
            .any(|p| p.len() != 2 || !p.bytes().all(|c| c.is_ascii_hexdigit()))
        || u8::from_str_radix(parts[0], 16)? & 1 != 0
        || mac == "00:00:00:00:00:00"
    {
        bail!("Invalid unicast MAC address")
    }
    Ok(mac)
}

pub async fn init(pool: &db::DbPool) -> Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS unifi_meta (name TEXT PRIMARY KEY, value TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS unifi_clients (mac TEXT PRIMARY KEY, data TEXT NOT NULL, last_seen BIGINT NOT NULL, profile_id TEXT, mode TEXT NOT NULL DEFAULT 'observe', excluded INTEGER NOT NULL DEFAULT 1, policy_name TEXT, policy_id TEXT, result TEXT NOT NULL DEFAULT 'observe')",
        "CREATE TABLE IF NOT EXISTS unifi_events (id TEXT PRIMARY KEY, at BIGINT NOT NULL, mac TEXT NOT NULL, action TEXT NOT NULL)",
    ] {
        sqlx::query(statement).execute(pool).await?;
    }
    sqlx::query(
        "INSERT INTO unifi_meta(name,value) VALUES('owner',$1) ON CONFLICT(name) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct Binding {
    pub profile_id: Option<Uuid>,
    pub mode: String,
    pub excluded: bool,
}

pub async fn bind(pool: &db::DbPool, mac: &str, binding: &Binding) -> Result<()> {
    if !["observe", "follow", "block"].contains(&binding.mode.as_str()) {
        bail!("Unknown mode")
    }
    if let Some(id) = binding.profile_id {
        if db::get_profile(pool, id).await?.is_none() {
            bail!("Profile does not exist")
        }
    } else if binding.mode != "observe" {
        bail!("A profile is required for enforcement")
    }
    let changed =
        sqlx::query("UPDATE unifi_clients SET profile_id=$1,mode=$2,excluded=$3 WHERE mac=$4")
            .bind(binding.profile_id.map(|id| id.to_string()))
            .bind(&binding.mode)
            .bind(i32::from(binding.excluded))
            .bind(mac)
            .execute(pool)
            .await?
            .rows_affected();
    if changed == 0 {
        bail!("Discover the client before linking it")
    }
    event(pool, mac, "binding_changed").await?;
    Ok(())
}

pub async fn clients(pool: &db::DbPool) -> Result<Vec<Value>> {
    let rows = sqlx::query("SELECT * FROM unifi_clients ORDER BY mac")
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(|r| Ok(json!({
        "mac":r.try_get::<String,_>("mac")?, "data":serde_json::from_str::<Value>(&r.try_get::<String,_>("data")?)?,
        "last_seen":r.try_get::<i64,_>("last_seen")?, "profile_id":r.try_get::<Option<String>,_>("profile_id")?,
        "mode":r.try_get::<String,_>("mode")?, "excluded":r.try_get::<i32,_>("excluded")? != 0,
        "policy_id":r.try_get::<Option<String>,_>("policy_id")?, "result":r.try_get::<String,_>("result")?
    }))).collect()
}
async fn event(pool: &db::DbPool, mac: &str, action: &str) -> Result<()> {
    sqlx::query("INSERT INTO unifi_events(id,at,mac,action) VALUES($1,$2,$3,$4)")
        .bind(Uuid::new_v4().to_string())
        .bind(Utc::now().timestamp())
        .bind(mac)
        .bind(action)
        .execute(pool)
        .await?;
    Ok(())
}
pub async fn events(pool: &db::DbPool) -> Result<Vec<Value>> {
    sqlx::query("SELECT at,mac,action FROM unifi_events ORDER BY at DESC LIMIT 100").fetch_all(pool).await?
        .into_iter().map(|r| Ok(json!({"at":r.try_get::<i64,_>("at")?,"mac":r.try_get::<String,_>("mac")?,"action":r.try_get::<String,_>("action")?}))).collect()
}

async fn blocked(pool: &db::DbPool, id: Uuid) -> Result<bool> {
    let tz: chrono_tz::Tz = db::get_admin_timezone(pool)
        .await?
        .parse()
        .context("Invalid administrator timezone")?;
    let now = Utc::now().with_timezone(&tz);
    let date = now.date_naive().to_string();
    let dow = now.weekday().num_days_from_monday() as u8;
    let limit = db::get_daily_limits(pool, id)
        .await?
        .into_iter()
        .find(|l| l.day_of_week == dow)
        .map(|l| l.allowed_minutes)
        .unwrap_or(1440);
    let adjustment = db::sum_adjustments_for_date(pool, id, &date).await?;
    let used = db::get_used_seconds_for_profile_today(pool, id, &date).await?;
    let schedules = db::get_schedules(pool, id).await?;
    let in_window = schedules.is_empty()
        || schedules.iter().any(|s| {
            s.day_of_week == dow
                && db::parse_time(&s.start_time)
                    .zip(db::parse_time(&s.end_time))
                    .is_some_and(|(start, end)| now.time() >= start && now.time() < end)
        });
    Ok(policy_blocked(limit, adjustment, used, in_window))
}
fn policy_blocked(limit: i32, adjustment: i32, used: i64, in_window: bool) -> bool {
    !in_window || used >= (i64::from(limit) + i64::from(adjustment)).max(0) * 60
}
fn rule(name: &str, mac: &str, source: Uuid, destination: Uuid) -> Value {
    json!({"name":name,"description":name,"enabled":true,"loggingEnabled":false,
        "action":{"type":"BLOCK"},"ipProtocolScope":{"ipVersion":"IPV4_AND_IPV6"},
        "source":{"zoneId":source,"trafficFilter":{"type":"MAC_ADDRESS","macAddressFilter":{"macAddresses":[mac]}}},
        "destination":{"zoneId":destination}})
}
// UniFi may serialize absent optional fields as explicit nulls.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), without_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        _ => value.clone(),
    }
}

fn owned_matches(actual: &Value, expected: &Value) -> bool {
    [
        "name",
        "description",
        "enabled",
        "loggingEnabled",
        "action",
        "ipProtocolScope",
        "source",
        "destination",
    ]
    .iter()
    .all(|k| without_nulls(&actual[*k]) == without_nulls(&expected[*k]))
        && ["schedule", "connectionStateFilter", "ipsecFilter"]
            .iter()
            .all(|k| actual[*k].is_null())
}

fn statistics_devices(snapshot: &Value) -> Vec<(Uuid, String)> {
    snapshot["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(|d| {
            Some((
                Uuid::parse_str(d["id"].as_str()?).ok()?,
                d["name"].as_str().unwrap_or("UniFi").into(),
            ))
        })
        .collect()
}

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            {
                let _guard = state.unifi.lock.lock().await;
                let result = sync(&state).await;
                if let Err(error) = result {
                    // Preserve the last good snapshot and label it stale.
                    let mut snapshot = state.unifi.snapshot.write().await;
                    if !snapshot.is_object() {
                        *snapshot = json!({});
                    }
                    snapshot["error"] = json!(error.to_string());
                    snapshot["stale"] = json!(true);
                    tracing::warn!("UniFi sync failed: {error}");
                }
            }
            tokio::select! { _=tokio::time::sleep(Duration::from_secs(30))=>{}, _=state.unifi.wake.notified()=>{} }
        }
    });
}

async fn sync(state: &AppState) -> Result<()> {
    let Some(cfg) = Config::load()? else {
        *state.unifi.snapshot.write().await = json!({"configured":false,"enforcement":false});
        return Ok(());
    };
    let sites = cfg.list("/sites").await?;
    let mut snapshot = json!({"configured":true,"url":cfg.base,"sites":sites,"enforcement":cfg.enforce,
        "last_sync":Utc::now().timestamp(),"stale":false});
    let Some(site) = cfg.site else {
        *state.unifi.snapshot.write().await = snapshot;
        return Ok(());
    };
    // Refuse silent retargeting: persisted client mappings and ownership belong to one controller/site.
    let scope = format!("{}|{}", cfg.base, site);
    sqlx::query(
        "INSERT INTO unifi_meta(name,value) VALUES('scope',$1) ON CONFLICT(name) DO NOTHING",
    )
    .bind(&scope)
    .execute(&state.db)
    .await?;
    let saved: String = sqlx::query_scalar("SELECT value FROM unifi_meta WHERE name='scope'")
        .fetch_one(&state.db)
        .await?;
    if saved != scope {
        bail!("Controller/site changed; clean up existing rules and mappings before migration")
    }
    let root = format!("/sites/{site}");
    let discovered = cfg.list(&format!("{root}/clients")).await?;
    for c in &discovered {
        let Some(mac) = c["macAddress"].as_str().and_then(|s| normalize_mac(s).ok()) else {
            continue;
        };
        // Persist only explicit display fields, not arbitrary controller response data.
        let data = json!({"id":c["id"],"name":c["name"],"ip":c["ipAddress"],"type":c["type"],"connected_at":c["connectedAt"],"uplink_id":c["uplinkDeviceId"]});
        sqlx::query("INSERT INTO unifi_clients(mac,data,last_seen) VALUES($1,$2,$3) ON CONFLICT(mac) DO UPDATE SET data=$2,last_seen=$3")
            .bind(mac).bind(data.to_string()).bind(Utc::now().timestamp()).execute(&state.db).await?;
    }
    snapshot["connected_clients"] = json!(discovered.len());
    snapshot["site_id"] = json!(site);
    // Read-only optional capabilities may be missing with a restricted key.
    for (key, path) in [
        ("zones", "firewall/zones"),
        ("networks", "networks"),
        ("devices", "devices"),
    ] {
        match cfg.list(&format!("{root}/{path}")).await {
            Ok(data) => snapshot[key] = json!(data),
            Err(e) => snapshot[format!("{key}_error")] = json!(e.to_string()),
        }
    }
    // Infrastructure rates are not per-child traffic totals or screen time.
    use futures_util::{StreamExt, stream};
    let devices = statistics_devices(&snapshot);
    let statistics: Vec<Value> = stream::iter(devices)
        .map(|(id, name)| {
            let cfg = &cfg;
            let root = &root;
            async move {
                match cfg
                    .request(
                        Method::GET,
                        &format!("{root}/devices/{id}/statistics/latest"),
                        None,
                    )
                    .await
                {
                    Ok(stats) => json!({"id":id,"name":name,"cpu":stats["cpuUtilizationPct"],
                    "memory":stats["memoryUtilizationPct"],"uptime":stats["uptimeSec"],
                    "rx_bps":stats["uplink"]["rxRateBps"],"tx_bps":stats["uplink"]["txRateBps"]}),
                    Err(e) => json!({"id":id,"name":name,"error":e.to_string()}),
                }
            }
        })
        .buffer_unordered(4)
        .collect()
        .await;
    snapshot["statistics"] = json!(statistics);
    snapshot["protected_macs"] = json!(cfg.protected);
    let policies = cfg.list(&format!("{root}/firewall/policies")).await;
    match &policies {
        Ok(p) => {
            snapshot["firewall_api_available"] = json!(true);
            snapshot["policy_count"] = json!(p.len());
        }
        Err(e) => {
            snapshot["firewall_api_available"] = json!(false);
            snapshot["firewall_error"] = json!(e.to_string());
        }
    }
    *state.unifi.snapshot.write().await = snapshot;
    let owner: String = sqlx::query_scalar("SELECT value FROM unifi_meta WHERE name='owner'")
        .fetch_one(&state.db)
        .await?;
    for c in clients(&state.db).await? {
        let mac = c["mac"].as_str().unwrap();
        let result = reconcile(state, &cfg, &root, &owner, &c, policies.as_ref().ok()).await;
        let status = match result {
            Ok(s) => s,
            Err(e) => format!("error: {e}"),
        };
        if c["result"].as_str() != Some(&status) {
            event(&state.db, mac, &status).await?;
        }
        sqlx::query("UPDATE unifi_clients SET result=$1 WHERE mac=$2")
            .bind(status)
            .bind(mac)
            .execute(&state.db)
            .await?;
    }
    sqlx::query("DELETE FROM unifi_events WHERE at < $1")
        .bind(Utc::now().timestamp() - 30 * 86400)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn reconcile(
    state: &AppState,
    cfg: &Config,
    root: &str,
    owner: &str,
    c: &Value,
    policies: Option<&Vec<Value>>,
) -> Result<String> {
    let mac = c["mac"].as_str().context("Missing MAC")?;
    let profile = c["profile_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok());
    let profile = match profile {
        Some(id) if db::get_profile(&state.db, id).await?.is_some() => Some(id),
        _ => None,
    };
    let excluded = c["excluded"] == true || cfg.protected.contains(mac);
    let want = if excluded || profile.is_none() {
        false
    } else {
        match c["mode"].as_str() {
            Some("block") => true,
            Some("follow") => blocked(&state.db, profile.unwrap()).await?,
            _ => false,
        }
    };
    let policy_id = c["policy_id"].as_str();
    if !cfg.enforce {
        return Ok(if policy_id.is_some() {
            "disabled: existing rule may still block; enable enforcement and release first"
        } else if want {
            "preview: would block internet"
        } else {
            "preview: allow"
        }
        .into());
    }
    let Some(policies) = policies else {
        bail!("Firewall API unavailable; no changes made")
    };
    let (Some(source), Some(destination)) = (cfg.source, cfg.destination) else {
        bail!("Select source and external destination zone IDs")
    };
    if source == destination {
        bail!("Source and destination zones must differ")
    }
    let name = format!("ScreenGuard-{owner}-{}", mac.replace(':', ""));
    let expected = rule(&name, mac, source, destination);
    // Name is persisted before creation so a crash/timeout can recover the result without duplicate rules.
    let saved_name: Option<String> =
        sqlx::query_scalar("SELECT policy_name FROM unifi_clients WHERE mac=$1")
            .bind(mac)
            .fetch_one(&state.db)
            .await?;
    let found = policies
        .iter()
        .find(|p| p["id"].as_str() == policy_id && policy_id.is_some())
        .or_else(|| {
            policies
                .iter()
                .find(|p| saved_name.as_deref() == Some(&name) && p["name"] == name)
        });
    if let Some(actual) = found {
        if !owned_matches(actual, &expected) {
            bail!("Rule ownership or contents changed; manual review required")
        }
        let id = actual["id"].as_str().context("Rule ID missing")?;
        Uuid::parse_str(id)?;
        if !want {
            cfg.request(
                Method::DELETE,
                &format!("{root}/firewall/policies/{id}"),
                None,
            )
            .await?;
            sqlx::query("UPDATE unifi_clients SET policy_id=NULL,policy_name=NULL WHERE mac=$1")
                .bind(mac)
                .execute(&state.db)
                .await?;
            return Ok("own rule removed; other UniFi restrictions unchanged".into());
        }
        sqlx::query("UPDATE unifi_clients SET policy_id=$1 WHERE mac=$2")
            .bind(id)
            .bind(mac)
            .execute(&state.db)
            .await?;
        return Ok("block rule observed; traffic test required".into());
    }
    if !want {
        sqlx::query("UPDATE unifi_clients SET policy_id=NULL,policy_name=NULL WHERE mac=$1")
            .bind(mac)
            .execute(&state.db)
            .await?;
        return Ok("no ScreenGuard block rule".into());
    }
    sqlx::query("UPDATE unifi_clients SET policy_name=$1 WHERE mac=$2")
        .bind(&name)
        .bind(mac)
        .execute(&state.db)
        .await?;
    let created = cfg
        .request(
            Method::POST,
            &format!("{root}/firewall/policies"),
            Some(&expected),
        )
        .await?;
    let id = created["id"]
        .as_str()
        .context("Create response has no rule ID; will reconcile by name")?;
    Uuid::parse_str(id)?;
    sqlx::query("UPDATE unifi_clients SET policy_id=$1 WHERE mac=$2")
        .bind(id)
        .bind(mac)
        .execute(&state.db)
        .await?;
    Ok("block rule created; verification pending".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::any};
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn fixture() -> (
        Arc<AppState>,
        Config,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let pool = db::tests::test_pool().await;
        init(&pool).await.unwrap();
        let state = AppState::new(pool, "test".into(), 24);
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/proxy/network/integration/v1/{*path}",
                any(|State(c): State<Arc<AtomicUsize>>| async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"id":Uuid::new_v4()}))
                }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = Config {
            base: format!("http://{addr}"),
            site: Some(Uuid::new_v4()),
            source: Some(Uuid::new_v4()),
            destination: Some(Uuid::new_v4()),
            enforce: true,
            protected: HashSet::new(),
            client: Client::new(),
        };
        (state, cfg, calls, task)
    }
    async fn bound_client(state: &AppState) -> Value {
        let profile = db::create_profile(&state.db, "Child").await.unwrap();
        sqlx::query(
            "INSERT INTO unifi_clients(mac,data,last_seen) VALUES('02:11:22:33:44:55','{}',0)",
        )
        .execute(&state.db)
        .await
        .unwrap();
        bind(
            &state.db,
            "02:11:22:33:44:55",
            &Binding {
                profile_id: Some(profile.id),
                mode: "block".into(),
                excluded: false,
            },
        )
        .await
        .unwrap();
        clients(&state.db).await.unwrap().remove(0)
    }
    #[tokio::test]
    async fn pagination_handles_server_page_caps_and_sends_key() {
        use axum::{extract::Query, http::HeaderMap, routing::get};
        let app = Router::new().route("/proxy/network/integration/v1/sites", get(
            |Query(q):Query<std::collections::HashMap<String,String>>,headers:HeaderMap| async move {
                assert_eq!(headers.get("X-API-KEY").unwrap(),"fixture-key");
                let offset:usize=q["offset"].parse().unwrap();
                Json(json!({"totalCount":5,"data":(offset..(offset+2).min(5)).map(|i|json!({"id":i})).collect::<Vec<_>>()}))
            }
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "X-API-KEY",
            reqwest::header::HeaderValue::from_static("fixture-key"),
        );
        let cfg = Config {
            base: format!("http://{addr}"),
            site: None,
            source: None,
            destination: None,
            enforce: false,
            protected: HashSet::new(),
            client: Client::builder().default_headers(headers).build().unwrap(),
        };
        let result = cfg.list("/sites").await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|r| r["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        task.abort();
    }

    #[test]
    fn validates_mac_identity() {
        assert_eq!(
            normalize_mac("02-AA-bb-cc-DD-ee").unwrap(),
            "02:aa:bb:cc:dd:ee"
        );
        for invalid in [
            "ff:ff:ff:ff:ff:ff",
            "01:00:00:00:00:00",
            "00:00:00:00:00:00",
            "../../sites",
            "123",
        ] {
            assert!(normalize_mac(invalid).is_err())
        }
    }
    #[test]
    fn limits_adjustments_and_windows() {
        assert!(policy_blocked(60, 0, 3600, true));
        assert!(!policy_blocked(60, 10, 3600, true));
        assert!(policy_blocked(60, 10, 0, false));
        assert!(policy_blocked(0, 0, 0, true));
        assert!(!policy_blocked(60, 0, 3599, true));
    }
    #[test]
    fn exact_rule_ownership_and_dual_stack() {
        let expected = rule("owned", "02:11:22:33:44:55", Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(expected["ipProtocolScope"]["ipVersion"], "IPV4_AND_IPV6");
        let mut actual = expected.clone();
        actual["id"] = json!(Uuid::new_v4());
        assert!(owned_matches(&actual, &expected));
        actual["ipProtocolScope"]["protocolFilter"] = Value::Null;
        actual["source"]["trafficFilter"]["portFilter"] = Value::Null;
        assert!(owned_matches(&actual, &expected));
        actual["schedule"] = json!({"mode":"ALWAYS"});
        assert!(!owned_matches(&actual, &expected));
        actual = expected.clone();
        actual["destination"]["zoneId"] = json!(Uuid::new_v4());
        assert!(!owned_matches(&actual, &expected));
    }
    #[tokio::test]
    async fn preview_and_exclusion_never_write() {
        let (state, mut cfg, calls, task) = fixture().await;
        let c = bound_client(&state).await;
        cfg.enforce = false;
        assert!(
            reconcile(&state, &cfg, "/sites/test", "owner", &c, Some(&vec![]))
                .await
                .unwrap()
                .contains("would block")
        );
        cfg.enforce = true;
        cfg.protected.insert(c["mac"].as_str().unwrap().into());
        reconcile(&state, &cfg, "/sites/test", "owner", &c, Some(&vec![]))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
    #[tokio::test]
    async fn create_recover_and_release_own_rule() {
        let (state, cfg, calls, task) = fixture().await;
        let c = bound_client(&state).await;
        reconcile(&state, &cfg, "/sites/test", "owner", &c, Some(&vec![]))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut c = clients(&state.db).await.unwrap().remove(0);
        let mut actual = rule(
            "ScreenGuard-owner-021122334455",
            c["mac"].as_str().unwrap(),
            cfg.source.unwrap(),
            cfg.destination.unwrap(),
        );
        actual["id"] = c["policy_id"].clone();
        // Simulate crash after remote creation, before storing its ID.
        c["policy_id"] = Value::Null;
        reconcile(
            &state,
            &cfg,
            "/sites/test",
            "owner",
            &c,
            Some(&vec![actual.clone()]),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        c = clients(&state.db).await.unwrap().remove(0);
        c["mode"] = json!("observe");
        reconcile(
            &state,
            &cfg,
            "/sites/test",
            "owner",
            &c,
            Some(&vec![actual]),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(clients(&state.db).await.unwrap()[0]["policy_id"].is_null());
        task.abort();
    }
    #[tokio::test]
    async fn changed_rule_is_never_deleted() {
        let (state, cfg, calls, task) = fixture().await;
        let mut c = bound_client(&state).await;
        c["policy_id"] = json!(Uuid::new_v4());
        c["excluded"] = json!(true);
        let actual = json!({"id":c["policy_id"],"name":"someone else's rule"});
        assert!(
            reconcile(
                &state,
                &cfg,
                "/sites/test",
                "owner",
                &c,
                Some(&vec![actual])
            )
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
    #[tokio::test]
    async fn profile_deletion_releases_not_recreates() {
        let (state, cfg, calls, task) = fixture().await;
        let c = bound_client(&state).await;
        db::delete_profile(
            &state.db,
            Uuid::parse_str(c["profile_id"].as_str().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        reconcile(&state, &cfg, "/sites/test", "owner", &c, Some(&vec![]))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
    #[tokio::test]
    async fn missing_capability_does_not_write() {
        let (state, cfg, calls, task) = fixture().await;
        let c = bound_client(&state).await;
        assert!(
            reconcile(&state, &cfg, "/sites/test", "owner", &c, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            bind(
                &state.db,
                c["mac"].as_str().unwrap(),
                &Binding {
                    profile_id: None,
                    mode: "block".into(),
                    excluded: false
                }
            )
            .await
            .is_err()
        );
        task.abort();
    }
}
