# voiceapi

A small, reliable service that fully owns a GSM modem and exposes a curated SMS
API over a WebSocket. Clients never send raw AT commands — voiceapi hides all AT
sequencing, parsing, and error mapping behind a closed set of operations.

## Interface

- **Transport:** WebSocket (JSON messages) over TCP. Intended to sit behind the
  reverse proxy's TLS as `wss://` (Apache `mod_proxy_wstunnel`).
- **Single client at a time.** A second connection is rejected at connect with
  `already_connected` and closed — which also serves as a liveness signal.
- **SQLite is the single source of truth** for all requests and errors. Nothing
  is logged to stdout in normal operation; the only stderr output is a fatal
  startup failure (DB or listener could not be opened).

### Operations

Requests carry a client-chosen `id` echoed back on the response.

| request | response |
|---------|----------|
| `{"id":1,"op":"list_sms"}` | `{"id":1,"ok":true,"messages":[{index,status,sender,timestamp,text}]}` |
| `{"id":2,"op":"read_sms","index":5}` | `{"id":2,"ok":true,"message":{...}}` |
| `{"id":3,"op":"delete_sms","index":5}` | `{"id":3,"ok":true}` |
| `{"id":4,"op":"health"}` | `{"id":4,"ok":true,"status":{modem,sim,started_at,uptime_s}}` |
| `{"id":5,"op":"list_errors","limit":20}` | `{"id":5,"ok":true,"errors":[{rowid,ts,severity,context,message,req_rowid}]}` |

Errors: `{"id":N,"ok":false,"error":{"code":"...","message":"..."}}`
(`bad_request`, `not_found`, `modem_error`, `modem_not_ready`, `storage_error`,
`already_connected`).

`health` answers one question — is the modem ready. When something is wrong,
`list_errors` explains it: bring-up failures *and* failed requests (linked back to
the request via `req_rowid`), newest first. `limit` defaults to 20, max 200. It is
a plain database read, so it still answers while the modem is `not_ready`.

SMS index is the modem's native storage slot; `read_sms` marks a message read;
bodies are handled in text mode (`AT+CMGF=1`).

## Configuration (environment)

| var | purpose | default |
|-----|---------|---------|
| `MODEM_AT_PORT` | modem AT serial port | — (required for modem ops) |
| `MODEM_PIN1` | SIM PIN, if the SIM is locked | — |
| `VOICEAPI_TCP_ADDR` | WebSocket listener bind address | `127.0.0.1:9500` |
| `VOICEAPI_DB` | SQLite database path | `./voiceapi.db` |

Modem bring-up runs on a background thread, so the listener answers `health`
immediately at startup. It is also non-fatal: `health` reports `modem` as
`initializing` while it runs, then `ready`, or `not_ready` if it failed (the
reason is recorded in the `errors` table).

## Build & run

```sh
cargo build --release
MODEM_AT_PORT=/dev/ttyUSB2 VOICEAPI_DB=/var/lib/voiceapi/voiceapi.db \
  ./target/release/voiceapi
```

`cargo test` covers the SMS response parsing (`+CMGL` / `+CMGR` / SCTS).
