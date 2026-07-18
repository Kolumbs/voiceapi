//! SQLite persistence — the single source of truth for all requests and errors.
//! Uses the bundled SQLite (no system libsqlite3 dependency).

use rusqlite::{params, Connection};

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
";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database and apply migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        Ok(Self { conn })
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

    /// Total number of recorded errors (surfaced in `health`).
    pub fn error_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM errors", [], |r| r.get(0))
            .unwrap_or(0)
    }
}
