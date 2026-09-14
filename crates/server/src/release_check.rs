use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

#[derive(Default, Debug)]
pub struct Releases { pub linux: Option<String>, pub windows: Option<String> }
impl Releases {
    pub fn for_platform(&self, platform: &str) -> Option<&str> {
        if platform == "windows" { self.windows.as_deref() } else { self.linux.as_deref() }
    }
}
pub type LatestRelease = Arc<RwLock<Releases>>;

pub fn parse_semver(v: &str) -> Option<(u32, u32, u32)> {
    let mut p = v.split('.');
    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?))
}

pub fn is_older(agent: &str, latest: &str) -> bool {
    match (parse_semver(agent), parse_semver(latest)) {
        (Some(a), Some(l)) => a < l,
        _ => agent != latest,
    }
}

/// Fetch the highest `v<major>.<minor>.<patch>` release from GitHub, ignoring
/// mobile/other tags so that `mobile0.0.x` releases don't appear as "latest".
async fn fetch_latest(repo: &str) -> Option<Releases> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=50");
    let client = reqwest::Client::builder()
        .user_agent("screenguard-server/release-check")
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let releases: Vec<serde_json::Value> = client
        .get(&url)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(Releases {
        linux: latest_asset_release(&releases, "screenguard-agent-x86_64"),
        windows: latest_asset_release(&releases, "screenguard-windows-x86_64.zip"),
    })
}

fn latest_asset_release(releases: &[serde_json::Value], asset: &str) -> Option<String> {
    releases.iter().filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter(|r| r["assets"].as_array().is_some_and(|a| a.iter().any(|v| v["name"] == asset)))
        .filter_map(|r| r["tag_name"].as_str()?.strip_prefix('v'))
        .filter_map(|v| Some((parse_semver(v)?,v.to_owned())))
        .max_by_key(|(v,_)| *v).map(|(_,v)| v)
}

/// Spawn a background task that refreshes the cached latest release every hour.
/// The first fetch happens immediately.
pub fn spawn_updater(repo: &'static str, cache: LatestRelease) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            match fetch_latest(repo).await {
                Some(v) => {
                    tracing::info!("Latest agent releases on GitHub: {v:?}");
                    *cache.write().await = v;
                }
                None => tracing::warn!("Could not fetch latest release from GitHub"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_offer_available_platform_assets_and_stable_releases() {
        let releases = serde_json::json!([
            {"tag_name":"v0.11.0","assets":[{"name":"screenguard-agent-x86_64"}]},
            {"tag_name":"v0.10.9","assets":[{"name":"screenguard-windows-x86_64.zip"}]},
            {"tag_name":"v0.12.0","prerelease":true,"assets":[{"name":"screenguard-windows-x86_64.zip"}]},
            {"tag_name":"v0.13.0","draft":true,"assets":[{"name":"screenguard-windows-x86_64.zip"}]}
        ]);
        assert_eq!(latest_asset_release(releases.as_array().unwrap(),"screenguard-windows-x86_64.zip").as_deref(),Some("0.10.9"));
        assert_eq!(latest_asset_release(releases.as_array().unwrap(),"screenguard-agent-x86_64").as_deref(),Some("0.11.0"));
    }
}
