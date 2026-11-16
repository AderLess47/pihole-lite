//! Lastgenerator für das Messprotokoll (siehe `docs/messungen.md`).
//!
//! Bewusst ohne Abhängigkeiten und außerhalb des Crate-Baums, damit weder
//! `Cargo.toml` noch der Speicher-Fußabdruck des Servers davon berührt werden.
//! Bauen mit: `rustc -O --edition 2021 bench/loadgen.rs -o loadgen`
//!
//! Aufruf:
//!   loadgen --server 127.0.0.1:5353 --seconds 10 --threads 8 --mode blocked
//!
//! Modi: `blocked` fragt Namen aus der 100k-Liste ab (reiner Filter-Pfad, keine
//! Upstream-Latenz), `free` fragt ungeblockte Namen ab (Cache/Upstream-Pfad).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NAME_POOL: usize = 4096;
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

fn main() {
    let mut server: SocketAddr = "127.0.0.1:5353".parse().expect("adresse");
    let mut seconds = 10u64;
    let mut threads = 8usize;
    let mut mode = "blocked".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--server" => server = next(&mut args, &flag).parse().expect("adresse"),
            "--seconds" => seconds = next(&mut args, &flag).parse().expect("sekunden"),
            "--threads" => threads = next(&mut args, &flag).parse().expect("threads"),
            "--mode" => mode = next(&mut args, &flag),
            "--help" | "-h" => {
                println!(
                    "usage: loadgen [--server ADDR] [--seconds N] [--threads N] [--mode blocked|free]"
                );
                return;
            }
            other => {
                eprintln!("unbekanntes argument: {other}");
                std::process::exit(2);
            }
        }
    }
    if threads == 0 {
        eprintln!("--threads muss > 0 sein");
        std::process::exit(2);
    }
    if mode != "blocked" && mode != "free" {
        eprintln!("--mode muss `blocked` oder `free` sein");
        std::process::exit(2);
    }

    // Namen einmal vorbereiten: kein `format!` pro Query, sonst misst man den
    // Allokator des Lastgenerators statt den Server.
    let names: Vec<String> = (0..NAME_POOL)
        .map(|index| match mode.as_str() {
            "free" => format!("free-{index}.example.org"),
            _ => format!("blocked-{index}.example.com"),
        })
        .collect();
    let names = Arc::new(names);

    let answered = Arc::new(AtomicU64::new(0));
    let timeouts = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let latency_sum_us = Arc::new(AtomicU64::new(0));
    let latency_max_us = Arc::new(AtomicU64::new(0));

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut handles = Vec::with_capacity(threads);

    for thread_id in 0..threads {
        let names = Arc::clone(&names);
        let answered = Arc::clone(&answered);
        let timeouts = Arc::clone(&timeouts);
        let errors = Arc::clone(&errors);
        let latency_sum_us = Arc::clone(&latency_sum_us);
        let latency_max_us = Arc::clone(&latency_max_us);

        handles.push(std::thread::spawn(move || {
            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(socket) => socket,
                Err(err) => {
                    eprintln!("socket: {err}");
                    return;
                }
            };
            if let Err(err) = socket.connect(server) {
                eprintln!("connect {server}: {err}");
                return;
            }
            let _ = socket.set_read_timeout(Some(QUERY_TIMEOUT));

            // xorshift: reproduzierbar pro Thread, ohne rand-Abhängigkeit.
            let mut state = seed(thread_id);
            let mut buffer = [0u8; 512];
            let mut reply = [0u8; 512];
            let mut local_answered = 0u64;
            let mut local_timeouts = 0u64;
            let mut local_errors = 0u64;
            let mut local_sum = 0u64;
            let mut local_max = 0u64;

            while Instant::now() < deadline {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let index = (state as usize) % names.len();
                let id = (state >> 16) as u16;
                let len = build_query(id, &names[index], &mut buffer);

                let started = Instant::now();
                if socket.send(&buffer[..len]).is_err() {
                    local_errors += 1;
                    continue;
                }
                match socket.recv(&mut reply) {
                    Ok(received) => {
                        let micros = started.elapsed().as_micros() as u64;
                        let id_ok = received >= 12 && reply[0] == (id >> 8) as u8
                            && reply[1] == id as u8;
                        let rcode = reply[3] & 0x0f;
                        // NOERROR und NXDOMAIN sind gültige Antworten; SERVFAIL und
                        // REFUSED zählen als Fehler, weil sie keinen Durchsatz belegen.
                        if id_ok && (rcode == 0 || rcode == 3) {
                            local_answered += 1;
                            local_sum += micros;
                            if micros > local_max {
                                local_max = micros;
                            }
                        } else {
                            local_errors += 1;
                        }
                    }
                    Err(_) => local_timeouts += 1,
                }
            }

            answered.fetch_add(local_answered, Ordering::Relaxed);
            timeouts.fetch_add(local_timeouts, Ordering::Relaxed);
            errors.fetch_add(local_errors, Ordering::Relaxed);
            latency_sum_us.fetch_add(local_sum, Ordering::Relaxed);
            let mut current = latency_max_us.load(Ordering::Relaxed);
            while local_max > current {
                match latency_max_us.compare_exchange_weak(
                    current,
                    local_max,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        }));
    }

    for handle in handles {
        let _ = handle.join();
    }

    let answered = answered.load(Ordering::Relaxed);
    let timeouts = timeouts.load(Ordering::Relaxed);
    let errors = errors.load(Ordering::Relaxed);
    let sum = latency_sum_us.load(Ordering::Relaxed);
    let max_ms = latency_max_us.load(Ordering::Relaxed) as f64 / 1000.0;
    let avg_ms = if answered > 0 {
        sum as f64 / answered as f64 / 1000.0
    } else {
        0.0
    };
    let qps = answered as f64 / seconds as f64;

    println!("mode={mode}");
    println!("threads={threads}");
    println!("seconds={seconds}");
    println!("answered={answered}");
    println!("timeouts={timeouts}");
    println!("errors={errors}");
    println!("qps={:.1}", qps);
    println!("avg_ms={:.2}", avg_ms);
    println!("max_ms={:.2}", max_ms);
}

fn next(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| panic!("`{flag}` erwartet einen wert"))
}

fn seed(thread_id: usize) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    let base = 0x9E37_79B9_7F4A_7C15u64 ^ (thread_id as u64 + 1);
    base ^ nanos.max(1)
}

/// Baut eine DNS-Query mit einer Frage (A, IN) in `buffer` und liefert die Länge.
fn build_query(id: u16, name: &str, buffer: &mut [u8]) -> usize {
    buffer[0] = (id >> 8) as u8;
    buffer[1] = id as u8;
    buffer[2] = 0x01; // RD
    buffer[3] = 0x00;
    buffer[4] = 0x00;
    buffer[5] = 0x01; // QDCOUNT
    buffer[6..12].fill(0);
    let mut offset = 12;
    for label in name.split('.') {
        let bytes = label.as_bytes();
        buffer[offset] = bytes.len() as u8;
        offset += 1;
        buffer[offset..offset + bytes.len()].copy_from_slice(bytes);
        offset += bytes.len();
    }
    buffer[offset] = 0;
    offset += 1;
    buffer[offset] = 0x00;
    buffer[offset + 1] = 0x01; // QTYPE A
    buffer[offset + 2] = 0x00;
    buffer[offset + 3] = 0x01; // QCLASS IN
    offset + 4
}
