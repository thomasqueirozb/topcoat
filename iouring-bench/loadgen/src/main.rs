//! Minimal HTTP/1.1 keep-alive load generator.
//! Usage: loadgen <url> <connections> <duration_secs>
//! Prints: requests, req/s, errors, latency percentiles (microsecond
//! histogram, 1us..60s log-ish buckets via exact sorted sample of
//! per-connection counters is too big; we use a 100k-bucket histogram
//! over microseconds capped at 100ms plus an overflow count).
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const BUCKETS: usize = 100_001; // index = latency in microseconds, last = >=100ms

struct Stats {
    requests: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
    hist: Vec<AtomicU64>,
}

fn percentile(hist: &[AtomicU64], total: u64, p: f64) -> f64 {
    let target = (total as f64 * p).ceil() as u64;
    let mut acc = 0u64;
    for (i, b) in hist.iter().enumerate() {
        acc += b.load(Ordering::Relaxed);
        if acc >= target {
            return i as f64 / 1000.0; // ms
        }
    }
    100.0
}

async fn worker(url_host: String, path: String, deadline: Instant, stats: Arc<Stats>) {
    let req = format!(
        "GET {path} HTTP/1.1\r\nhost: {url_host}\r\naccept: */*\r\n\r\n"
    );
    'reconnect: while Instant::now() < deadline {
        let mut stream = match TcpStream::connect(&url_host).await {
            Ok(s) => s,
            Err(_) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue 'reconnect;
            }
        };
        let mut buf: Vec<u8> = Vec::with_capacity(16384);
        let mut chunk = [0u8; 8192];
        'conn: loop {
            if Instant::now() >= deadline {
                break 'reconnect;
            }
            let t0 = Instant::now();
            if stream.write_all(req.as_bytes()).await.is_err() {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                break 'conn;
            }
            // Read one full response: headers, then content-length body.
            buf.clear();
            let body_len: usize;
            let head_end: usize;
            loop {
                if let Some(pos) = find(&buf, b"\r\n\r\n") {
                    head_end = pos + 4;
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let mut len = 0usize;
                    for line in head.lines() {
                        if let Some(v) = line.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    body_len = len;
                    break;
                }
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        break 'conn;
                    }
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                if Instant::now() >= deadline + Duration::from_secs(5) {
                    break 'reconnect;
                }
            }
            // Any bytes past head_end already read count toward the body;
            // keep reading until the full body arrived. (Responses here
            // are one per request, no pipelining, so no leftover frames.)
            let mut have = buf.len() - head_end;
            while have < body_len {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        break 'conn;
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        have += n;
                    }
                }
            }
            let us = t0.elapsed().as_micros() as usize;
            let idx = us.min(BUCKETS - 1);
            stats.hist[idx].fetch_add(1, Ordering::Relaxed);
            stats.requests.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add((head_end + body_len) as u64, Ordering::Relaxed);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = &args[1]; // http://127.0.0.1:PORT/path
    let conns: usize = args[2].parse().unwrap();
    let dur: u64 = args[3].parse().unwrap();
    let stripped = url.strip_prefix("http://").unwrap();
    let (host, path) = match stripped.find('/') {
        Some(i) => (stripped[..i].to_string(), stripped[i..].to_string()),
        None => (stripped.to_string(), "/".to_string()),
    };
    let stats = Arc::new(Stats {
        requests: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        hist: (0..BUCKETS).map(|_| AtomicU64::new(0)).collect(),
    });
    let deadline = Instant::now() + Duration::from_secs(dur);
    let mut handles = Vec::new();
    for _ in 0..conns {
        handles.push(tokio::spawn(worker(
            host.clone(),
            path.clone(),
            deadline,
            stats.clone(),
        )));
    }
    for h in handles {
        let _ = h.await;
    }
    let total = stats.requests.load(Ordering::Relaxed);
    let errs = stats.errors.load(Ordering::Relaxed);
    let secs = dur as f64;
    println!("requests={total}");
    println!("errors={errs}");
    println!("req_per_sec={:.1}", total as f64 / secs);
    println!("p50_ms={:.3}", percentile(&stats.hist, total, 0.50));
    println!("p95_ms={:.3}", percentile(&stats.hist, total, 0.95));
    println!("p99_ms={:.3}", percentile(&stats.hist, total, 0.99));
    println!("bytes={}", stats.bytes.load(Ordering::Relaxed));
}
