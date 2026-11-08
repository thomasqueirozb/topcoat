# io_uring benchmark harness

Supporting material for the `io-uring` branch of this fork: the benchmark
app, the load generator, build/run scripts, evidence, and `RESULTS.md`
(full methodology, correctness results, and baseline-vs-fork-default
numbers; the io_uring-mode run is still outstanding because io_uring was
blocked by seccomp on the machine where this was developed).

Two ways to run:

**In-repo (simplest):** clone this fork, check out the `io-uring` branch,
and run the script from inside the clone. The fork tree is the repo
itself; upstream is cloned automatically (pinned to the baseline commit
`4602ca4`) into `iouring-bench/.upstream` on first use.

```bash
git clone -b io-uring https://github.com/thomasqueirozb/topcoat.git
cd topcoat
./iouring-bench/run-on-host.sh   # probes io_uring first; fails fast if blocked
```

**Sibling layout:** this directory sitting next to `fork/` and `upstream/`
clones is also supported; existing sibling trees are used as-is.

`run-on-host.sh` rebuilds baseline / fork-default / fork-uring from the
same app, runs the uring-mode curl correctness check against
`evidence/curl-baseline.txt`, then benchmarks all three modes.
