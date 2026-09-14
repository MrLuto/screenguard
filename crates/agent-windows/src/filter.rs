//! Driver-free per-user HTTP(S) enforcement: force external web connections
//! through SID-authenticated local proxies. WFP rules are dynamic and replaced
//! transactionally. IPv4 and IPv6 receive identical rules.
use crate::native::{LocalAlloc, wide};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::Arc,
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use windows_sys::Win32::{
    Foundation::HANDLE, NetworkManagement::WindowsFilteringPlatform::*,
    Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
};

struct Proxy {
    port: u16,
    tx: watch::Sender<Vec<String>>,
    task: JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub struct Filter {
    engine: HANDLE,
    ids: Vec<u64>,
    proxies: HashMap<u32, Proxy>,
}
// Only the owning runtime mutates the engine; WFP handles support cross-thread calls.
unsafe impl Send for Filter {}
fn check(code: u32) -> Result<()> {
    ensure!(code == 0, "WFP operation failed: 0x{code:08x}");
    Ok(())
}
impl Filter {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut engine = null_mut();
            let mut session: FWPM_SESSION0 = zeroed();
            session.flags = FWPM_SESSION_FLAG_DYNAMIC;
            check(FwpmEngineOpen0(null(), 10, null(), &session, &mut engine))?;
            Ok(Self {
                engine,
                ids: Vec::new(),
                proxies: HashMap::new(),
            })
        }
    }
    pub fn port(&self, uid: u32) -> Option<u16> {
        self.proxies.get(&uid).map(|p| p.port)
    }
    pub async fn apply(&mut self, configs: Vec<(u32, String, Vec<String>)>) -> Result<()> {
        let active: Vec<_> = configs
            .into_iter()
            .filter(|(_, _, domains)| !domains.is_empty())
            .collect();
        // New proxies are bound before any WFP changes. If staging fails, existing
        // restrictions remain intact. Ports remain stable across policy updates.
        let mut staged = HashMap::new();
        for (uid, sid, domains) in &active {
            if self.proxies.contains_key(uid) {
                continue;
            }
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let port = listener.local_addr()?.port();
            let (tx, rx) = watch::channel(domains.clone());
            let sid = sid.clone();
            let task = tokio::spawn(async move {
                let auth = Arc::new(move |peer, local| {
                    crate::native::socket_owner(peer, local).is_ok_and(|s| s == sid)
                });
                if let Err(e) = agent_core::proxy::serve(listener, rx, auth).await {
                    tracing::error!("Proxy stopped: {e:#}");
                }
            });
            staged.insert(*uid, Proxy { port, tx, task });
        }
        let mut ids = Vec::new();
        unsafe {
            check(FwpmTransactionBegin0(self.engine, 0))?;
        }
        let result = (|| {
            for id in &self.ids {
                unsafe {
                    check(FwpmFilterDeleteById0(self.engine, *id))?;
                }
            }
            for (_, sid, _) in &active {
                self.add_user_rules(sid, &mut ids)?;
            }
            unsafe { check(FwpmTransactionCommit0(self.engine)) }
        })();
        if let Err(e) = result {
            unsafe {
                FwpmTransactionAbort0(self.engine);
            }
            return Err(e);
        }
        self.ids = ids;
        self.proxies
            .retain(|uid, _| active.iter().any(|(u, _, _)| u == uid));
        self.proxies.extend(staged);
        for (uid, _, domains) in active {
            self.proxies[&uid].tx.send_replace(domains);
        }
        Ok(())
    }
    fn add_user_rules(&self, sid: &str, ids: &mut Vec<u64>) -> Result<()> {
        unsafe {
            let sddl = wide(&format!("D:(A;;CC;;;{sid})"));
            let mut sd = null_mut();
            let mut len = 0;
            ensure!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut sd,
                    &mut len
                ) != 0,
                "Invalid user descriptor"
            );
            let _guard = LocalAlloc(sd);
            let mut blob = FWP_BYTE_BLOB {
                size: len,
                data: sd.cast(),
            };
            for layer in [
                FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                FWPM_LAYER_ALE_AUTH_CONNECT_V6,
            ] {
                // TCP HTTP/HTTPS and encrypted DNS, plus QUIC and classic DNS.
                for (protocol, port) in [
                    (6u8, 80u16),
                    (6, 443),
                    (6, 853),
                    (6, 53),
                    (17, 443),
                    (17, 853),
                    (17, 53),
                ] {
                    let mut conditions = [
                        FWPM_FILTER_CONDITION0 {
                            fieldKey: FWPM_CONDITION_ALE_USER_ID,
                            matchType: FWP_MATCH_EQUAL,
                            conditionValue: FWP_CONDITION_VALUE0 {
                                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                                Anonymous: FWP_CONDITION_VALUE0_0 { sd: &mut blob },
                            },
                        },
                        FWPM_FILTER_CONDITION0 {
                            fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                            matchType: FWP_MATCH_EQUAL,
                            conditionValue: FWP_CONDITION_VALUE0 {
                                r#type: FWP_UINT8,
                                Anonymous: FWP_CONDITION_VALUE0_0 { uint8: protocol },
                            },
                        },
                        FWPM_FILTER_CONDITION0 {
                            fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                            matchType: FWP_MATCH_EQUAL,
                            conditionValue: FWP_CONDITION_VALUE0 {
                                r#type: FWP_UINT16,
                                Anonymous: FWP_CONDITION_VALUE0_0 { uint16: port },
                            },
                        },
                        FWPM_FILTER_CONDITION0 {
                            fieldKey: FWPM_CONDITION_FLAGS,
                            matchType: FWP_MATCH_FLAGS_NONE_SET,
                            conditionValue: FWP_CONDITION_VALUE0 {
                                r#type: FWP_UINT32,
                                Anonymous: FWP_CONDITION_VALUE0_0 {
                                    uint32: FWP_CONDITION_FLAG_IS_LOOPBACK,
                                },
                            },
                        },
                    ];
                    let mut name = wide("ScreenGuard per-user web filter");
                    let mut filter: FWPM_FILTER0 = zeroed();
                    filter.displayData.name = name.as_mut_ptr();
                    filter.layerKey = layer;
                    filter.subLayerKey = FWPM_SUBLAYER_UNIVERSAL;
                    filter.action.r#type = FWP_ACTION_BLOCK;
                    filter.weight.r#type = FWP_UINT8;
                    filter.weight.Anonymous.uint8 = 15;
                    filter.numFilterConditions = conditions.len() as u32;
                    filter.filterCondition = conditions.as_mut_ptr();
                    let mut id = 0;
                    check(FwpmFilterAdd0(self.engine, &filter, null_mut(), &mut id))?;
                    ids.push(id);
                }
            }
            Ok(())
        }
    }
}
impl Drop for Filter {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.engine);
        }
    }
}
