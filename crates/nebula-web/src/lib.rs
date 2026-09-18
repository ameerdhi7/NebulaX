//! `nebula web`: a browser dashboard for the ticket board.
//!
//! It is an ordinary daemon client, not a daemon feature (execution-plan D4):
//! each browser tab gets its own unix-socket connection to the daemon, and this
//! process translates between the daemon's `Ext` protocol (positional
//! MessagePack over the socket) and clean JSON over a WebSocket. The daemon
//! stays browser-ignorant — the same `Subscribe`/snapshot/deltas the TUI uses.
//!
//! First cut serves a self-contained vanilla-JS page (no build toolchain) that
//! renders the assigned board and can start/sync tickets. A Vite/React SPA
//! embedded via `rust-embed` is the later polish; the bridge below is what it
//! would talk to unchanged.

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use nebula_core::codec::{read_frame, write_frame};
use nebula_core::{paths, ClientRequest, ServerEvent, PROTOCOL_VERSION};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::io::BufReader;
use tokio::net::{TcpListener, UnixStream};

/// How `nebula web` was invoked.
pub struct WebOpts {
    pub port: u16,
    pub bind: IpAddr,
    /// Open a desktop browser on the served URL.
    pub open: bool,
}

impl Default for WebOpts {
    fn default() -> Self {
        Self {
            port: 7690,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            open: true,
        }
    }
}

/// Blocking entry point for the `nebula web` CLI.
pub fn run_web(opts: WebOpts) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(opts))
}

async fn serve(opts: WebOpts) -> Result<()> {
    // Fail fast when no daemon is up — the dashboard is a view of a running one.
    if UnixStream::connect(paths::socket_path()).await.is_err() {
        bail!("no nebula daemon is running — launch nebula first, then `nebula web`");
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::new(opts.bind, opts.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://{}:{port}/", display_host(opts.bind));
    println!("nebula web dashboard on {url}");
    if !opts.bind.is_loopback() && !opts.bind.is_unspecified() {
        eprintln!(
            "warning: bound to a non-loopback address — this serves your board to the network"
        );
    }
    if opts.open {
        open_browser(&url);
    }
    axum::serve(listener, app)
        .await
        .context("web server error")?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(bridge_socket)
}

/// Bridge one browser WebSocket to its own daemon connection: daemon `Ext`
/// events become `{kind, payload}` JSON to the browser, and the browser's
/// `{kind, payload}` messages become `Ext` requests to the daemon.
async fn bridge_socket(mut socket: WebSocket) {
    let daemon = match connect_daemon().await {
        Ok(d) => d,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    format!(r#"{{"kind":"error","payload":"{e}"}}"#).into(),
                ))
                .await;
            return;
        }
    };
    let (read_half, mut write_half) = daemon.into_split();

    // Daemon → browser, on its own task, feeding a channel the main loop drains
    // (so `socket` is only ever touched in one place).
    let (to_ws_tx, mut to_ws_rx) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        while let Ok(Some(ev)) = read_frame::<ServerEvent, _>(&mut reader).await {
            if let ServerEvent::Ext { kind, json, .. } = ev {
                let payload: serde_json::Value =
                    serde_json::from_slice(&json).unwrap_or(serde_json::Value::Null);
                let msg = serde_json::json!({ "kind": kind, "payload": payload }).to_string();
                if to_ws_tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(req) = parse_action(&text) {
                        if write_frame(&mut write_half, &req).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            outgoing = to_ws_rx.recv() => match outgoing {
                Some(msg) => {
                    if socket.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
}

/// A browser `{kind, payload}` message → an `Ext` client request. Unknown or
/// malformed messages are dropped.
fn parse_action(text: &str) -> Option<ClientRequest> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let kind = v.get("kind")?.as_str()?.to_string();
    let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    let json = if payload.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&payload).ok()?
    };
    Some(ClientRequest::Ext {
        req_id: 0,
        kind,
        json,
    })
}

/// Open a daemon connection, handshake, and subscribe — the same first frames
/// the TUI sends.
async fn connect_daemon() -> Result<UnixStream> {
    let mut stream = UnixStream::connect(paths::socket_path())
        .await
        .context("connect daemon socket")?;
    write_frame(
        &mut stream,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;
    match read_frame::<ServerEvent, _>(&mut stream).await? {
        Some(ServerEvent::HelloOk { .. }) => {}
        Some(ServerEvent::Incompatible {
            daemon_protocol_version,
        }) => bail!(
            "protocol mismatch: the daemon speaks v{daemon_protocol_version}, this web bridge v{PROTOCOL_VERSION} — reinstall so both match"
        ),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
    write_frame(&mut stream, &ClientRequest::Subscribe).await?;
    Ok(stream)
}

fn display_host(bind: IpAddr) -> String {
    if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The dashboard page — a self-contained board over the WebSocket bridge.
const INDEX_HTML: &str = include_str!("index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_wraps_kind_and_payload() {
        let req = parse_action(r#"{"kind":"board/sync-now"}"#).unwrap();
        match req {
            ClientRequest::Ext { kind, json, .. } => {
                assert_eq!(kind, "board/sync-now");
                assert!(json.is_empty(), "no payload = empty json");
            }
            _ => panic!("expected Ext"),
        }
        let req = parse_action(r#"{"kind":"board/start","payload":{"tickets":[]}}"#).unwrap();
        match req {
            ClientRequest::Ext { kind, json, .. } => {
                assert_eq!(kind, "board/start");
                let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
                assert!(v.get("tickets").is_some());
            }
            _ => panic!("expected Ext"),
        }
        assert!(parse_action("not json").is_none());
        assert!(parse_action(r#"{"no_kind":1}"#).is_none());
    }
}
