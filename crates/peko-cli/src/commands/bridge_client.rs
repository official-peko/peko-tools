//! The devtools window's client of the running app's bridge.
//!
//! `peko run --devtools` connects to the app's loopback WebSocket bridge as a
//! second client (alongside the web UI). The app publishes this run's bridge URL
//! and token to a dev file, which this reads. Once authenticated, the client
//! forwards the calls the window issues (devtools.eval for the interactive
//! console and view source) and delivers devtools:result events back. The app is
//! killed and respawned on every rebuild, so the connection is re-established
//! whenever it drops, reading the fresh coordinates each time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message;

use crate::commands::devtools::DevEvent;

/// Read `{ url, token }` from the dev-bridge file, or `None` when it is absent
/// or not yet written.
fn read_coordinates(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let url = value.get("url")?.as_str()?.to_string();
    let token = value.get("token")?.as_str()?.to_string();
    Some((url, token))
}

/// Run the bridge client until shutdown. It owns the outgoing-call receiver
/// across reconnects; each connection authenticates, then pumps incoming events
/// and outgoing calls until the socket drops, then retries with the latest
/// coordinates.
pub async fn run(
    bridge_file: PathBuf,
    events: Sender<DevEvent>,
    mut calls: UnboundedReceiver<String>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let Some((url, token)) = read_coordinates(&bridge_file) else {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        };

        let stream = match tokio_tungstenite::connect_async(&url).await {
            Ok((stream, _)) => stream,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
        };
        let (mut write, mut read) = stream.split();

        let auth = format!("{{\"t\":\"auth\",\"token\":\"{token}\"}}");
        if write.send(Message::text(auth)).await.is_err() {
            continue;
        }

        loop {
            tokio::select! {
                incoming = read.next() => {
                    match incoming {
                        Some(Ok(message)) => {
                            if message.is_close() {
                                break;
                            }
                            if let Ok(text) = message.to_text() {
                                handle_incoming(text, &events);
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                outgoing = calls.recv() => {
                    match outgoing {
                        // The dev loop dropped the sender: the session is ending.
                        None => return,
                        Some(message) => {
                            if write.send(Message::text(message)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                }
            }
        }
    }
}

/// Forward the devtools events we care about to the window; ignore the rest.
fn handle_incoming(text: &str, events: &Sender<DevEvent>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if value.get("t").and_then(|v| v.as_str()) != Some("event") {
        return;
    }
    let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let Some(data) = value.get("data") else {
        return;
    };

    match name {
        "devtools:result" => {
            let kind = data
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("eval")
                .to_string();
            let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            let result = data
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = events.send(DevEvent::EvalResult {
                kind,
                ok,
                text: result,
            });
        }
        "devtools:trace" => {
            let dir = data
                .get("dir")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let label = data
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // The payload is arbitrary JSON; keep it as compact text for display.
            let payload = data.get("data").map(|v| v.to_string()).unwrap_or_default();
            let _ = events.send(DevEvent::Trace {
                dir,
                label,
                data: payload,
            });
        }
        _ => {}
    }
}
