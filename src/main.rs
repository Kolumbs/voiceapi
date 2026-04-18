use serialport;
use std::io::{self, BufRead, Read, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let port_name = "/dev/ttyUSB5";
    let baud_rate = 115200;

    // 1. Open the port
    let port = serialport::new(port_name, baud_rate)
        .timeout(Duration::from_millis(100))
        .open()
        .expect("Failed to open port. Is the modem connected?");

    // We "clone" the port so one thread can read and one can write
    let mut reader = port.try_clone().expect("Failed to clone port for reading");
    let mut writer = port;

    println!("--- Connected to {} at {} baud ---", port_name, baud_rate);
    println!("--- Type your AT commands below (e.g., AT) ---");

    // THREAD 1: Read from Modem -> Print to Screen
    thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        loop {
            if let Ok(t) = reader.read(&mut buffer) {
                if t > 0 {
                    // Print the modem's response to the screen
                    io::stdout().write_all(&buffer[..t]).unwrap();
                    io::stdout().flush().unwrap();
                }
            }
        }
    });

    // THREAD 2: Read from Stdin -> Send to Modem
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let l = line.expect("Failed to read line");

        match l.trim() {
            "!DTR_LOW" => {
                // Asserting DTR pulls the physical line LOW -> WAKES Modem
                writer
                    .write_data_terminal_ready(true)
                    .expect("Failed to set DTR LOW");
                println!(">>> DTR Line Asserted (Physical LOW / Wake Signal)");
            }
            "!DTR_HIGH" => {
                // De-asserting DTR lets the physical line go HIGH -> ALLOWS Sleep
                writer
                    .write_data_terminal_ready(false)
                    .expect("Failed to set DTR HIGH");
                println!(">>> DTR Line De-asserted (Physical HIGH / Sleep Signal)");
            }
            _ => {
                // Normal AT command logic
                let cmd = format!("{}\r\n", l);
                writer
                    .write_all(cmd.as_bytes())
                    .expect("Failed to write to modem");
            }
        }
    }
}
