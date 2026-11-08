# Running the Topcoat io_uring benchmark on another host

**Why this exists:** the fork is done and the baseline-vs-fork-default
numbers are in `RESULTS.md`, but the io_uring numbers could not be produced
in the Muse VM: `io_uring_setup` returns `EPERM` there even as root, because
a seccomp filter is inherited from PID 1 (`evidence/io-uring-blocked.txt`).
No in-VM workaround exists — checked 2026-11-07: no `/dev/kvm`, no
`vmx`/`svm` CPU flags, no QEMU/Docker/Podman installed, and
`systemd-nspawn`/`unshare` cannot lift an inherited seccomp filter.

## What you need

A plain Linux VM where `io_uring_setup` is permitted — i.e. a normal KVM
cloud VM, **not** a container. Docker's default seccomp profile blocks
`io_uring_setup`/`io_uring_enter`/`io_uring_register`, so running inside
Docker without `--security-opt seccomp=unconfined` reproduces the same
block. A 4 vCPU / 8 GB box matches the original machine; 2 vCPU works too,
just expect lower absolute numbers (compare modes against each other, not
against the old absolute figures — the load generator shares the box).

Cheap options, re-checked 2026-11-08 (verify before ordering — Hetzner
repriced twice in 2026 and its cheap line is currently out of stock):

| Host | Size | Indicative price |
|---|---|---|
| **Vultr — cheapest orderable** | Regular Cloud Compute, 2 vCPU / 4 GB | **$20/mo, $0.030/hr** — a few hours costs cents |
| DigitalOcean | Basic Droplet, 2 vCPU / 4 GiB | $24/mo, $0.03571/hr, per-second billing with a monthly cap — spin up, run, destroy |
| Hetzner Cloud | CX23/CX33 (2–4 vCPU) | **Currently not available** (marked so since Oct 2026, no restock date given); the orderable equivalent CPX22 is ~€19.99/mo — so Hetzner is no longer the cheap option for this |

Any of these is a few dollars for an afternoon if destroyed afterwards.
No account of Thomas's is known to be connected, so no host is pre-chosen.

## How to run it

```bash
# 1. On the new host (Ubuntu 24.04+ / Debian 12+ assumed):
sudo apt-get update && sudo apt-get install -y curl python3 build-essential pkg-config libssl-dev

# 2. Copy this directory over (see make-pack.sh), e.g.:
scp topcoat-iouring-pack.tar.gz user@HOST:~/
ssh user@HOST
mkdir -p topcoat-iouring && tar xzf topcoat-iouring-pack.tar.gz -C topcoat-iouring && cd topcoat-iouring
# (the tarball has no top-level directory, so extract it into one first —
# a plain `tar xzf` followed by `cd topcoat-iouring` will not work)

# 3. Run it — it probes io_uring first and stops with a clear error if blocked:
./run-on-host.sh
```

That script rebuilds all three binaries from the same bench app
(baseline, fork-default, fork-uring), runs the **outstanding uring-mode
curl correctness check** (5 cases, diffed against the verified baseline
transcript in `evidence/curl-baseline.txt`), then benchmarks each mode
with the same Rust load generator and settings as `RESULTS.md`
(10 s runs ×2, 50 and 500 keep-alive connections, `/` and `/big`).
Raw outputs land in `results/`.

## What to bring back

- `results/curl-fork-uring.txt` — should be byte-identical to `evidence/curl-baseline.txt`
- `results/loadgen-fork-uring-*.txt` vs `loadgen-baseline-*.txt` — req/s, p50/p95/p99
- If the uring binary fails to start, the startup error + the probe output
  from step 1 of the script (that tells us whether the new host blocks it too)
