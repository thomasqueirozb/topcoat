# io_uring benchmark harness

Supporting material for the `io-uring` branch of this fork: the benchmark
app, the load generator, build/run scripts, evidence, and `RESULTS.md`
(full methodology, correctness results, and baseline-vs-fork-default
numbers; the io_uring-mode run is still outstanding because io_uring was
blocked by seccomp on the machine where this was developed).

The scripts assume this layout (a parent directory containing both trees):

```bash
git clone https://github.com/tokio-rs/topcoat.git upstream
git clone -b io-uring https://github.com/thomasqueirozb/topcoat.git fork
cp -r fork/iouring-bench/* .
./run-on-host.sh   # probes io_uring first; fails fast if blocked
```

`run-on-host.sh` rebuilds baseline / fork-default / fork-uring from the
same app, runs the uring-mode curl correctness check against
`evidence/curl-baseline.txt`, then benchmarks all three modes.
