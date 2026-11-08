//! Experimental io_uring edge transport (feature `io-uring`).
//!
//! Split-plane design: a small number of edge threads (one per core) each
//! own a `tokio-uring` runtime and a `SO_REUSEPORT` listener. The edge owns
//! the client sockets end to end — accept, recv, HTTP/1.1 parsing, send —
//! and never runs application code. Each *complete* request is dispatched
//! exactly once over a channel to a shared multi-thread Tokio runtime,
//! where the router handles it with the normal Tokio APIs application code
//! expects. The collected response travels back over a oneshot channel and
//! the edge thread that owns the connection writes it.
//!
//! The crossing is per request, not per read/write, and buffers are moved,
//! not copied more than the one collect of the response body (which the
//! buffered dispatch requires anyway).
//!
//! Limitations of this prototype: HTTP/1.1 only (no HTTP/2, no TLS, no
//! WebSocket/SSE upgrades), no chunked request bodies, no
//! `Expect: 100-continue`, request bodies are fully buffered with a size
//! cap, and there is no graceful shutdown — the process is expected to be
//! stopped by a signal, which terminates it as usual. Requests that need
//! the excluded features must use the default transport.

use std::{io, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::{Body, HeaderMap, RouterService, StatusCode, request::Request};

/// Largest request head (request line + headers) the edge will buffer.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest request body the edge will buffer before dispatching.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Read chunk size for one io_uring recv operation.
const READ_CHUNK_BYTES: usize = 16 * 1024;
/// Requests waiting between the edge threads and the handler runtime.
const DISPATCH_QUEUE: usize = 1024;

type BufferedResponse = (StatusCode, HeaderMap, Bytes);
type Job = (
    Request,
    Option<SocketAddr>,
    oneshot::Sender<BufferedResponse>,
);

/// Serves `service` on `addr` with the io_uring edge transport, blocking
/// the calling thread until an edge thread fails.
///
/// Spawns one edge thread per available core, plus a multi-thread Tokio
/// runtime for the handlers. Returns an error immediately when io_uring
/// cannot be created on this system — this transport never falls back to
/// `epoll` silently.
///
/// # Errors
///
/// Returns an error if io_uring is unavailable, if a listener cannot be
/// bound, or if an edge thread's accept loop fails.
pub fn serve_uring_blocking(service: RouterService, addr: SocketAddr) -> io::Result<()> {
    // Probe the ring first so an unsupported system fails here, with a
    // clear message, instead of panicking inside an edge thread.
    check_uring_available()?;

    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());

    let handler_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("topcoat-handler")
        .enable_all()
        .build()?;

    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>(DISPATCH_QUEUE);
    let jobs_rx = Arc::new(tokio::sync::Mutex::new(jobs_rx));

    // One dispatcher task owns the receiving end; it spawns a task per
    // request so a slow handler does not hold up the queue.
    handler_runtime.spawn(async move {
        loop {
            let job = jobs_rx.lock().await.recv().await;
            let Some((request, remote_addr, respond)) = job else {
                break;
            };
            let service = service.clone();
            tokio::spawn(async move {
                let response = service.handle_buffered(request, remote_addr).await;
                let _ = respond.send(response);
            });
        }
    });

    let mut edge_threads = Vec::with_capacity(workers);
    for _ in 0..workers {
        let listener = bind_reuseport(addr)?;
        let jobs = jobs_tx.clone();
        edge_threads.push(std::thread::spawn(move || {
            tokio_uring::start(async move {
                let listener = tokio_uring::net::TcpListener::from_std(listener);
                loop {
                    let (stream, peer) = listener.accept().await?;
                    let jobs = jobs.clone();
                    tokio_uring::spawn(async move {
                        let _ = handle_connection(stream, peer, jobs).await;
                    });
                }
                #[allow(unreachable_code)]
                Ok::<(), io::Error>(())
            })
        }));
    }
    drop(jobs_tx);

    for thread in edge_threads {
        // An edge thread only returns when its accept loop errored (or the
        // runtime could not be created despite the probe). Propagate the
        // first failure instead of serving on silently fewer threads.
        match thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(io::Error::other(
                    "an io_uring edge thread panicked; io_uring may not be usable on this system",
                ));
            }
        }
    }
    Ok(())
}

/// Creates and drops one ring to prove io_uring works on this system.
fn check_uring_available() -> io::Result<()> {
    io_uring::IoUring::new(8).map(|_| ()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "io_uring is not available on this system ({error}); the io-uring \
                 transport requires Linux with io_uring enabled and not blocked by \
                 seccomp or the kernel.io_uring_disabled sysctl"
            ),
        )
    })
}

/// Binds a `SO_REUSEPORT` TCP listener so every edge thread owns one and
/// the kernel load-balances connections between them.
fn bind_reuseport(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Serves one connection: read and dispatch requests in order until the
/// peer closes it, a response ends keep-alive, or an error occurs.
async fn handle_connection(
    stream: tokio_uring::net::TcpStream,
    peer: SocketAddr,
    jobs: mpsc::Sender<Job>,
) -> io::Result<()> {
    let mut buffered: Vec<u8> = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut read_buf = vec![0u8; READ_CHUNK_BYTES];

    loop {
        // Try to dispatch every complete request already buffered, reading
        // more from the socket only when the buffer holds a partial one.
        let parsed = match parse_request(&buffered) {
            Parse::Incomplete => None,
            Parse::BadRequest => {
                let response = plain_response(
                    StatusCode::BAD_REQUEST,
                    "bad request",
                    false,
                );
                let (result, _) = stream.write_all(response).await;
                result?;
                return Ok(());
            }
            Parse::Complete(complete) => Some(complete),
        };

        if let Some(complete) = parsed {
            buffered.drain(..complete.consumed);
            let head_only = complete.request.method() == http::Method::HEAD;
            let (respond_tx, respond_rx) = oneshot::channel();
            if jobs
                .send((complete.request, Some(peer), respond_tx))
                .await
                .is_err()
            {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the handler runtime has shut down",
                ));
            }
            let Ok((status, headers, body)) = respond_rx.await else {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the handler dropped the request",
                ));
            };
            let response = encode_response(
                status,
                &headers,
                &body,
                complete.keep_alive,
                head_only,
            );
            let (result, _) = stream.write_all(response).await;
            result?;
            if !complete.keep_alive {
                return Ok(());
            }
            continue;
        }

        if buffered.len() > MAX_HEAD_BYTES + MAX_BODY_BYTES {
            let response = plain_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request too large",
                false,
            );
            let (result, _) = stream.write_all(response).await;
            result?;
            return Ok(());
        }

        let (result, buf) = stream.read(read_buf).await;
        read_buf = buf;
        let read = result?;
        if read == 0 {
            return Ok(());
        }
        buffered.extend_from_slice(&read_buf[..read]);
    }
}

/// The result of trying to parse one request out of the receive buffer.
enum Parse {
    /// More bytes are needed before a full request is present.
    Incomplete,
    /// The bytes present can never become a valid supported request.
    BadRequest,
    /// One full request was parsed.
    Complete(CompleteRequest),
}

struct CompleteRequest {
    /// Bytes to drop from the front of the buffer (head + body).
    consumed: usize,
    request: Request,
    keep_alive: bool,
}

/// Parses one HTTP/1.1 request (head plus `Content-Length` body) from
/// `buffered`. Chunked bodies and `Expect: 100-continue` are rejected as
/// bad requests by this prototype.
fn parse_request(buffered: &[u8]) -> Parse {
    let mut header_slots = [httparse::EMPTY_HEADER; 64];
    let mut head = httparse::Request::new(&mut header_slots);
    let head_len = match head.parse(buffered) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => {
            return if buffered.len() > MAX_HEAD_BYTES {
                Parse::BadRequest
            } else {
                Parse::Incomplete
            };
        }
        Err(_) => return Parse::BadRequest,
    };

    let mut content_length = 0usize;
    let mut keep_alive = head.version == Some(1);
    let mut headers = HeaderMap::with_capacity(head.headers.len());
    for header in head.headers.iter() {
        let name = header.name.to_ascii_lowercase();
        match name.as_str() {
            "content-length" => {
                let Ok(value) = std::str::from_utf8(header.value) else {
                    return Parse::BadRequest;
                };
                let Ok(parsed) = value.trim().parse::<usize>() else {
                    return Parse::BadRequest;
                };
                content_length = parsed;
            }
            "transfer-encoding" | "expect" => return Parse::BadRequest,
            "connection" => {
                let value = String::from_utf8_lossy(header.value).to_ascii_lowercase();
                if value.contains("close") {
                    keep_alive = false;
                } else if value.contains("keep-alive") {
                    keep_alive = true;
                }
            }
            _ => {}
        }
        let Ok(name) = http::HeaderName::from_bytes(header.name.as_bytes()) else {
            return Parse::BadRequest;
        };
        let Ok(value) = http::HeaderValue::from_bytes(header.value) else {
            return Parse::BadRequest;
        };
        headers.append(name, value);
    }

    if content_length > MAX_BODY_BYTES {
        return Parse::BadRequest;
    }
    let Some(consumed) = head_len.checked_add(content_length) else {
        return Parse::BadRequest;
    };
    if buffered.len() < consumed {
        return Parse::Incomplete;
    }

    let Some(method) = head.method else {
        return Parse::BadRequest;
    };
    let Ok(method) = http::Method::from_bytes(method.as_bytes()) else {
        return Parse::BadRequest;
    };
    let Some(path) = head.path else {
        return Parse::BadRequest;
    };
    let version = if head.version == Some(0) {
        http::Version::HTTP_10
    } else {
        http::Version::HTTP_11
    };
    let body = Bytes::copy_from_slice(&buffered[head_len..consumed]);
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = method;
    *request.uri_mut() = match path.parse() {
        Ok(uri) => uri,
        Err(_) => return Parse::BadRequest,
    };
    *request.version_mut() = version;
    *request.headers_mut() = headers;

    Parse::Complete(CompleteRequest {
        consumed,
        request,
        keep_alive,
    })
}

/// Encodes a collected response as HTTP/1.1. Framing headers from the
/// router (`content-length`, `transfer-encoding`, `connection`) are
/// replaced with the framing of the buffered body the edge actually sends.
fn encode_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    keep_alive: bool,
    head_only: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )
        .as_bytes(),
    );
    for (name, value) in headers {
        let lower = name.as_str();
        if lower == "content-length" || lower == "transfer-encoding" || lower == "connection" {
            continue;
        }
        out.extend_from_slice(lower.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    // Match the hyper transport's wire format: a `date` header on every
    // response, and `connection: close` only when actually closing
    // (HTTP/1.1 keep-alive is implicit, hyper sends no header for it).
    out.extend_from_slice(
        format!(
            "date: {}\r\n",
            httpdate::fmt_http_date(std::time::SystemTime::now())
        )
        .as_bytes(),
    );
    if !keep_alive {
        out.extend_from_slice(b"connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    if !head_only {
        out.extend_from_slice(body);
    }
    out
}

/// A minimal plain-text response for requests the edge itself rejects.
fn plain_response(status: StatusCode, text: &str, keep_alive: bool) -> Vec<u8> {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    encode_response(status, &headers, text.as_bytes(), keep_alive, false)
}
