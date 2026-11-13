//! Local control socket for the CLI and UI: JSON lines over a Unix socket at
//! `$XDG_RUNTIME_DIR/test-bed/share.sock`, or a named pipe on Windows.
//!
//! Requests: `{"cmd":"status"}`, `{"cmd":"pair"}`, `{"cmd":"invite"}`,
//! `{"cmd":"cancel_pair"}`, `{"cmd":"list"}`, `{"cmd":"unpair","name":"..."}`,
//! `{"cmd":"disconnect"}`. Each request gets one JSON response line; `pair`
//! and `invite` (pairing over the internet) additionally stream events until
//! pairing succeeds, fails or expires.

use std::sync::Arc;

use anyhow::Result;
use ac_remote::trust::AuthorizedClients;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use ac_remote::ipc::{self, Reader, Writer};

use crate::server::{Host, PairEvent};

#[derive(Deserialize)]
struct Request {
    cmd: String,
    #[serde(default)]
    name: Option<String>,
}

pub async fn serve(host: Arc<Host>) -> Result<()> {
    let mut listener = ipc::Listener::bind()?;
    loop {
        let sock = listener.accept().await?;
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(host, sock).await {
                tracing::debug!("control client: {e:#}");
            }
        });
    }
}

pub fn status_json(host: &Host) -> Value {
    let paired = paired_json();
    json!({
        "running": true,
        "name": host.name,
        "listen": host.listen.to_string(),
        "port": host.listen.port(),
        "fingerprint": host.identity.fingerprint,
        "pairing_open": host.pairing_open(),
        "client": host.client_status(),
        "paired": paired,
        "internet": host.internet_status(),
    })
}

/// Paired clients, LAN first, then those paired over the internet.
pub fn paired_json() -> Value {
    let mut out = Vec::new();
    for (internet, store) in [(false, AuthorizedClients::load()), (true, AuthorizedClients::load_internet())] {
        match store {
            Ok(a) => out.extend(a.clients.iter().map(|c| {
                json!({
                    "name": c.name,
                    "fingerprint": c.fingerprint,
                    "fingerprint_short": ac_remote::identity::short_fingerprint(&c.fingerprint),
                    "paired_at": c.paired_at,
                    "internet": internet,
                })
            })),
            Err(e) => return json!({ "error": format!("{e:#}") }),
        }
    }
    json!(out)
}

/// Remove a paired client from both lists; true if one was removed.
pub fn unpair(name: &str) -> Result<bool> {
    let mut removed = false;
    for mut store in [AuthorizedClients::load()?, AuthorizedClients::load_internet()?] {
        if store.remove(name) {
            store.save()?;
            removed = true;
        }
    }
    Ok(removed)
}

async fn send(w: &mut Writer, v: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn handle(host: Arc<Host>, (r, mut w): (Reader, Writer)) -> Result<()> {
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut w, &json!({ "error": format!("bad request: {e}") })).await?;
                continue;
            }
        };
        match req.cmd.as_str() {
            "status" => send(&mut w, &status_json(&host)).await?,
            "list" => send(&mut w, &json!({ "paired": paired_json() })).await?,
            "unpair" => {
                let name = req.name.unwrap_or_default();
                let removed = unpair(&name)?;
                send(&mut w, &json!({ "removed": removed, "name": name })).await?;
            }
            "disconnect" => send(&mut w, &json!({ "disconnected": host.disconnect_client() })).await?,
            "cancel_pair" => {
                host.close_pairing();
                host.close_invite();
                send(&mut w, &json!({ "cancelled": true })).await?;
            }
            "pair" | "invite" => {
                let invite = req.cmd == "invite";
                let close = |h: &Host| if invite { h.close_invite() } else { h.close_pairing() };
                let mut events = host.pair_events.subscribe();
                let win = if invite {
                    match host.open_invite() {
                        Ok((code, win)) => {
                            send(&mut w, &json!({ "invite": code, "pin": win.pin, "expires_in": win.remaining().as_secs() })).await?;
                            win
                        }
                        Err(e) => {
                            send(&mut w, &json!({ "error": format!("{e:#}") })).await?;
                            continue;
                        }
                    }
                } else {
                    let win = host.open_pairing();
                    send(&mut w, &json!({ "pin": win.pin, "expires_in": win.remaining().as_secs() })).await?;
                    win
                };
                let deadline = tokio::time::sleep(win.remaining());
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => {
                            close(&host);
                            send(&mut w, &json!({ "event": "expired" })).await?;
                            break;
                        }
                        ev = events.recv() => match ev {
                            Ok(ev) => {
                                send(&mut w, &serde_json::to_value(&ev)?).await?;
                                if matches!(ev, PairEvent::Paired { .. } | PairEvent::Expired) { break; }
                            }
                            Err(_) => break,
                        },
                        // Requester went away: close the window.
                        l = lines.next_line() => {
                            if matches!(l, Ok(None) | Err(_)) { close(&host); return Ok(()); }
                            if let Ok(Some(l)) = l {
                                if l.contains("cancel_pair") {
                                    close(&host);
                                    send(&mut w, &json!({ "event": "cancelled" })).await?;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            other => send(&mut w, &json!({ "error": format!("unknown command {other:?}") })).await?,
        }
    }
    Ok(())
}

/// Client side, used by the `test-bed-share` CLI: connect to a running daemon.
pub async fn connect() -> Option<(Reader, Writer)> {
    ipc::connect().await.ok()
}
