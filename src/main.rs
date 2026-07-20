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
use store::{ModemConfig, Store};

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

    // 3. Modem bring-up — non-fatal, and off the accept path so `health` stays
    //    answerable while the modem initializes.
    let exec: SharedExec = Arc::new(Mutex::new(None));
    let bringing_up = Arc::new(AtomicBool::new(false));
    spawn_bring_up(
        store.clone(),
        status.clone(),
        exec.clone(),
        bringing_up.clone(),
    );

    // Occupancy gate: only one client may hold a connection at a time.
    let busy = Arc::new(AtomicBool::new(false));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ctx = Ctx {
            store: store.clone(),
            status: status.clone(),
            exec: exec.clone(),
            bringing_up: bringing_up.clone(),
        };
        let busy = busy.clone();
        thread::spawn(move || handle_connection(stream, ctx, busy));
    }
}

/// Shared state handed to each connection.
#[derive(Clone)]
struct Ctx {
    store: SharedStore,
    status: SharedStatus,
    exec: SharedExec,
    bringing_up: Arc<AtomicBool>,
}

/// Open the port, unlock the SIM and put the modem into a known SMS state.
fn bring_up_modem(cfg: &ModemConfig) -> Result<ModemExecutor, String> {
    let port_name = cfg
        .at_port
        .as_deref()
        .ok_or_else(|| "AT port is not configured".to_string())?;
    let mut port = serial::open_port(port_name)?;
    serial::health_check(&mut port, cfg.pin.as_deref())?;
    serial::sms_init(&mut port)?;
    Ok(ModemExecutor::new(port))
}

/// Run bring-up on its own thread and publish the outcome. Until it finishes,
/// `health` reports `modem: initializing` and modem ops return `modem_not_ready`.
/// Returns false if an attempt is already running.
fn spawn_bring_up(
    store: SharedStore,
    status: SharedStatus,
    exec: SharedExec,
    in_progress: Arc<AtomicBool>,
) -> bool {
    if in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }

    thread::spawn(move || {
        if let Ok(mut st) = status.lock() {
            st.modem = "initializing".to_string();
            st.sim = "unknown".to_string();
        }
        // Drop any existing handle before reopening: serialport takes an exclusive
        // flock, so the old one would block reopening the same device.
        if let Ok(mut guard) = exec.lock() {
            *guard = None;
        }

        let cfg = store.lock().map_or_else(
            |_| Err("config unavailable".to_string()),
            |s| s.get_config(),
        );

        let outcome = cfg.and_then(|cfg| bring_up_modem(&cfg));
        match outcome {
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
        }
        in_progress.store(false, Ordering::Release);
    });
    true
}

fn handle_connection(stream: TcpStream, ctx: Ctx, busy: Arc<AtomicBool>) {
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
                let resp = process_message(t.as_str(), &peer, &ctx);
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
fn process_message(text: &str, peer: &str, ctx: &Ctx) -> Value {
    let (store, status, exec) = (&ctx.store, &ctx.status, &ctx.exec);
    let started = Instant::now();
    let ts_received = now_iso();

    let (id, body) = match api::parse_request(text) {
        Ok(v) => v,
        Err((id, e)) => {
            let safe = api::audit_unparsed(text);
            record_parse_failure(store, &ts_received, peer, id, &safe, &e, started);
            return api::error_json(id, &e);
        }
    };

    let op_name = body.op_name();
    let audit = api::audit_params(&body, text);
    let rowid = store
        .lock()
        .unwrap()
        .insert_request(&ts_received, peer, id, op_name, &audit)
        .ok();

    let result: Result<Value, ApiError> = match body {
        RequestBody::Health => Ok(health_value(status)),
        RequestBody::ListSms => {
            run_modem(exec, |ex| ex.list_sms().map(|m| json!({ "messages": m })))
        }
        RequestBody::ReadSms { index } => {
            run_modem(exec, |ex| ex.read_sms(index).map(|m| json!({ "message": m })))
        }
        RequestBody::DeleteSms { index } => {
            run_modem(exec, |ex| ex.delete_sms(index).map(|()| json!({})))
        }
        // Pure database read: no modem executor, so it still answers while the
        // modem is not_ready — which is exactly when it is needed.
        RequestBody::ListErrors { limit } => store
            .lock()
            .unwrap()
            .recent_errors(limit.min(api::MAX_ERROR_LIMIT))
            .map(|errors| json!({ "errors": errors }))
            .map_err(ApiError::storage),
        RequestBody::GetConfig => config_json(&store.lock().unwrap()),
        // One lock for both the write and the read-back: std::sync::Mutex is not
        // reentrant, so re-locking here would deadlock the connection thread.
        RequestBody::SetConfig { at_port, pin } => {
            let s = store.lock().unwrap();
            s.set_config(at_port.as_deref(), pin.as_deref())
                .map_err(ApiError::storage)
                .and_then(|()| config_json(&s))
        }
        // Asynchronous: bring-up can take ~50s, and this connection is the only
        // one served, so the client polls `health` instead of blocking here.
        RequestBody::Reconnect => {
            if spawn_bring_up(
                store.clone(),
                status.clone(),
                exec.clone(),
                ctx.bringing_up.clone(),
            ) {
                Ok(health_value(status))
            } else {
                Err(ApiError::busy("bring-up already in progress"))
            }
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

/// Configuration as exposed to clients — the PIN is never returned, only whether
/// one is set. Takes an already-locked `Store` so callers control the lock.
fn config_json(s: &Store) -> Result<Value, ApiError> {
    let cfg = s.get_config().map_err(ApiError::storage)?;
    Ok(json!({
        "config": {
            "at_port": cfg.at_port,
            "pin_set": cfg.pin.is_some(),
        }
    }))
}

/// Liveness only: is the modem ready. Diagnostic detail lives in `list_errors`.
fn health_value(status: &SharedStatus) -> Value {
    let st = status.lock().unwrap();
    json!({
        "status": {
            "modem": st.modem,
            "sim": st.sim,
            "started_at": st.started_at,
            "uptime_s": st.start.elapsed().as_secs(),
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
