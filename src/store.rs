//! SQLite persistence — the single source of truth for all requests and errors.
//! Uses the bundled SQLite (no system libsqlite3 dependency).

use rusqlite::{params, Connection};
use serde::Serialize;
use std::os::unix::fs::PermissionsExt;

/// One row of the error log, shaped exactly like the `errors` table so the API
/// view and the database read identically.
#[derive(Serialize)]
pub struct ErrorRow {
    pub rowid: i64,
    pub ts: String,
    pub severity: String,
    pub context: Option<String>,
    pub message: String,
    pub req_rowid: Option<i64>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
  rowid         INTEGER PRIMARY KEY,
  ts_received   TEXT NOT NULL,
  ts_responded  TEXT,
  client_addr   TEXT,
  req_id        INTEGER,
  op            TEXT NOT NULL,
  params_json   TEXT,
  ok            INTEGER,
  response_json TEXT,
  error_code    TEXT,
  duration_ms   INTEGER
);
CREATE TABLE IF NOT EXISTS errors (
  rowid     INTEGER PRIMARY KEY,
  ts        TEXT NOT NULL,
  severity  TEXT NOT NULL,
  context   TEXT,
  message   TEXT NOT NULL,
  req_rowid INTEGER REFERENCES requests(rowid)
);
CREATE TABLE IF NOT EXISTS config (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  at_port TEXT,
  pin     TEXT
);
";

/// Modem configuration, persisted so the service comes up unattended.
#[derive(Default)]
pub struct ModemConfig {
    pub at_port: Option<String>,
    pub pin: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database and apply migrations. The file is
    /// restricted to 0600 because it stores the SIM PIN.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot restrict permissions on {path}: {e}"))?;
        Ok(Self { conn })
    }

    /// Current modem configuration (empty if never set).
    pub fn get_config(&self) -> Result<ModemConfig, String> {
        self.conn
            .query_row(
                "SELECT at_port, pin FROM config WHERE id = 1",
                [],
                |r| Ok(ModemConfig { at_port: r.get(0)?, pin: r.get(1)? }),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(ModemConfig::default()),
                other => Err(other.to_string()),
            })
    }

    /// Update configuration. `None` leaves a field unchanged; `Some("")` clears it.
    pub fn set_config(&self, at_port: Option<&str>, pin: Option<&str>) -> Result<(), String> {
        // Outer Option = "was this field supplied"; inner = the value, where an
        // empty string means "clear it".
        fn norm(v: Option<&str>) -> Option<Option<&str>> {
            v.map(|s| if s.is_empty() { None } else { Some(s) })
        }
        let (at_port, pin) = (norm(at_port), norm(pin));
        self.conn
            .execute(
                "INSERT INTO config (id, at_port, pin) VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                   at_port = CASE WHEN ?3 THEN ?1 ELSE at_port END,
                   pin     = CASE WHEN ?4 THEN ?2 ELSE pin     END",
                params![
                    at_port.flatten(),
                    pin.flatten(),
                    at_port.is_some(),
                    pin.is_some()
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record an incoming request; returns its rowid for later completion.
    pub fn insert_request(
        &self,
        ts_received: &str,
        client_addr: &str,
        req_id: Option<i64>,
        op: &str,
        params_json: &str,
    ) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO requests (ts_received, client_addr, req_id, op, params_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts_received, client_addr, req_id, op, params_json],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Complete a previously inserted request with its outcome.
    pub fn finish_request(
        &self,
        rowid: i64,
        ts_responded: &str,
        ok: bool,
        response_json: &str,
        error_code: Option<&str>,
        duration_ms: i64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE requests
                 SET ts_responded = ?2, ok = ?3, response_json = ?4, error_code = ?5, duration_ms = ?6
                 WHERE rowid = ?1",
                params![rowid, ts_responded, ok as i64, response_json, error_code, duration_ms],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record an error not necessarily tied to a request (e.g. bring-up).
    pub fn record_error(
        &self,
        ts: &str,
        severity: &str,
        context: &str,
        message: &str,
        req_rowid: Option<i64>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO errors (ts, severity, context, message, req_rowid)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, severity, context, message, req_rowid],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Most recent errors, newest first.
    pub fn recent_errors(&self, limit: u32) -> Result<Vec<ErrorRow>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rowid, ts, severity, context, message, req_rowid
                 FROM errors ORDER BY rowid DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok(ErrorRow {
                    rowid: r.get(0)?,
                    ts: r.get(1)?,
                    severity: r.get(2)?,
                    context: r.get(3)?,
                    message: r.get(4)?,
                    req_rowid: r.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
}
