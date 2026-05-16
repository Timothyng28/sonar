// Measures real MCP-path latency. Spawns `sonar mcp` as a subprocess,
// completes the JSON-RPC handshake, then runs N tool calls against it
// and times each round-trip (write request → read response).
//
// This is what Claude Code experiences when it calls the sonar tool —
// JSON-RPC framing + the actual search. The CLI's `--bench` mode only
// measures in-process query work; this measures the full MCP transport.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let query = args.get(1).cloned().unwrap_or_else(|| "alembic migration".to_string());
    let runs: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    // Resolve the sonar binary path: prefer release, fall back to debug.
    let bin = env::current_exe()?
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("sonar"))
        .context("locating sonar binary")?;

    println!("spawning: {} mcp", bin.display());
    let mut child = Command::new(&bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let mut reader = BufReader::new(stdout);

    // Handshake
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mcp_bench", "version": "0.0.1" }
            }
        }),
    )?;
    let _init = read_msg(&mut reader)?;
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )?;

    // Warmup
    for _ in 0..3 {
        send(
            &mut stdin,
            json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "tools/call",
                "params": {
                    "name": "sonar",
                    "arguments": { "query": query, "limit": 5 }
                }
            }),
        )?;
        let _ = read_msg(&mut reader)?;
    }

    // Timed runs
    let mut times_us: Vec<u128> = Vec::with_capacity(runs);
    for i in 0..runs {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1000 + i,
            "method": "tools/call",
            "params": {
                "name": "sonar",
                "arguments": { "query": query, "limit": 5 }
            }
        });
        let t0 = Instant::now();
        send(&mut stdin, req)?;
        let _resp = read_msg(&mut reader)?;
        times_us.push(t0.elapsed().as_micros());
    }

    // Shut the server down so the child doesn't linger.
    let _ = stdin; // drop, closes its end
    let _ = child.wait();

    times_us.sort();
    let n = times_us.len();
    let min = times_us[0];
    let median = times_us[n / 2];
    let p95 = times_us[(n as f64 * 0.95) as usize];
    let max = times_us[n - 1];
    let mean: u128 = times_us.iter().sum::<u128>() / n as u128;
    println!(
        "mcp bench: {} round-trips of query={:?} | min={}µs mean={}µs median={}µs p95={}µs max={}µs",
        runs, query, min, mean, median, p95, max
    );

    Ok(())
}

fn send(stdin: &mut ChildStdin, msg: Value) -> Result<()> {
    let mut s = serde_json::to_string(&msg)?;
    s.push('\n');
    stdin.write_all(s.as_bytes())?;
    stdin.flush()?;
    Ok(())
}

fn read_msg(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("mcp server closed stdout");
    }
    Ok(serde_json::from_str(&line)?)
}
