use chrono::Local;
use serialport::SerialPort;
use std::io::{self, BufRead, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

const PORT_NAME: &str = "/dev/ttyUSB5";
const BAUD_RATE: u32 = 115200;
/// Timeout for a single synchronous AT command round-trip.
const CMD_TIMEOUT_MS: u64 = 10_000;
/// How long to wait after sending a PIN before re-checking CPIN state.
const PIN_SETTLE_MS: u64 = 3_000;

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
        let pin = std::env::var("MODEM_PIN").map_err(|_| {
            "SIM PIN required but MODEM_PIN environment variable is not set".to_string()
        })?;

        // Log the command without exposing the actual PIN value.
        log("BOOT", "SIM PIN required – sending unlock command (AT+CPIN=****)");
        let unlock_cmd = format!("AT+CPIN={}", pin);
        let resp = send_at_command(port, &unlock_cmd, CMD_TIMEOUT_MS)?;
        log("MODEM", resp.trim());

        if !resp.contains("OK") {
            return Err(
                "SIM PIN unlock command rejected – check the MODEM_PIN value".to_string()
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
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // Open the serial port.
    let mut port = serialport::new(PORT_NAME, BAUD_RATE)
        .timeout(Duration::from_millis(200))
        .open()
        .unwrap_or_else(|e| {
            log(
                "ERROR",
                &format!("Failed to open {}: {}. Is the modem connected?", PORT_NAME, e),
            );
            std::process::exit(1);
        });

    log("INFO", &format!("Connected to {} at {} baud", PORT_NAME, BAUD_RATE));

    // Run the bootstrap health-check before accepting any commands.
    if let Err(e) = health_check(&mut port) {
        log("ERROR", &format!("Health check failed: {}", e));
        std::process::exit(1);
    }

    log("INFO", "Ready – type AT commands below (or !DTR_LOW / !DTR_HIGH).");

    // Split the port into a reader half (background thread) and a writer half
    // (main thread, driven by STDIN).
    let mut reader = port.try_clone().expect("Failed to clone port for reading");
    let mut writer = port;

    // Background thread: modem → STDOUT
    thread::spawn(move || {
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

    // Main thread: STDIN → modem
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let input = line.expect("Failed to read from STDIN");
        let cmd = input.trim();

        if cmd.is_empty() {
            continue;
        }

        match cmd {
            "!DTR_LOW" => {
                writer
                    .write_data_terminal_ready(true)
                    .expect("Failed to assert DTR");
                log("DTR", "Asserted (physical LOW / wake signal)");
            }
            "!DTR_HIGH" => {
                writer
                    .write_data_terminal_ready(false)
                    .expect("Failed to de-assert DTR");
                log("DTR", "De-asserted (physical HIGH / sleep signal)");
            }
            _ => {
                log("CMD", cmd);
                let at_cmd = format!("{}\r\n", cmd);
                writer
                    .write_all(at_cmd.as_bytes())
                    .expect("Failed to write to modem");
            }
        }
    }
}
