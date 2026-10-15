# Streaming footprint measurement

The streaming transport is a build feature of the serving worker
(`TP_ENABLE_STREAMING_GRPC`), linked statically from the pinned vcpkg
feature. What it adds to the worker is bounded by three budgets, and the
release qualifies the feature against them on the Jetson Orin Nano row
(`jetson-orin-nano-8gb-jp62`), with the release build configuration. The
budgets are footprint limits, not speech benchmarks: a measurement over a
budget blocks the transport's qualification until the implementation is
reduced or a reviewed architecture change moves the budget. Nothing selects
another transport.

`tools/validation/streaming-footprint.sh` is the harness, and
`tools/validation/streaming_footprint_record.py` builds and checks the
record it writes.

## The budgets are declared, not measured

[`tools/validation/streaming-footprint-thresholds.json`](../../tools/validation/streaming-footprint-thresholds.json)
holds the budgets, the number of runs and the stream count the steady-state
case holds open. They were written before the harness measured anything and
the harness cannot change them: it copies the file into every record as
read, and the check refuses a record whose copy differs from the file in
the checkout running the check.

| Budget | Maximum delta, with streaming minus without | What is measured |
| --- | --- | --- |
| `installed_size` | 32 MiB | the sum of the stripped sizes of every ELF file the serving package installs |
| `idle_rss` | 64 MiB | the worker's `VmRSS` once `/health` reports `ready`, the with-streaming worker's stream listener is bound, and no stream is open |
| `steady_rss` | 128 MiB | the worker's `VmRSS` while 16 bounded synthetic streams are held open |

Each budget is measured three times per side. A run's value is the largest
sample it took; a budget's maximum delta is the largest with-streaming run
minus the smallest without-streaming run, the worst pairing of the six.
The budgets are defined only for the static link the release jobs produce.
The harness cannot tell a static worker from any other, so the packages it
is given must be the release configuration's; the package closure check in
the release workflow is what asserts the link model.

## Two packages of the same configuration

The harness compares two `tensorplate-serving` packages built from the
same source and configuration, differing only in the feature. Both come
from the release builder in snapshot mode, on the machine or for it:

```bash
# With the feature: the release configuration, through the pinned vcpkg checkout.
VCPKG_ROOT=<vcpkg checkout at vcpkg.json's builtin-baseline> \
  tools/release/build-release-artifacts.sh --snapshot --branch develop \
  --artifacts-dir /var/tmp/footprint/with

# Without it: the same configuration, no vcpkg.
tools/release/build-release-artifacts.sh --snapshot --branch develop \
  --without-streaming --artifacts-dir /var/tmp/footprint/without
```

`--without-streaming` builds the snapshot without the feature and without
vcpkg; no release job passes it (see
[`docs/release/artifacts.md`](../release/artifacts.md)).

The harness does not know which package is which beyond what it is told.
It records each package's file name, SHA-256, version and architecture,
and each worker's `--version` output as printed, so the claim can be
checked against the record later by anyone who can reproduce the builds.

## Running it

On the machine the budgets are for, with both packages present:

```bash
tools/validation/streaming-footprint.sh \
  --with-streaming /var/tmp/footprint/with/tensorplate-serving_<version>_arm64.deb \
  --without-streaming /var/tmp/footprint/without/tensorplate-serving_<version>_arm64.deb \
  --out /var/tmp/footprint/record \
  --stream-driver <driver>
```

For each side and each run the harness extracts the package with
`dpkg-deb -x`, strips a copy of every ELF file it installs and records both
sizes and the `size` breakdown, starts the extracted worker on a free
loopback port with the packaged configuration (the mock session: no model,
no sidecar), waits for `/health` to report `ready`, lets it settle, samples
`VmRSS` from `/proc/<pid>/status`, runs the stream driver if one was given
and samples again while the streams are held, then stops the worker. The
settle time, the sample count and the interval are options (`--help` lists
them); the defaults are five seconds, ten samples a second apart. A package
for another architecture, a worker that is not an ELF executable or never
reports ready, a driver that does not hold its streams, and a record
directory that already holds captures all stop the run rather than produce a
record with a gap.

The with-streaming worker runs with a copy of that configuration that adds
`streaming: {enabled: true, port: 0}`, so it also binds its stream listener
on an ephemeral loopback port. With the mock session there is no dispatch
behind it, and it refuses every `Open`. The other side keeps the packaged
file, because a worker built without the feature refuses `streaming.enabled`
at start. `/health` reports `ready` a few milliseconds before the listener
is bound, so after `ready` the harness waits for the worker's
`stream listener bound` log line, up to the ready timeout, before it lets
the worker settle. Each run records whether the listener started. A
with-streaming run in which it did not start leaves the idle and steady
budgets incomplete, because those budgets are defined with the listener
running.

The exit status is the verdict: 0 when every budget is complete and within
its limit, 1 when every budget is complete and one is over, 2 when there is
no verdict because the record is incomplete or could not be assembled.

### The stream driver

The steady-state case needs something that opens the declared number of
streams against the running worker and holds them. The harness defines the
hook and ships no driver. The worker serves one stream session at a time,
and with the mock session its listener refuses every `Open`, so nothing can
hold the declared count yet. The driver comes with the measurement that
first needs it, and with the stream port, which the harness does not pass
to a driver today. The contract:

- The driver is an executable given with `--stream-driver`, run once per
  run after the worker is ready, with `TP_FOOTPRINT_WORKER_URL` (the
  worker's `http://127.0.0.1:<port>` base), `TP_FOOTPRINT_WORKER_PID` and
  `TP_FOOTPRINT_STREAMS` (the declared count) in its environment and a
  pipe on its standard input.
- It opens that many streams, prints the line `held` on its standard
  output once every stream is open and accepted, keeps them open until its
  standard input reaches end of file, releases them and exits 0.
- Its standard error is kept beside the run's other captures.

Without a driver every run records the steady state as not run, with the
reason, and the record is incomplete: the harness exits 2 and the
summary says so. A record with no steady-state measurement is never a
pass. `test/validation/fixtures/streaming_footprint_fakes/driver.sh` is the
contract against the test's fake worker, not a driver for the real one.

## What the harness writes

```
<out>/
  captures/                  the raw captures, kept whole
    thresholds.json          the budgets as read
    host.txt                 machine and kernel
    with_streaming/
      package.txt            file name, sha256, version, architecture
      version-output.txt     the worker's --version lines
      serving_worker.json    the config the worker ran with
      run-1/
        elf-files.tsv        path, installed bytes, stripped bytes
        size.txt             size(1) of each stripped file
        health.json          the /health body that reported ready
        idle-vmrss.tsv       time (ns), VmRSS (KiB), one line per sample
        steady-vmrss.tsv     the same while the streams were held, or
        steady-not-run.txt   why the steady state was not measured
        steady-streams.txt   the stream count the driver was told to hold
        worker.log           the worker's output
        stream-listener.txt  started or not_started
        worker-exit.txt      how it stopped
        driver.out, driver.log
      run-2/ run-3/
    without_streaming/       the same
  record.json                the record, shape config/schemas/streaming_footprint_record.json
  summary.json               what is published
```

The record carries the raw samples and file sizes of every run beside the
values, deltas and statuses derived from them. `streaming_footprint_record.py
check <record>` validates the record against its schema, requires its
thresholds to equal the declared file, recomputes every derived field from
the samples and refuses the record when anything differs: a missing run, a
run with no sample, a steady state held with fewer streams than declared,
a delta or status that does not follow from the samples. A record the
check refuses has no verdict, whatever its `result` says.

The summary is the record without the samples: the budgets, the package
identities and `--version` lines, each budget's per-run values, maximum
delta and status, the verdict, and the SHA-256 of the record it was
derived from. `check` must accept the record before a summary is written.

## What is filed

A footprint measurement is a qualification result, not lifecycle evidence
for a Production row. Its summary is public and its record is not: file
`summary.json` under
`docs/validation/evidence/<version>/jetson-orin-nano-8gb-jp62/streaming-footprint/`
with a README that names the two builds and the driver, and keep
`record.json` and the whole `captures/` directory with the private release
evidence. The record's SHA-256 in the summary ties the two together. The
summary holds the machine architecture, the kernel version string, package
file names and digests, and numbers; it holds no host name, path or
account, so the publication scanner has nothing in it to replace, but it is
still scanned before it is committed, with the private literal file, like
every other recorded file. A filed summary carries `provenance: recorded`;
a `synthetic` one comes from the tool's own tests, and `check` says so
when it reads one.

## What cannot be measured yet

The steady-state budget needs the declared number of streams held open at
once. The worker serves one stream session at a time, and with the mock
session its listener refuses every `Open`. So no driver exists, and every
record is incomplete on that budget. The size and idle budgets can be
measured: the with-streaming worker links the gRPC closure, and it starts
its listener before the idle samples are taken. The measurement that
qualifies the feature is taken on the Jetson Orin Nano, from both release
builds of the same commit, with a driver that holds real streams. Its
summary is filed as above, with the version it was taken for. A run on any
other machine, an amd64 workstation for one, is informational: nothing files
it.
