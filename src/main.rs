//! voiceapi — a single-client WebSocket service that fully owns a GSM modem and
//! exposes a small, curated SMS API. See docs/plan for the architecture.

mod api;
mod serial;
mod sms;
mod store;

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use chrono::Local;
use serde_json::{json, Value};
use tungstenite::Message;

use api::{ApiError, RequestBody};
use serial::fatal;
use sms::ModemExecutor;
use store::Store;

/// Cached service/modem status, reported by `health` without touching the modem.
struct Status {
    modem: String, // "initializing" | "ready" | "not_ready"
    sim: String,   // "READY" | "unknown"
    started_at: String,
    start: Instant,
}

type SharedStore = Arc<Mutex<Store>>;
type SharedStatus = Arc<Mutex<Status>>;
type SharedExec = Arc<Mutex<Option<ModemExecutor>>>;

fn now_iso() -> String {
    Local::now().to_rfc3339()
}

fn main() {
    let db_path = std::env::var("VOICEAPI_DB").unwrap_or_else(|_| "./voiceapi.db".to_string());
    let bind_addr =
        std::env::var("VOICEAPI_TCP_ADDR").unwrap_or_else(|_| "127.0.0.1:9500".to_string());

    // 1. SQLite — the source of truth. If this fails there is nowhere to record
    //    the failure, so it is the one place we write to stderr and exit.
    let store: SharedStore = match Store::open(&db_path) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            fatal("startup", &format!("cannot open SQLite {db_path}: {e}"));
            std::process::exit(1);
        }
    };

    // 2. Listener. Same bootstrap-gap rule.
    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) => {
            fatal("startup", &format!("cannot bind {bind_addr}: {e}"));
            std::process::exit(1);
        }
    };

    let status: SharedStatus = Arc::new(Mutex::new(Status {
        modem: "initializing".to_string(),
        sim: "unknown".to_string(),
        started_at: now_iso(),
        start: Instant::now(),
    }));

    // 3. Modem bring-up — non-fatal and **off the accept path**. It must not run
    //    synchronously here: bind() already makes the kernel queue inbound
    //    connections, so any time spent talking to the modem before the accept
    //    loop starts leaves clients connected but unanswered (an unresponsive
    //    modem can take ~50s of AT timeouts). Running it on its own thread keeps
    //    `health` answerable from the first moment.
    let exec: SharedExec = Arc::new(Mutex::new(None));
    spawn_bring_up(store.clone(), status.clone(), exec.clone());

    // Occupancy gate: only one client may hold a connection at a time.
    let busy = Arc::new(AtomicBool::new(false));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (store, status, exec, busy) =
            (store.clone(), status.clone(), exec.clone(), busy.clone());
        thread::spawn(move || handle_connection(stream, store, status, exec, busy));
    }
}

/// Open the port, unlock the SIM and put the modem into a known SMS state.
fn bring_up_modem() -> Result<ModemExecutor, String> {
    let port_name =
        std::env::var("MODEM_AT_PORT").map_err(|_| "MODEM_AT_PORT is not set".to_string())?;
    let mut port = serial::open_port(&port_name)?;
    serial::health_check(&mut port)?;
    serial::sms_init(&mut port)?;
    Ok(ModemExecutor::new(port))
}

/// Run bring-up on its own thread and publish the outcome. Until it finishes,
/// `health` reports `modem: initializing` and modem ops return `modem_not_ready`.
fn spawn_bring_up(store: SharedStore, status: SharedStatus, exec: SharedExec) {
    thread::spawn(move || match bring_up_modem() {
        Ok(executor) => {
            if let Ok(mut st) = status.lock() {
                st.modem = "ready".to_string();
                st.sim = "READY".to_string();
            }
            if let Ok(mut guard) = exec.lock() {
                *guard = Some(executor);
            }
        }
        Err(e) => {
            if let Ok(mut st) = status.lock() {
                st.modem = "not_ready".to_string();
            }
            if let Ok(s) = store.lock() {
                let _ = s.record_error(&now_iso(), "error", "bringup", &e, None);
            }
        }
    });
}

fn handle_connection(
    stream: TcpStream,
    store: SharedStore,
    status: SharedStatus,
    exec: SharedExec,
    busy: Arc<AtomicBool>,
) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    let mut ws = match tungstenite::accept(stream) {
        Ok(w) => w,
        Err(_) => return, // not a WebSocket client / handshake failed
    };

    // Reject a second connection at the door.
    if busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let resp = api::error_json(None, &ApiError::already_connected());
        let _ = ws.send(Message::text(resp.to_string()));
        let _ = ws.close(None);
        return;
    }

    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let resp = process_message(t.as_str(), &peer, &store, &status, &exec);
                if ws.send(Message::text(resp.to_string())).is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {} // ping/pong/binary: ignore (tungstenite auto-answers pings)
        }
    }

    busy.store(false, Ordering::Release);
}

/// Parse, execute, persist, and produce the JSON response for one request.
fn process_message(
    text: &str,
    peer: &str,
    store: &SharedStore,
    status: &SharedStatus,
    exec: &SharedExec,
) -> Value {
    let started = Instant::now();
    let ts_received = now_iso();

    let (id, body) = match api::parse_request(text) {
        Ok(v) => v,
        Err((id, e)) => {
            record_parse_failure(store, &ts_received, peer, id, text, &e, started);
            return api::error_json(id, &e);
        }
    };

    let op_name = body.op_name();
    let rowid = store
        .lock()
        .unwrap()
        .insert_request(&ts_received, peer, id, op_name, text)
        .ok();

    let result: Result<Value, ApiError> = match body {
        RequestBody::Health => Ok(health_value(status, store)),
        RequestBody::ListSms => {
            run_modem(exec, |ex| ex.list_sms().map(|m| json!({ "messages": m })))
        }
        RequestBody::ReadSms { index } => {
            run_modem(exec, |ex| ex.read_sms(index).map(|m| json!({ "message": m })))
        }
        RequestBody::DeleteSms { index } => {
            run_modem(exec, |ex| ex.delete_sms(index).map(|()| json!({})))
        }
    };

    let (resp, ok, error_code) = match result {
        Ok(payload) => (api::ok_json(id, payload), true, None),
        Err(e) => {
            if let Ok(s) = store.lock() {
                let _ = s.record_error(&now_iso(), "error", op_name, &e.message, rowid);
            }
            let code = e.code.clone();
            (api::error_json(id, &e), false, Some(code))
        }
    };

    if let Some(rid) = rowid {
        let duration = started.elapsed().as_millis() as i64;
        let _ = store.lock().unwrap().finish_request(
            rid,
            &now_iso(),
            ok,
            &resp.to_string(),
            error_code.as_deref(),
            duration,
        );
    }

    resp
}

/// Lock the executor and run a modem op, or fail with `modem_not_ready`.
fn run_modem<F>(exec: &SharedExec, f: F) -> Result<Value, ApiError>
where
    F: FnOnce(&mut ModemExecutor) -> Result<Value, ApiError>,
{
    let mut guard = exec.lock().unwrap();
    match guard.as_mut() {
        Some(ex) => f(ex),
        None => Err(ApiError::modem_not_ready()),
    }
}

fn health_value(status: &SharedStatus, store: &SharedStore) -> Value {
    let st = status.lock().unwrap();
    let recent_errors = store.lock().unwrap().error_count();
    json!({
        "status": {
            "modem": st.modem,
            "sim": st.sim,
            "started_at": st.started_at,
            "uptime_s": st.start.elapsed().as_secs(),
            "recent_errors": recent_errors,
        }
    })
}

/// Record a request that failed to parse (before dispatch) as one audited row.
fn record_parse_failure(
    store: &SharedStore,
    ts_received: &str,
    peer: &str,
    id: Option<i64>,
    params_json: &str,
    e: &ApiError,
    started: Instant,
) {
    let Ok(s) = store.lock() else { return };
    if let Ok(rid) = s.insert_request(ts_received, peer, id, "invalid", params_json) {
        let duration = started.elapsed().as_millis() as i64;
        let resp = api::error_json(id, e).to_string();
        let _ = s.finish_request(rid, &now_iso(), false, &resp, Some(&e.code), duration);
        let _ = s.record_error(&now_iso(), "warn", "invalid", &e.message, Some(rid));
    }
}
