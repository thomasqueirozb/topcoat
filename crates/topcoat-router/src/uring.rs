//! Experimental io_uring transport (feature `io-uring`).
//!
//! Thread-per-core design: one thread per core, each running a
//! `tokio-uring` runtime and a `SO_REUSEPORT` listener, so the kernel
//! load-balances connections across cores. A connection is handled end
//! to end on the thread that accepted it — accept, recv, HTTP/1.1
//! parsing, **the route handler itself**, and send — with no channel
//! hops, no cross-thread wakeups, and no second runtime competing for
//! the same cores.
//!
//! An earlier version of this transport dispatched each request over a
//! channel to a shared multi-thread Tokio runtime and the response back
//! over a oneshot. Benchmarking showed that crossing cost more than the
//! ring saved at every load level (up to ~30% slower than the default
//! hyper transport at 500 connections), so the dispatch was removed:
//! handlers now run on the edge threads, the way other io_uring
//! frameworks (monoio, glommio) are structured.
//!
//! The trade-off is the runtime model application code runs under in
//! this mode: a current-thread `tokio-uring` runtime per core. Code
//! that needs Tokio's epoll-backed networking (`tokio::net`, and with
//! it most database drivers and HTTP clients) or that blocks a thread
//! for a long time is a poor fit — a blocking handler stalls every
//! connection on its core. Such applications should keep the default
//! transport; the application code itself is identical either way.
//!
//! Limitations of this prototype: HTTP/1.1 only (no HTTP/2, no TLS, no
//! WebSocket/SSE upgrades), no chunked request bodies, no
//! `Expect: 100-continue`, request bodies are fully buffered with a size
//! cap, and there is no graceful shutdown — the process is expected to be
//! stopped by a signal, which terminates it as usual. Requests that need
//! the excluded features must use the default transport.

use std::{io, net::SocketAddr};

use bytes::Bytes;

use crate::{Body, HeaderMap, RouterService, StatusCode, request::Request};

/// Largest request head (request line + headers) the edge will buffer.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest request body the edge will buffer before dispatching.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Read chunk size for one io_uring recv operation.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Serves `service` on `addr` with the io_uring transport, blocking the
/// calling thread until an edge thread fails.
///
/// Spawns one edge thread per available core; each thread runs the
/// handlers for the connections it accepts. Returns an error immediately
/// when io_uring cannot be created on this system — this transport never
/// falls back to `epoll` silently.
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

    let mut edge_threads = Vec::with_capacity(workers);
    for _ in 0..workers {
        let listener = bind_reuseport(addr)?;
        let service = service.clone();
        edge_threads.push(std::thread::spawn(move || {
            tokio_uring::start(async move {
                let listener = tokio_uring::net::TcpListener::from_std(listener);
                loop {
                    let (stream, peer) = listener.accept().await?;
                    let service = service.clone();
                    tokio_uring::spawn(async move {
                        let _ = handle_connection(stream, peer, service).await;
                    });
                }
                #[allow(unreachable_code)]
                Ok::<(), io::Error>(())
            })
        }));
    }

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
    // Accepted sockets inherit TCP_NODELAY from the listener on Linux,
    // and tokio-uring streams expose no setter of their own.
    socket.set_nodelay(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Serves one connection: read and handle requests in order until the
/// peer closes it, a response ends keep-alive, or an error occurs.
async fn handle_connection(
    stream: tokio_uring::net::TcpStream,
    peer: SocketAddr,
    service: RouterService,
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
            // The handler runs here, on this connection's edge thread —
            // no dispatch, no wakeup of another runtime.
            let (status, headers, body) =
                service.handle_buffered(complete.request, Some(peer)).await;
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
    // Like hyper, the formatted date is cached for the current second —
    // formatting it per response is measurable at high request rates.
    thread_local! {
        static CACHED_DATE: std::cell::RefCell<(u64, String)> =
            const { std::cell::RefCell::new((0, String::new())) };
    }
    let date = CACHED_DATE.with(|cache| {
        let now = std::time::SystemTime::now();
        let secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut cache = cache.borrow_mut();
        if cache.0 != secs {
            cache.1 = httpdate::fmt_http_date(now);
            cache.0 = secs;
        }
        cache.1.clone()
    });
    out.extend_from_slice(format!("date: {date}\r\n").as_bytes());
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
