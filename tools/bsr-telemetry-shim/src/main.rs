use anyhow::Context;
use named_pipe::PipeOptions;
use serde_json::Value;
use std::env;
use std::io::{Read, Write};
use std::time::Duration;
use chrono::{DateTime, Utc};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let pipe_name = args.get(1).cloned().unwrap_or_else(|| "bsr-telemetry".to_string());
    let target_frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);

    let full_name = format!(r"\\.\\pipe\\{}", pipe_name);
    println!("bsr-telemetry-listener: listening on {} for {} frames at {}", full_name, target_frames, chrono::Utc::now().to_rfc3339());

    let connecting = PipeOptions::new(&full_name).single()?;
    let mut server = connecting.wait()?;

    println!("client connected at {}", chrono::Utc::now().to_rfc3339());

    let mut incoming = String::new();
    let mut read_buf = vec![0u8; 8192];

    let mut frames_received: usize = 0;
    let mut last_seq: u64 = 0;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut drops: usize = 0;
    let mut parse_errors: usize = 0;

    // Track connection lifecycle and log connects/disconnects for debugging
    println!("waiting for client connection...");

    loop {
        match server.read(&mut read_buf) {
                Ok(0) => {
                println!("pipe closed by client at {} — processing any leftover buffer and attempting to accept new connection", chrono::Utc::now().to_rfc3339());

                // If there's any leftover partial data that didn't end with a newline,
                // try to process it as a final frame before reconnecting.
                if !incoming.trim().is_empty() {
                    let trimmed = incoming.trim();
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(parsed) => {
                            // Telemetry envelope expected
                            let seq = parsed.get("sequence").and_then(|v| v.as_u64())
                                .or_else(|| parsed.get("event").and_then(|e| e.get("data")).and_then(|d| d.get("sequence")).and_then(|s| s.as_u64()))
                                .unwrap_or(0);
                            if seq != 0 {
                                if last_seq != 0 && seq > last_seq + 1 {
                                    drops += (seq - last_seq - 1) as usize;
                                    eprintln!("frame jump detected (leftover): last={} new={} dropped={}", last_seq, seq, seq - last_seq - 1);
                                }
                                last_seq = seq;
                                frames_received += 1;
                                println!("recv # (leftover) {}/{} seq={}", frames_received, target_frames, seq);
                            }
                        }
                        Err(e) => {
                            eprintln!("leftover json parse error on close: {} data={} ", e, incoming);
                            parse_errors += 1;
                        }
                    }
                    incoming.clear();
                }

                // Try a reconnect loop: CreateNamedPipe can return ACCESS_DENIED
                // briefly while the previous instance is cleaning up. Retry longer.
                let mut reconnect_ok = false;
                for attempt in 0..20 {
                    match PipeOptions::new(&full_name).single() {
                        Ok(conn) => match conn.wait() {
                            Ok(new_server) => {
                                println!("client re-connected (after close)");
                                server = new_server;
                                incoming.clear();
                                reconnect_ok = true;
                                break;
                            }
                            Err(e) => {
                                eprintln!("reconnect wait failed attempt {}: {}", attempt, e);
                                std::thread::sleep(Duration::from_millis(200));
                                continue;
                            }
                        },
                        Err(e) => {
                            eprintln!("recreate pipe options failed attempt {}: {}", attempt, e);
                            std::thread::sleep(Duration::from_millis(200));
                            continue;
                        }
                    }
                }
                if !reconnect_ok {
                    eprintln!("failed to re-create pipe after client close; aborting");
                    break;
                }
            }
            Ok(n) => {
                incoming.push_str(&String::from_utf8_lossy(&read_buf[..n]));
                while let Some(pos) = incoming.find('\n') {
                    let mut line = incoming.drain(..=pos).collect::<String>();
                    if line.ends_with('\n') { line.pop(); }
                    if line.ends_with('\r') { line.pop(); }
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue }

                    // Try parse JSON
                    let parsed: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => { eprintln!("json parse error: {} line={}", e, trimmed); parse_errors += 1; continue; }
                    };

                    // Distinguish IpcEnvelope (has payload) vs TelemetryEnvelope
                    if parsed.get("payload").is_some() {
                        // ignore response/command envelopes
                        continue;
                    }

                    // If this is a TelemetryDone event, ACK it and finish.
                    if let Some(ev) = parsed.get("event").and_then(|e| e.get("event")).and_then(|v| v.as_str()) {
                        if ev == "TelemetryDone" {
                            println!("received TelemetryDone from sender at {} , sending drain_complete ack", chrono::Utc::now().to_rfc3339());
                            let ack = serde_json::json!({"ack": "drain_complete"});
                            let ack_s = match serde_json::to_string(&ack) {
                                Ok(s) => s,
                                Err(e) => { eprintln!("failed to serialize ack: {}", e); String::from("{\"ack\":\"drain_complete\"}") }
                            };
                            if let Err(e) = server.write_all(format!("{}\n", ack_s).as_bytes()) {
                                eprintln!("failed to write ack at {}: {}", chrono::Utc::now().to_rfc3339(), e);
                            } else {
                                println!("sent drain_complete ack at {}", chrono::Utc::now().to_rfc3339());
                            }
                            // After acking, break to finish and print summary
                            break;
                        }
                    }

                    // Telemetry envelope expected
                    let seq = parsed.get("sequence").and_then(|v| v.as_u64())
                        .or_else(|| parsed.get("event").and_then(|e| e.get("data")).and_then(|d| d.get("sequence")).and_then(|s| s.as_u64()))
                        .unwrap_or(0);

                    let ts_str = parsed.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                    let ts = DateTime::parse_from_rfc3339(ts_str).map(|dt| dt.with_timezone(&Utc));

                    if let Ok(tsv) = ts {
                        if let Some(prev) = last_ts {
                            if tsv <= prev {
                                eprintln!("timestamp not monotonic: {} <= {}", tsv, prev);
                            }
                        }
                        last_ts = Some(tsv);
                    }

                    if seq == 0 {
                        eprintln!("telemetry missing sequence: {}", trimmed);
                    } else {
                        if last_seq != 0 && seq > last_seq + 1 {
                            drops += (seq - last_seq - 1) as usize;
                            eprintln!("frame jump detected: last={} new={} dropped={}", last_seq, seq, seq - last_seq - 1);
                        }
                        last_seq = seq;
                    }

                    frames_received += 1;
                    println!("recv #{}/{} seq={}", frames_received, target_frames, seq);

                    if frames_received >= target_frames {
                        println!("target reached");
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("read error: {}", e);
                // On connection error, log and attempt to accept a new client.
                std::thread::sleep(Duration::from_millis(100));
                // Try to accept a new connection if the server is still open
                match PipeOptions::new(&full_name).single() {
                    Ok(conn) => match conn.wait() {
                        Ok(new_server) => {
                            println!("client re-connected");
                            server = new_server;
                            incoming.clear();
                            continue;
                        }
                        Err(e) => {
                            eprintln!("reconnect wait failed: {}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        eprintln!("recreate pipe options failed: {}", e);
                        continue;
                    }
                }
            }
        }
        if frames_received >= target_frames { break; }
    }

    println!("summary: frames_received={} drops={} parse_errors={}", frames_received, drops, parse_errors);
    Ok(())
}
