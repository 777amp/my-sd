// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Pure-Rust Signal Protocol engine powered by `presage` + `presage-store-sqlite`.
//!
//! Handles real Signal QR device linking (`sgnl://linkdevice?...`), SVG QR rendering,
//! live E2EE message receiving & multi-device sync, and real E2EE 1:1 & group sending.
//!
//! Runs `presage::Manager` inside a dedicated single-threaded Tokio `LocalSet` worker
//! thread because `presage` futures are `!Send`.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use futures::{channel::oneshot as futures_oneshot, future, pin_mut, StreamExt};
use parking_lot::RwLock;
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::proto::sync_message::Content as SyncContent;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::zkgroup::GroupMasterKeyBytes;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::ContentsStore;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use qrcode::render::svg;
use qrcode::QrCode;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

pub type UiEventCallback = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SignalStatus {
    pub linked: bool,
    pub linking: bool,
    pub aci: Option<String>,
    pub phone: Option<String>,
    pub device_name: String,
    pub qr_url: Option<String>,
    pub qr_svg: Option<String>,
    pub last_error: Option<String>,
}

enum WorkerCmd {
    StartLinking {
        device_name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SendMessage {
        recipient_type: String,
        recipient_id: String,
        body: String,
        reply: oneshot::Sender<Result<Value>>,
    },
    ListContactsAndGroups {
        reply: oneshot::Sender<Result<Value>>,
    },
}

pub struct SignalClient {
    status: Arc<RwLock<SignalStatus>>,
    cmd_tx: mpsc::UnboundedSender<WorkerCmd>,
}

impl SignalClient {
    pub fn spawn(
        user_data_dir: PathBuf,
        sql_key: String,
        on_event: UiEventCallback,
    ) -> Self {
        let status = Arc::new(RwLock::new(SignalStatus {
            device_name: "Signal-Pi4".into(),
            ..Default::default()
        }));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let status_bg = Arc::clone(&status);
        std::thread::Builder::new()
            .name("presage-signal-worker".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build presage Tokio runtime");
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    worker_main(user_data_dir, sql_key, status_bg, on_event, cmd_rx).await;
                });
            })
            .expect("Failed to spawn presage worker thread");

        Self { status, cmd_tx }
    }

    pub fn status(&self) -> SignalStatus {
        self.status.read().clone()
    }

    pub async fn start_linking(&self, device_name: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(WorkerCmd::StartLinking {
                device_name,
                reply: tx,
            })
            .map_err(|_| anyhow!("Signal worker thread stopped"))?;
        rx.await.map_err(|_| anyhow!("Signal worker dropped reply"))?
    }

    pub async fn send_message(
        &self,
        recipient_type: String,
        recipient_id: String,
        body: String,
    ) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(WorkerCmd::SendMessage {
                recipient_type,
                recipient_id,
                body,
                reply: tx,
            })
            .map_err(|_| anyhow!("Signal worker thread stopped"))?;
        rx.await.map_err(|_| anyhow!("Signal worker dropped reply"))?
    }

    pub async fn list_contacts_and_groups(&self) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(WorkerCmd::ListContactsAndGroups { reply: tx })
            .map_err(|_| anyhow!("Signal worker thread stopped"))?;
        rx.await.map_err(|_| anyhow!("Signal worker dropped reply"))?
    }
}

/// Renders a crisp, scannable SVG QR code with white quiet zone for `url`.
pub fn render_qr_svg(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes())
        .map_err(|e| anyhow!("Failed to encode QR code: {e}"))?;
    let svg_str = code
        .render::<svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(svg_str)
}

async fn worker_main(
    user_data_dir: PathBuf,
    sql_key: String,
    status: Arc<RwLock<SignalStatus>>,
    on_event: UiEventCallback,
    mut cmd_rx: mpsc::UnboundedReceiver<WorkerCmd>,
) {
    let db_path = user_data_dir.join("presage.db");
    let db_path_str = db_path.to_string_lossy().into_owned();
    info!("Opening pure-Rust Signal store at {db_path_str}");

    let store = match SqliteStore::open_with_passphrase(
        &db_path_str,
        Some(sql_key.as_str()),
        OnNewIdentity::Trust,
    )
    .await
    {
        Ok(s) => s,
        Err(err) => {
            error!("Failed to open presage SqliteStore: {err:#}");
            status.write().last_error = Some(format!("Failed to open presage store: {err:#}"));
            return;
        }
    };

    let mut manager: Option<Manager<SqliteStore, Registered>> = None;

    // Auto-load if already linked on a previous run
    match Manager::load_registered(store.clone()).await {
        Ok(mgr) => {
            info!("Loaded existing pure-Rust linked Signal device");
            update_whoami(&mgr, &status, &on_event).await;
            spawn_receive_loop(mgr.clone(), Arc::clone(&on_event));
            manager = Some(mgr);
        }
        Err(err) => {
            info!("No linked pure-Rust Signal device yet ({err}); awaiting QR link");
        }
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            WorkerCmd::StartLinking { device_name, reply } => {
                if status.read().linking {
                    let _ = reply.send(Ok(()));
                    continue;
                }

                {
                    let mut st = status.write();
                    st.linking = true;
                    st.device_name = device_name.clone();
                    st.last_error = None;
                    st.qr_url = None;
                    st.qr_svg = None;
                }

                // Acknowledge command immediately so Tauri IPC doesn't block while waiting for scan
                let _ = reply.send(Ok(()));

                let (link_tx, link_rx) = futures_oneshot::channel::<url::Url>();
                let status_qr = Arc::clone(&status);
                let on_event_qr = Arc::clone(&on_event);

                let qr_task = async move {
                    match link_rx.await {
                        Ok(url) => {
                            let url_str = url.to_string();
                            info!("Received real Signal provisioning URL");
                            match render_qr_svg(&url_str) {
                                Ok(svg) => {
                                    {
                                        let mut st = status_qr.write();
                                        st.qr_url = Some(url_str.clone());
                                        st.qr_svg = Some(svg.clone());
                                    }
                                    on_event_qr(
                                        "signal:qr",
                                        json!({ "url": url_str, "svg": svg }),
                                    );
                                }
                                Err(err) => {
                                    error!("QR SVG render failed: {err:#}");
                                    on_event_qr(
                                        "signal:link-error",
                                        json!({ "error": format!("{err:#}") }),
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            warn!("Provisioning link channel closed: {err}");
                        }
                    }
                };

                let link_result = future::join(
                    Manager::link_secondary_device(
                        store.clone(),
                        SignalServers::Production,
                        device_name,
                        link_tx,
                    ),
                    qr_task,
                )
                .await
                .0;

                match link_result {
                    Ok(mgr) => {
                        info!("Successfully linked secondary Signal device via QR!");
                        {
                            let mut st = status.write();
                            st.linking = false;
                            st.linked = true;
                            st.qr_url = None;
                            st.qr_svg = None;
                            st.last_error = None;
                        }
                        update_whoami(&mgr, &status, &on_event).await;
                        spawn_receive_loop(mgr.clone(), Arc::clone(&on_event));
                        manager = Some(mgr);
                    }
                    Err(err) => {
                        error!("Signal QR device linking failed: {err:#}");
                        let msg = format!("{err:#}");
                        {
                            let mut st = status.write();
                            st.linking = false;
                            st.last_error = Some(msg.clone());
                        }
                        on_event("signal:link-error", json!({ "error": msg }));
                    }
                }
            }

            WorkerCmd::SendMessage {
                recipient_type,
                recipient_id,
                body,
                reply,
            } => {
                let Some(ref mut mgr) = manager else {
                    let _ = reply.send(Err(anyhow!(
                        "Device is not linked yet. Click 'Link Device (QR)' and scan the QR code with Signal on your phone."
                    )));
                    continue;
                };

                let res = send_e2ee_message(mgr, &recipient_type, &recipient_id, body).await;
                let _ = reply.send(res);
            }

            WorkerCmd::ListContactsAndGroups { reply } => {
                let res = list_store_threads(&store).await;
                let _ = reply.send(res);
            }
        }
    }
}

async fn update_whoami(
    mgr: &Manager<SqliteStore, Registered>,
    status: &Arc<RwLock<SignalStatus>>,
    on_event: &UiEventCallback,
) {
    match mgr.whoami().await {
        Ok(who) => {
            let aci = who.aci.to_string();
            let phone = Some(who.number.to_string());
            {
                let mut st = status.write();
                st.linked = true;
                st.aci = Some(aci.clone());
                st.phone = phone.clone();
            }
            on_event(
                "signal:linked",
                json!({ "linked": true, "aci": aci, "phone": phone }),
            );
        }
        Err(err) => {
            warn!("whoami failed after load: {err:#}");
            status.write().linked = true;
            on_event("signal:linked", json!({ "linked": true }));
        }
    }
}

fn spawn_receive_loop(
    mut mgr: Manager<SqliteStore, Registered>,
    on_event: UiEventCallback,
) {
    tokio::task::spawn_local(async move {
        loop {
            info!("Starting pure-Rust Signal live receive_messages stream...");
            match mgr.receive_messages().await {
                Ok(stream) => {
                    pin_mut!(stream);
                    while let Some(item) = stream.next().await {
                        match item {
                            Received::QueueEmpty => {
                                info!("Signal receive queue synchronized");
                                on_event("signal:synced", json!({ "ok": true }));
                            }
                            Received::Content(content) => {
                                emit_received_content(&content, &on_event);
                            }
                            Received::DecryptionError(err) => {
                                warn!("Signal decryption error: {err:?}");
                            }
                        }
                    }
                }
                Err(err) => {
                    error!("Signal receive_messages stream error: {err:#}");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

fn emit_received_content(content: &Content, on_event: &UiEventCallback) {
    let ts = content.metadata.client_timestamp.timestamp_millis().max(0) as u64;
    let sender = content.metadata.sender.raw_uuid().to_string();

    match &content.body {
        ContentBody::DataMessage(dm) => {
            if let Some(ref body) = dm.body {
                on_event(
                    "signal:live-message",
                    json!({
                        "direction": "incoming",
                        "senderServiceId": sender,
                        "groupMasterKey": dm.group_v2.as_ref().and_then(|g| g.master_key.as_ref()).map(hex::encode),
                        "body": body,
                        "timestamp": ts,
                    }),
                );
            }
        }
        ContentBody::SynchronizeMessage(sync) => {
            if let Some(SyncContent::Sent(sent)) = &sync.content {
                if let Some(body) = sent.message.as_ref().and_then(|m| m.body.as_ref()) {
                    on_event(
                        "signal:live-message",
                        json!({
                            "direction": "outgoing",
                            "destinationServiceId": sent.destination_service_id,
                            "destinationE164": sent.destination_e164,
                            "groupMasterKey": sent.message.as_ref()
                                .and_then(|m| m.group_v2.as_ref())
                                .and_then(|g| g.master_key.as_ref())
                                .map(hex::encode),
                            "body": body,
                            "timestamp": sent.timestamp.unwrap_or(ts),
                        }),
                    );
                }
            }
        }
        _ => {}
    }
}

async fn send_e2ee_message(
    mgr: &mut Manager<SqliteStore, Registered>,
    recipient_type: &str,
    recipient_id: &str,
    body: String,
) -> Result<Value> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    if recipient_type == "group" {
        let key_bytes = decode_group_master_key(recipient_id)?;
        let data_message = DataMessage {
            body: Some(body),
            timestamp: Some(timestamp),
            group_v2: Some(GroupContextV2 {
                master_key: Some(key_bytes.to_vec()),
                revision: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        mgr.send_message_to_group(&key_bytes, ContentBody::DataMessage(data_message), timestamp)
            .await
            .map_err(|e| anyhow!("Failed to send group message: {e:#}"))?;
        return Ok(json!({ "sent": true, "timestamp": timestamp, "recipientType": "group" }));
    }

    // 1:1 contact — parse ServiceId ("ACI:uuid" or plain UUID)
    let service_id = parse_recipient_service_id(recipient_id)?;
    let data_message = DataMessage {
        body: Some(body),
        timestamp: Some(timestamp),
        ..Default::default()
    };

    mgr.send_message(service_id, ContentBody::DataMessage(data_message), timestamp)
        .await
        .map_err(|e| anyhow!("Failed to send Signal message: {e:#}"))?;

    Ok(json!({ "sent": true, "timestamp": timestamp, "recipientType": "contact" }))
}

fn parse_recipient_service_id(raw: &str) -> Result<ServiceId> {
    let trimmed = raw.trim();
    if let Some(sid) = ServiceId::parse_from_service_id_string(trimmed) {
        return Ok(sid);
    }
    let clean = trimmed
        .strip_prefix("ACI:")
        .or_else(|| trimmed.strip_prefix("aci:"))
        .unwrap_or(trimmed);
    let uuid = Uuid::parse_str(clean)
        .map_err(|e| anyhow!("Recipient '{raw}' is not a valid Signal ACI/UUID: {e}"))?;
    Ok(ServiceId::Aci(uuid.into()))
}

fn decode_group_master_key(raw: &str) -> Result<GroupMasterKeyBytes> {
    let bytes = hex::decode(raw.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(raw.trim()))
        .map_err(|e| anyhow!("Invalid group master key (expected hex or base64): {e}"))?;
    let arr: GroupMasterKeyBytes = bytes
        .try_into()
        .map_err(|_| anyhow!("Group master key must be exactly 32 bytes"))?;
    Ok(arr)
}

async fn list_store_threads(store: &SqliteStore) -> Result<Value> {
    let mut contacts_out = Vec::new();
    if let Ok(iter) = store.contacts().await {
        for c in iter.flatten() {
            contacts_out.push(json!({
                "id": c.uuid.to_string(),
                "serviceId": c.uuid.to_string(),
                "type": "private",
                "name": c.name,
                "e164": c.phone_number.map(|p| p.to_string()),
            }));
        }
    }

    let mut groups_out = Vec::new();
    if let Ok(iter) = store.groups().await {
        for (master_key, group) in iter.flatten() {
            groups_out.push(json!({
                "id": hex::encode(master_key),
                "groupMasterKey": hex::encode(master_key),
                "type": "group",
                "name": group.title,
                "members": group.members.len(),
            }));
        }
    }

    Ok(json!({
        "contacts": contacts_out,
        "groups": groups_out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_qr_svg() {
        let svg = render_qr_svg("sgnl://linkdevice?uuid=test-uuid&pub_key=abc123").unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn test_parse_recipient_service_id() {
        let uuid_str = "12345678-1234-5678-1234-567812345678";
        assert!(parse_recipient_service_id(uuid_str).is_ok());
        assert!(parse_recipient_service_id(&format!("ACI:{uuid_str}")).is_ok());
    }
}
