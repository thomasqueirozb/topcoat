# Topcoat + io_uring — prototype results

**Date:** 2026-11-07 · **Machine:** 4 vCPU (AMD EPYC 9D25), kernel `7.0.0-43-generic`, 7.9 GB RAM, loopback only
**Upstream:** tokio-rs/topcoat @ `4602ca4` ("refactor!: move more things into request context (#593)"), topcoat 0.11.0
**Toolchain:** rustc/cargo 1.99.1 (workspace MSRV)

## TL;DR

- The fork implements the agreed **hybrid split-plane** design behind an opt-in
  `io-uring` Cargo feature: a tokio-uring edge owns client sockets on one
  thread per core, parses complete HTTP/1.1 requests with owned buffers, and
  dispatches each *complete request* — once, not per read/write — over a
  channel to a normal multi-thread Tokio runtime where Topcoat handlers run
  unchanged. Responses travel back the same way.
- **io_uring is hard-blocked in this VM** by seccomp: `io_uring_setup` returns
  `EPERM` even as root and even from a transient systemd service in PID 1's
  context, with `/proc/sys/kernel/io_uring_disabled = 0`. So **no io_uring
  benchmark numbers exist** — the uring binary fails fast at startup with a
  clear error (by design, no silent fallback). Evidence below and in
  `evidence/`.
- Baseline (upstream) vs fork-in-default-mode benchmarks are provided; they
  are within noise of each other, confirming the feature costs nothing when
  disabled. The io_uring comparison must be re-run on a host where
  `io_uring_setup` is permitted.

## Design

Seam used: Topcoat's `serve` feature already isolates the listener —
`RouterService` (topcoat-router) turns an `http::Request` into a response via
`Router::handle` without owning any socket. The fork adds a second transport
next to the existing hyper-based `serve` path; the default path is untouched.

```
                 io_uring mode (feature = "io-uring")
 ┌──────────────────────── edge threads (1/core) ───────────────────────┐
 │ tokio-uring current-thread runtime, SO_REUSEPORT listener            │
 │ accept → read owned buffers → httparse → COMPLETE request  ──┐       │
 └──────────────────────────────────────────────────────────────┼───────┘
                                              tokio mpsc (cap 1024)
 ┌──────────────────────── handler runtime ─────────────────────▼───────┐
 │ normal multi-thread Tokio runtime                                    │
 │ RouterService::handle_buffered → Router::handle → app handlers       │
 │ (pages, components, loaders — 100% stock Topcoat/Tokio code)         │
 └──────────────────────────────────────────────────────────────┬───────┘
                                              oneshot reply: (status, headers, body)
 ┌──────────────────────── edge thread ───────────────────────▼───────┐
 │ encode HTTP/1.1 response → stream.write_all on the uring socket    │
 └────────────────────────────────────────────────────────────────────┘
```

- Crossing granularity is **per request**, buffers are **moved** (owned
  `Vec<u8>`), exactly as the design discussion concluded. Wakeups are not
  additionally batched in this prototype (one channel send per request);
  that is a listed future optimisation.
- Edge implementation: `tokio-uring` 0.5 (on `io-uring` 0.6.4) — chosen over
  raw `io-uring`, compio/cyper, or monoio because it offers safe, owned-buffer
  TCP APIs and per-thread runtimes with the smallest integration surface.
- Startup probes the ring first (`io_uring::IoUring::new(8)`) and returns a
  descriptive error if unavailable. There is **no epoll/poll fallback**.

## Files changed (fork/ vs upstream/)

| File | Change |
|---|---|
| `crates/topcoat-router/src/uring.rs` | **New, 402 lines.** Edge threads, listener, HTTP/1.1 parse/encode, dispatch channel plumbing, availability probe, `serve_uring_blocking`. |
| `crates/topcoat-router/src/service.rs` | Added `#[cfg(feature = "io-uring")] RouterService::handle_buffered` — wraps the stock handler, injects `RemoteAddr`, collects the response body. Default-mode code unchanged. |
| `crates/topcoat-router/src/lib.rs` | `mod uring` + re-export, both cfg-gated. |
| `crates/topcoat-router/Cargo.toml` | Feature `io-uring = ["serve", dep:tokio-uring, dep:io-uring, dep:httparse, dep:socket2, "tokio/rt-multi-thread"]`; optional deps. |
| `crates/topcoat/src/serve.rs` | Added `pub fn start_uring(...)` (cfg `io-uring`): same `HOST`/`PORT` env handling and defaults (127.0.0.1:3000) as `start`. |
| `crates/topcoat/src/lib.rs` | Re-export `start_uring` (cfg-gated). |
| `crates/topcoat/Cargo.toml` | Feature `io-uring = ["serve", "topcoat-router/io-uring"]`. |
| `Cargo.toml` (workspace) | Version pins: `tokio-uring 0.5`, `io-uring 0.6`, `httparse 1`, `socket2 0.5`. |
| `Cargo.lock` | Regenerated (new deps only). |

`unsafe` code: none added — the workspace's `unsafe_code = "deny"` lint holds
with the feature enabled (`cargo check -p topcoat --features io-uring` clean).

## API compatibility

Routes, pages, components, handlers, and `Router` construction are
**byte-identical** between modes — the benchmark app proves it: the same
`src/main.rs` is used for every build (verified with `diff`; see
`bench-app/` and `make-builds.sh`). The only differences are:

1. Cargo: enable the feature (`topcoat = { ..., features = ["io-uring"] }`).
2. **Entry point (the one user-facing change, per the approved fallback):**
   io_uring mode cannot run inside `#[tokio::main]`, because `tokio_uring`
   runtimes cannot be created inside another runtime and the edge must own
   the main threads. So:

   ```rust
   // default mode                          // io_uring mode
   #[tokio::main]                           fn main() {
   async fn main() {                            topcoat::start_uring(router()).unwrap();
       topcoat::start(router()).await       }
   }                                        // handlers still run on a Tokio
                                            // runtime built inside start_uring
   ```

With the feature disabled, nothing changes at all: no new required deps, no
API changes, identical behaviour (verified by tests + benchmarks below).

## Correctness

Harness: `evidence/correctness.sh` (start server → `curl -i` on 5 cases).
Transcripts: `evidence/curl-baseline.txt`, `evidence/curl-fork-default.txt`
(both 8,261 bytes).

| Check | Baseline | Fork (default) | Body identical |
|---|---|---|---|
| `GET /` | 200 | 200 | ✅ (102 B) |
| `GET /big` | 200 | 200 | ✅ (7,337 B) |
| `GET /missing` | 404 | 404 | ✅ |
| `HEAD /` | 200 | 200 | ✅ (empty) |
| `POST /` | 405 | 405 | ✅ |

Existing tests, fork in default mode: `cargo test -p topcoat-router` —
**742 passed, 0 failed** (621 + 117 + 4 doc-tests; 4 ignored upstream).
Full log: `evidence/cargo-test-topcoat-router.txt`.
`cargo check -p topcoat --features io-uring`: clean, 0 warnings.

io_uring mode correctness (curl parity in uring mode) **could not be run** in
this VM — the binary refuses to start (see next section). That verification
remains outstanding and must be done on an io_uring-capable host; the uring
path shares `RouterService`/`Router::handle` with the verified default path
and its wire encode/decode is the only new surface.

## Was io_uring verified active? — No: it is blocked in this VM

Four independent checks, all on 2026-11-07, all agree (`evidence/`):

1. **Rust probe** (`evidence/uring-probe.rs`,
   `evidence/io-uring-blocked.txt`): a minimal `tokio_uring::start` program
   panics at runtime creation — `PermissionDenied`.
2. **Raw syscall**: `io_uring_setup(256, …)` via ctypes → `-1`, errno 1
   (EPERM), as root. `/proc/sys/kernel/io_uring_disabled` = `0`, so this is
   not the kernel sysctl.
3. **strace** (`evidence/strace-uring-setup.txt`):
   `io_uring_setup(256, …) = -1 EPERM (Operation not permitted)`;
   `/proc/self/status` shows `Seccomp: 2` with 4–5 filters (the filter is
   also on PID 1).
4. **PID-1 context**: a transient systemd service running the same probe
   (`evidence/uring-probe-systemd-service.txt`) also gets
   `io_uring_setup -> -1 errno 1`. Seccomp is inherited and cannot be
   removed from inside, so no in-VM workaround exists.

Consequence, by design: the uring transport checks ring availability first
and fails loudly. Captured (`evidence/uring-mode-startup.txt`):

```
Error: Custom { kind: PermissionDenied, error: "io_uring is not available on
this system (Operation not permitted (os error 1)); the io-uring transport
requires Linux with io_uring enabled and not blocked by seccomp or the
kernel.io_uring_disabled sysctl" }
```

**Therefore there are no io_uring performance numbers in this report, and
nothing below is presented as one.** Re-run on a host without the seccomp
block (e.g. a bare-metal/standard cloud VM where `io_uring_setup` succeeds)
using the exact binaries/harness in this directory.

## Benchmarks (baseline vs fork-default only — see above)

**App** (`bench-app/`): one Topcoat app, identical source for all builds —
`GET /` renders a hello-world page (102 B body); `GET /big` renders 40
components (7,337 B body). Builds differ only in the topcoat path
(upstream vs fork) — `make-builds.sh` regenerates both build trees from the
single canonical source; binaries in `binaries/`.

**Load generator:** `wrk` was unavailable in this environment (apt package
could not be installed reliably), so a purpose-built Rust keep-alive load
generator was used (`loadgen/`, ~200 lines, Tokio): N persistent HTTP/1.1
connections, sequential requests per connection, per-request latency
histogram (1 µs buckets). 3 s warm-up, then 10 s measured runs, ×2 runs per
cell. Server CPU sampled from `/proc/<pid>/stat` across each run.
Command per run, e.g.:
`loadgen http://127.0.0.1:3231/big 500 10`. Raw outputs: `results/`.

Caveat: load generator and server share the same 4 vCPUs, so absolute
numbers are far below what separate hosts would show and p99 has large
scheduling spikes in both modes (visible in raw files). p50/p95 and req/s
are the stable signals; both configurations suffer the same contention.

### `GET /` (102 B)

| Mode | Conn | Run | req/s | p50 | p95 | p99 | Server CPU |
|---|---|---|---|---|---|---|---|
| upstream baseline | 50 | 1 | 42,372 | 0.49 ms | 1.04 ms | 53.1 ms | 102% |
| upstream baseline | 50 | 2 | 42,422 | 0.50 ms | 1.03 ms | 52.2 ms | 102% |
| fork, default mode | 50 | 1 | 43,362 | 0.47 ms | 1.09 ms | 52.6 ms | 100% |
| fork, default mode | 50 | 2 | 38,835 | 0.54 ms | 1.08 ms | 54.9 ms | 102% |
| upstream baseline | 500 | 1 | 44,873 | 4.06 ms | 66.6 ms | 70.7 ms | 103% |
| upstream baseline | 500 | 2 | 41,349 | 4.40 ms | 67.5 ms | 71.8 ms | 104% |
| fork, default mode | 500 | 1 | 38,294 | 4.77 ms | 68.2 ms | 72.4 ms | 103% |
| fork, default mode | 500 | 2 | 40,012 | 4.53 ms | 67.6 ms | 71.9 ms | 103% |

### `GET /big` (7,337 B)

| Mode | Conn | Run | req/s | p50 | p95 | p99 | Server CPU |
|---|---|---|---|---|---|---|---|
| upstream baseline | 50 | 1 | 21,892 | 0.94 ms | 1.87 ms | 60.0 ms | 117% |
| upstream baseline | 50 | 2 | 23,341 | 0.88 ms | 1.87 ms | 59.6 ms | 118% |
| fork, default mode | 50 | 1 | 19,884 | 1.01 ms | 2.07 ms | 61.2 ms | 117% |
| fork, default mode | 50 | 2 | 20,512 | 0.98 ms | 2.03 ms | 60.9 ms | 117% |
| upstream baseline | 500 | 1 | 21,790 | 9.13 ms | 76.3 ms | 82.4 ms | 118% |
| upstream baseline | 500 | 2 | 25,028 | 7.09 ms | 73.4 ms | 79.5 ms | 118% |
| fork, default mode | 500 | 1 | 21,413 | 9.52 ms | 77.0 ms | 84.9 ms | 118% |
| fork, default mode | 500 | 2 | 21,729 | 8.90 ms | 76.1 ms | 82.6 ms | 118% |

(Full raw outputs: `results/loadgen-*.txt`.) 0 errors in all runs.

**Reading:** fork-default ≈ baseline within run-to-run noise (same code
path — the transport was not touched in default mode). This validates that
adopting the fork with the feature off is free; it says nothing about
io_uring performance, which remains unmeasured for the reasons above.

## Limitations of the io_uring prototype

- **HTTP/1.1 only.** No HTTP/2, no TLS (hyper's TLS/Http2 paths are not wired
  to the edge), no WebSocket/SSE or other upgrades — upgrade requests are
  refused by the parser path rather than silently mishandled.
- No chunked *request* bodies and no `Expect: 100-continue` (requests using
  them fail parsing and the connection is closed); request bodies are
  buffered whole (16 MB cap; 64 KB header cap).
- HEAD is handled (headers + content-length, no body). Keep-alive and
  pipelining-free sequential requests per connection are supported.
- No graceful shutdown (Ctrl-C kills the process; edge threads are detached)
  and the CLI dev-server "ready" notification is not performed in uring mode.
- One channel hop per request; no wakeup batching, no zero-copy response
  path, response bodies are fully buffered (inherent to
  `handle_buffered`).
- Linux only (feature compiles the edge only on Linux; on other platforms
  `serve_uring_blocking` is unavailable).

## Reproducing

```bash
# builds (regenerates build-baseline/ and build-fork/ from bench-app/)
./make-builds.sh   # then cargo build --release in each tree
# correctness (baseline / fork-default / uring attempt)
./evidence/correctness.sh ./binaries/bench-baseline 3221 baseline
# tests
cd fork && cargo test -p topcoat-router
# benchmark one binary
./bench2.sh ./binaries/bench-baseline baseline 3231
# io_uring mode — on a host where io_uring_setup is permitted:
PORT=3000 ./binaries/bench-fork-uring   # currently: fails fast, see evidence
```

## Directory map

- `upstream/` — pristine tokio-rs/topcoat @ 4602ca4 (baseline)
- `fork/` — the fork with the `io-uring` feature
- `bench-app/`, `make-builds.sh`, `build-baseline/`, `build-fork/` — the
  shared benchmark app and its two build trees
- `binaries/` — `bench-baseline`, `bench-fork-default`, `bench-fork-uring`
- `loadgen/` — Rust load generator source
- `bench.sh` (wrk variant, unused — wrk uninstallable here), `bench2.sh`
- `results/` — raw load-generator outputs + server logs
- `evidence/` — io_uring-block probes (syscall, strace, systemd, Rust),
  uring-mode startup transcript, curl correctness transcripts, test log
