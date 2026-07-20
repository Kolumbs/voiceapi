//! Serial transport to the modem: opening the AT port, the synchronous
//! AT-command helper, and the bootstrap bring-up (SIM PIN, SMS mode).
//!
//! Nothing here logs to stdout. Failures are returned as `Err(String)` so the
//! caller can record them in the database. The only console output in the whole
//! program is [`fatal`], reserved for startup failures that have nowhere else to
//! be recorded (the DB or listener could not be created).

use serialport::SerialPort;
use std::io::Read;
use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub const BAUD_RATE: u32 = 115200;
/// Timeout for a single synchronous AT command round-trip.
pub const CMD_TIMEOUT_MS: u64 = 10_000;
/// Shorter timeout for the `at` connectivity probe: a live modem answers in
/// milliseconds, and probing several candidate ports should not take a minute.
pub const AT_PROBE_TIMEOUT_MS: u64 = 3_000;
/// How long to wait after sending a PIN before re-checking CPIN state.
const PIN_SETTLE_MS: u64 = 3_000;

/// A concrete, owned serial handle.
pub type Port = Box<dyn SerialPort>;

/// Emit a fatal-level line to stderr. Used *only* for startup failures that
/// cannot be recorded in SQLite (the DB or listener itself could not be opened).
pub fn fatal(context: &str, message: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z");
    eprintln!("[{ts}][FATAL][{context}] {message}");
}

/// Wall-clock budget for opening a port. `serialport`'s open runs termios ioctls
/// that ignore the read/write timeout and can block uninterruptibly on a wedged
/// USB endpoint, so the open itself must be bounded.
pub const OPEN_TIMEOUT_MS: u64 = 5_000;

/// Open the modem's AT serial port by explicit path (no enumeration).
fn open_port_raw(port_name: &str) -> Result<Port, String> {
    serialport::new(port_name, BAUD_RATE)
        .timeout(Duration::from_millis(200))
        .open()
        .map_err(|e| format!("failed to open {port_name}: {e}"))
}

/// Open a port with a hard wall-clock timeout. If the open does not complete in
/// [`OPEN_TIMEOUT_MS`] the worker thread is abandoned (it may be stuck in an
/// uninterruptible syscall) and an error is returned, so one bad device cannot
/// hang the caller.
pub fn open_port(port_name: &str) -> Result<Port, String> {
    let name = port_name.to_string();
    match run_with_timeout(Duration::from_millis(OPEN_TIMEOUT_MS), move || {
        open_port_raw(&name)
    }) {
        Ok(res) => res,
        Err(()) => Err(format!("timed out opening {port_name} (device not responding)")),
    }
}

/// Run `f` on a worker thread, returning its value, or `Err(())` if it does not
/// finish within `timeout`. On timeout the worker is left running (and its result
/// dropped when it eventually finishes), which is the price of not blocking here.
pub fn run_with_timeout<T, F>(timeout: Duration, f: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).map_err(|_| ())
}

/// Send `command` (without terminator) and collect the full response up to the
/// first final result code (`OK` / `ERROR` / `+CME ERROR` / `+CMS ERROR`) or
/// until `timeout_ms` elapses. Returns the raw response string.
pub fn send_at_command(port: &mut Port, command: &str, timeout_ms: u64) -> Result<String, String> {
    let cmd = format!("{command}\r\n");
    port.write_all(cmd.as_bytes()).map_err(|e| e.to_string())?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut response = String::new();
    let mut buf = [0u8; 256];

    loop {
        if Instant::now() >= deadline {
            return Err(format!("timeout waiting for response to command: {command}"));
        }

        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                response.push_str(&String::from_utf8_lossy(&buf[..n]));
                let trimmed = response.trim_end();
                if trimmed.ends_with("OK")
                    || trimmed.ends_with("ERROR")
                    || trimmed.contains("+CME ERROR")
                    || trimmed.contains("+CMS ERROR")
                {
                    break;
                }
            }
            // read() returned 0 bytes or timed out while waiting — sleep briefly
            // to avoid busy-spinning.
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }

    Ok(response)
}

/// Bootstrap health-check: enable network time-zone sync and ensure the SIM is
/// unlocked and READY. `pin` is only used if the modem reports `+CPIN: SIM PIN`,
/// so a wrong PIN costs at most one of the SIM's three attempts per call.
pub fn health_check(port: &mut Port, pin: Option<&str>) -> Result<(), String> {
    // Enable automatic time-zone update from the network.
    let resp = send_at_command(port, "AT+CTZU=1", CMD_TIMEOUT_MS)?;
    if !resp.contains("OK") {
        return Err("failed to enable automatic time-zone update (AT+CTZU=1)".to_string());
    }

    // Check SIM PIN state.
    let resp = send_at_command(port, "AT+CPIN?", CMD_TIMEOUT_MS)?;

    if resp.contains("+CPIN: SIM PIN") {
        let pin = pin.ok_or_else(|| "SIM PIN required but no PIN is configured".to_string())?;

        let resp = send_at_command(port, &format!("AT+CPIN={pin}"), CMD_TIMEOUT_MS)?;
        if !resp.contains("OK") {
            return Err("SIM PIN unlock rejected — check the configured PIN".to_string());
        }

        // Allow the modem to process the PIN and register to the network.
        thread::sleep(Duration::from_millis(PIN_SETTLE_MS));

        let resp = send_at_command(port, "AT+CPIN?", CMD_TIMEOUT_MS)?;
        if !resp.contains("+CPIN: READY") {
            return Err(format!(
                "SIM did not reach READY after PIN entry: {}",
                resp.trim()
            ));
        }
    } else if !resp.contains("+CPIN: READY") {
        return Err(format!("unexpected SIM state (expected READY): {}", resp.trim()));
    }

    Ok(())
}

/// Put the modem into a known SMS state: text mode, and suppress unsolicited
/// new-message indications so incoming SMS are stored but not pushed as URCs
/// (keeps the serial line quiet except during a command).
pub fn sms_init(port: &mut Port) -> Result<(), String> {
    let resp = send_at_command(port, "AT+CMGF=1", CMD_TIMEOUT_MS)?;
    if !resp.contains("OK") {
        return Err("failed to set SMS text mode (AT+CMGF=1)".to_string());
    }
    // mode=1 (discard/no-buffer of URCs), mt=0 (no delivery indications — SMS
    // stored on the modem/SIM and retrieved via list_sms).
    let resp = send_at_command(port, "AT+CNMI=1,0,0,0,0", CMD_TIMEOUT_MS)?;
    if !resp.contains("OK") {
        return Err("failed to suppress SMS indications (AT+CNMI)".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_timeout_returns_value() {
        assert_eq!(run_with_timeout(Duration::from_secs(2), || 42), Ok(42));
    }

    #[test]
    fn run_with_timeout_times_out_on_a_slow_task() {
        let r = run_with_timeout(Duration::from_millis(100), || {
            thread::sleep(Duration::from_secs(3));
            1
        });
        assert_eq!(r, Err(()));
    }

    #[test]
    fn open_port_bounds_a_nonexistent_device() {
        // A missing device fails fast (not a timeout), proving the happy path is
        // unaffected by the wrapper.
        assert!(open_port("/dev/voiceapi-does-not-exist").is_err());
    }
}
