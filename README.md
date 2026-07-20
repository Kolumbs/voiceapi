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
| `{"id":6,"op":"get_config"}` | `{"id":6,"ok":true,"config":{"at_port":"/dev/ttyUSB2","pin_set":true}}` |
| `{"id":7,"op":"set_config","at_port":"...","pin":"1234"}` | same shape as `get_config` |
| `{"id":8,"op":"reconnect"}` | `{"id":8,"ok":true,"status":{...}}` |
| `{"id":9,"op":"check","at_port":"/dev/ttyUSB2"}` | `{"id":9,"ok":true,"check":{"port":"...","reply":"OK"}}` |

Errors: `{"id":N,"ok":false,"error":{"code":"...","message":"..."}}`
(`bad_request`, `not_found`, `modem_error`, `modem_not_ready`, `storage_error`,
`busy`, `already_connected`).

`health` answers one question — is the modem ready. When something is wrong,
`list_errors` explains it: bring-up failures *and* failed requests (linked back to
the request via `req_rowid`), newest first. `limit` defaults to 20, max 200. It is
a plain database read, so it still answers while the modem is `not_ready`.

SMS index is the modem's native storage slot; `read_sms` marks a message read;
bodies are handled in text mode (`AT+CMGF=1`).

## Configuration (environment)

Only bootstrap settings are environment variables — they must exist before the
database and listener do. The modem's AT port and SIM PIN are configured through
the API and persisted in the database.

| var | purpose | default |
|-----|---------|---------|
| `VOICEAPI_TCP_ADDR` | WebSocket listener bind address | `127.0.0.1:9500` |
| `VOICEAPI_DB` | SQLite database path | `./voiceapi.db` |

### First run

The service starts with no modem configuration and reports `modem: not_ready`
(`list_errors` shows `AT port is not configured`). Configure it once:

```json
{"id":1,"op":"set_config","at_port":"/dev/serial/by-id/...","pin":"1234"}
{"id":2,"op":"reconnect"}
```

Config persists across restarts. `set_config` only changes the fields you supply;
`""` clears one. `reconnect` re-runs bring-up **asynchronously** (it can take ~50s
against an unresponsive modem) — poll `health` for the outcome, and a second
concurrent attempt returns `busy`.

### Finding the AT port

A module usually exposes several `ttyUSB` interfaces and only one answers AT
commands — and the numbering moves between reboots. `check` sends a fixed `AT`
(it is a reachability test, not AT passthrough) and reports the reply. Given
`at_port` it probes that device **without** changing the configuration, and it
works while the modem is `not_ready`, so candidates can be tried before
committing one. It fails in ~3s rather than the usual 10s timeout:

```sh
for p in /dev/ttyUSB*; do
  python3 -m voiceapi check -d "{\"at_port\": \"$p\"}" >/dev/null 2>&1 \
    && echo "$p answers"
done
```

Then `set_config` the winner and `reconnect`. With no `at_port`, `check` probes
the configured port — reusing the live handle when the modem is already up, since
the device cannot be opened twice.

The PIN is never returned by the API (`get_config` reports only `pin_set`) and is
redacted from the request audit log. Since the database stores it, the file is
created with mode `0600`.

Modem bring-up runs on a background thread, so the listener answers `health`
immediately at startup. It is also non-fatal: `health` reports `modem` as
`initializing` while it runs, then `ready`, or `not_ready` if it failed (the
reason is recorded in the `errors` table).

## Build & run

```sh
cargo build --release
VOICEAPI_DB=/var/lib/voiceapi/voiceapi.db \
  ./target/release/voiceapi
```

`cargo test` covers the SMS response parsing (`+CMGL` / `+CMGR` / SCTS).

## Client

A dependency-free client ships with the repo — standard library only, so it runs
on the Pi with nothing to install. Run it from the repo root:

```sh
python3 -m voiceapi health
python3 -m voiceapi read_sms   -d '{"index": 3}'
python3 -m voiceapi set_config -d '{"at_port": "/dev/ttyUSB5", "pin": "1234"}'
python3 -m voiceapi reconnect
python3 -m voiceapi list_errors -d '{"limit": 5}'
```

`-d` takes a JSON object merged into the request. The endpoint defaults to
`ws://127.0.0.1:9500/` and is overridable with `--url` (or `VOICEAPI_URL`);
`wss://` is supported for going through the TLS reverse proxy. The operation name
is passed straight through, so the client needs no update when the server gains an
op. Exit status is 0 when the response is `ok`, 1 otherwise — so it scripts:

```sh
python3 -m voiceapi health >/dev/null || echo "voiceapi is unhappy"
```
