#!/bin/bash
# run-on-host.sh — run the Topcoat io_uring benchmark on a host where
# io_uring_setup is permitted (a normal bare-metal / KVM cloud VM).
# Run from inside the unpacked topcoat-iouring/ directory.
# It FAILS FAST if io_uring is blocked, so it can never produce fake numbers.
# Steps: probe -> install Rust if needed -> rebuild all three binaries
# -> correctness in uring mode (the outstanding check) -> bench baseline
# vs fork-default vs fork-uring, same app, same harness as RESULTS.md.
set -euo pipefail
cd "$(dirname "$0")"

echo "== 1. io_uring probe =="
python3 - <<'PY'
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
nr = 425 if os.uname().machine == "x86_64" else 426  # io_uring_setup
# io_uring_setup copies the params struct from user memory, so it needs a
# real zeroed buffer (io_uring_params is 120 bytes). Passing None makes a
# WORKING kernel return EFAULT, which looks exactly like a block — an
# earlier version of this probe did that and misreported healthy hosts.
params = ctypes.create_string_buffer(128)
fd = libc.syscall(nr, 8, params)
if fd < 0:
    e = ctypes.get_errno()
    print(f"io_uring_setup(8) -> -1 errno {e} ({os.strerror(e)}) — BLOCKED, stopping here")
    sys.exit(3)
print(f"io_uring_setup(8) -> fd {fd} — io_uring available")
os.close(fd)
PY
echo "sysctl kernel.io_uring_disabled: $(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo unknown)"
grep -H . /proc/self/status | grep -i seccomp || true

echo "== 2. Rust toolchain =="
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found — installing rustup (stable)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
cargo --version; rustc --version

echo "== 3. Rebuild (same app, three binaries) =="
./make-builds.sh
(cd build-baseline && cargo build --release)
(cd build-fork && cargo build --release)
(cd build-fork && cargo build --release --features io-uring)
mkdir -p binaries results
cp build-baseline/target/release/bench-app binaries/bench-baseline
cp build-fork/target/release/bench-app binaries/bench-fork-uring
# fork-default must be built WITHOUT the feature; rebuild it after the
# io-uring build overwrote build-fork/target/release/bench-app.
(cd build-fork && cargo build --release)
cp build-fork/target/release/bench-app binaries/bench-fork-default
(cd loadgen && cargo build --release)

echo "== 4. Correctness in uring mode (outstanding check) =="
./evidence/correctness.sh ./binaries/bench-fork-uring 3241 results/curl-fork-uring.txt
# The `date` header value depends on when each transcript was captured,
# so date lines are stripped from both sides before diffing; everything
# else (status, headers, bodies) must match byte for byte.
strip_dates() { sed -i '/^date: /d' "$1"; }
cp results/curl-fork-uring.txt results/curl-fork-uring-nodate.txt
cp evidence/curl-baseline.txt results/curl-baseline-nodate.txt
strip_dates results/curl-fork-uring-nodate.txt
strip_dates results/curl-baseline-nodate.txt
diff results/curl-fork-uring-nodate.txt results/curl-baseline-nodate.txt && echo "uring curl output IDENTICAL to baseline (modulo date values)" || echo "NOTE: uring curl output differs from baseline — inspect results/curl-fork-uring.txt vs evidence/curl-baseline.txt"

echo "== 5. Benchmarks (bench2.sh logic, relative paths) =="
LG="$PWD/loadgen/target/release/loadgen"
bench_one() {
  local BIN="$1" NAME="$2" PORT="$3"
  PORT=$PORT "$BIN" > "results/server-$NAME.log" 2>&1 &
  local PID=$!
  for i in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break || sleep 0.2; done
  cpu_ticks() { awk '{print $14+$15}' /proc/$PID/stat; }
  local CLK; CLK=$(getconf CLK_TCK)
  run() {
    local path="$1" conn="$2" label="$3"
    local f="results/loadgen-$NAME-$label.txt"
    local t0; t0=$(cpu_ticks)
    "$LG" "http://127.0.0.1:$PORT$path" "$conn" 10 > "$f" 2>&1
    local t1; t1=$(cpu_ticks)
    python3 -c "print(f'server_cpu_percent_avg={(($t1-$t0)/$CLK/10*100):.0f}')" >> "$f"
    echo "$NAME $label: $(grep req_per_sec "$f" || echo no-result)"
  }
  "$LG" "http://127.0.0.1:$PORT/" 10 3 > /dev/null 2>&1
  for r in 1 2; do
    run /    50  "root-c50-r$r"
    run /    500 "root-c500-r$r"
    run /big 50  "big-c50-r$r"
    run /big 500 "big-c500-r$r"
  done
  kill -TERM $PID 2>/dev/null; wait $PID 2>/dev/null || true
  echo "ALL DONE $NAME"
}
bench_one ./binaries/bench-baseline     baseline     3251
bench_one ./binaries/bench-fork-default fork-default 3252
bench_one ./binaries/bench-fork-uring   fork-uring   3253
echo "== DONE. Raw outputs in results/ — compare loadgen-*-*.txt =="
