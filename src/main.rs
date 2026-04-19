use chrono::Local;
use serialport::SerialPort;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BAUD_RATE: u32 = 115200;
/// Timeout for a single synchronous AT command round-trip.
const CMD_TIMEOUT_MS: u64 = 10_000;
/// How long to wait after sending a PIN before re-checking CPIN state.
const PIN_SETTLE_MS: u64 = 3_000;
/// Unix socket file name (created in the working directory).
const SOCKET_PATH: &str = "voiceapi.sock";

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn log(tag: &str, message: &str) {
    let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z");
    println!("[{}][{}] {}", ts, tag, message);
}

// ---------------------------------------------------------------------------
// Synchronous AT-command helper
// ---------------------------------------------------------------------------

/// Send `command` (without terminator) to `port` and collect the full
/// response up to the first `OK` or `ERROR` line, or until `timeout_ms`
/// elapses.  Returns the raw response string.
fn send_at_command(
    port: &mut Box<dyn SerialPort>,
    command: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    let cmd = format!("{}\r\n", command);
    port.write_all(cmd.as_bytes()).map_err(|e| e.to_string())?;
    port.flush().map_err(|e| e.to_string())?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut response = String::new();
    let mut buf = [0u8; 256];

    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "Timeout waiting for response to command: {}",
                command
            ));
        }

        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                response.push_str(&String::from_utf8_lossy(&buf[..n]));
                // A complete AT response always ends with a final result code.
                let trimmed = response.trim_end();
                if trimmed.ends_with("OK")
                    || trimmed.ends_with("ERROR")
                    || trimmed.contains("+CME ERROR")
                    || trimmed.contains("+CMS ERROR")
                {
                    break;
                }
            }
            // read() returns 0 bytes or a timeout error while waiting – sleep
            // briefly to avoid busy-spinning.
            _ => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    Ok(response)
}

// ---------------------------------------------------------------------------
// Bootstrap / health-check
// ---------------------------------------------------------------------------

/// Execute the bootstrap sequence before accepting any user commands:
///
/// 1. Enable automatic time-zone update from the network (`AT+CTZU=1`).
/// 2. Query the SIM PIN state (`AT+CPIN?`).
/// 3. If the SIM requires a PIN, unlock it using the `MODEM_PIN` environment
///    variable and verify that the SIM reaches the READY state.
pub fn health_check(port: &mut Box<dyn SerialPort>) -> Result<(), String> {
    // --- Step 1: Enable network time-zone synchronisation ---
    log("BOOT", "Enabling automatic time-zone update (AT+CTZU=1)");
    let resp = send_at_command(port, "AT+CTZU=1", CMD_TIMEOUT_MS)?;
    log("MODEM", resp.trim());
    if !resp.contains("OK") {
        return Err("Failed to enable automatic time-zone update (AT+CTZU=1)".to_string());
    }

    // --- Step 2: Check SIM PIN state ---
    log("BOOT", "Checking SIM PIN state (AT+CPIN?)");
    let resp = send_at_command(port, "AT+CPIN?", CMD_TIMEOUT_MS)?;
    log("MODEM", resp.trim());

    if resp.contains("+CPIN: SIM PIN") {
        // --- Step 3: Unlock SIM with PIN from environment ---
        let pin = std::env::var("MODEM_PIN1").map_err(|_| {
            "SIM PIN required but MODEM_PIN1 environment variable is not set".to_string()
        })?;

        // Log the command without exposing the actual PIN value.
        log("BOOT", "SIM PIN required – sending unlock command (AT+CPIN=****)");
        let unlock_cmd = format!("AT+CPIN={}", pin);
        let resp = send_at_command(port, &unlock_cmd, CMD_TIMEOUT_MS)?;
        log("MODEM", resp.trim());

        if !resp.contains("OK") {
            return Err(
                "SIM PIN unlock command rejected – check the MODEM_PIN1 value".to_string()
            );
        }

        // Allow the modem to process the PIN and register to the network.
        log("BOOT", "PIN accepted – waiting for SIM to become ready …");
        thread::sleep(Duration::from_millis(PIN_SETTLE_MS));

        // Confirm the SIM is now in the READY state.
        let resp = send_at_command(port, "AT+CPIN?", CMD_TIMEOUT_MS)?;
        log("MODEM", resp.trim());
        if !resp.contains("+CPIN: READY") {
            return Err(format!(
                "SIM did not reach READY state after PIN entry. Response: {}",
                resp.trim()
            ));
        }
    } else if !resp.contains("+CPIN: READY") {
        return Err(format!(
            "Unexpected SIM state – expected READY. Response: {}",
            resp.trim()
        ));
    }

    log("BOOT", "Health check passed – modem and SIM are ready.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Client connection handler
// ---------------------------------------------------------------------------

/// Handle a single Unix socket client: read lines and dispatch commands to
/// the modem via the shared `writer`.
fn handle_client(
    stream: std::os::unix::net::UnixStream,
    writer: Arc<Mutex<Box<dyn SerialPort>>>,
) {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let input = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let cmd = input.trim().to_owned();

        if cmd.is_empty() {
            continue;
        }

        let mut w = writer.lock().expect("writer lock poisoned");
        match cmd.as_str() {
            "!DTR_LOW" => {
                w.write_data_terminal_ready(true)
                    .expect("Failed to assert DTR");
                log("DTR", "Asserted (physical LOW / wake signal)");
            }
            "!DTR_HIGH" => {
                w.write_data_terminal_ready(false)
                    .expect("Failed to de-assert DTR");
                log("DTR", "De-asserted (physical HIGH / sleep signal)");
            }
            _ => {
                log("CMD", &cmd);
                let at_cmd = format!("{}\r\n", cmd);
                w.write_all(at_cmd.as_bytes())
                    .expect("Failed to write to modem");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // Resolve the serial port path from the environment.
    let port_name = std::env::var("MODEM_AT_PORT").unwrap_or_else(|_| {
        log("ERROR", "MODEM_AT_PORT environment variable is not set.");
        std::process::exit(1);
    });

    // Open the serial port.
    let mut port = serialport::new(&port_name, BAUD_RATE)
        .timeout(Duration::from_millis(200))
        .open()
        .unwrap_or_else(|e| {
            log(
                "ERROR",
                &format!("Failed to open {}: {}. Is the modem connected?", port_name, e),
            );
            std::process::exit(1);
        });

    log("INFO", &format!("Connected to {} at {} baud", port_name, BAUD_RATE));

    // Run the bootstrap health-check before accepting any commands.
    if let Err(e) = health_check(&mut port) {
        log("ERROR", &format!("Health check failed: {}", e));
        std::process::exit(1);
    }

    // Split into a reader half (background thread) and a shared writer half
    // (one per connected client, protected by a mutex).
    let reader_port = port.try_clone().expect("Failed to clone port for reading");
    let writer: Arc<Mutex<Box<dyn SerialPort>>> = Arc::new(Mutex::new(port));

    // Background thread: modem → STDOUT
    thread::spawn(move || {
        let mut reader = reader_port;
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            log("MODEM", trimmed);
                        }
                    }
                }
                _ => {}
            }
        }
    });

    // Remove any stale socket file from a previous run.
    if std::path::Path::new(SOCKET_PATH).exists() {
        std::fs::remove_file(SOCKET_PATH)
            .unwrap_or_else(|e| log("WARN", &format!("Could not remove stale socket: {}", e)));
    }

    // Register Ctrl+C handler: clean up the socket file and exit.
    ctrlc::set_handler(|| {
        let _ = std::fs::remove_file(SOCKET_PATH);
        std::process::exit(0);
    })
    .expect("Failed to set Ctrl+C handler");

    // Bind the Unix domain socket.
    let listener = UnixListener::bind(SOCKET_PATH).unwrap_or_else(|e| {
        log("ERROR", &format!("Failed to bind {}: {}", SOCKET_PATH, e));
        std::process::exit(1);
    });

    log(
        "INFO",
        &format!("Listening on {} – send AT commands or !DTR_LOW / !DTR_HIGH", SOCKET_PATH),
    );

    // Accept client connections indefinitely.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let writer_clone = Arc::clone(&writer);
                thread::spawn(move || handle_client(stream, writer_clone));
            }
            Err(e) => {
                log("ERROR", &format!("Accept error on socket: {}", e));
            }
        }
    }
}

