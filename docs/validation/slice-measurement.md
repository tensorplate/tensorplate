# Streaming slice measurement

`tools/validation/slice_measure.py` measures the streaming path from the
caller's side: paced speech-to-text sessions and text-to-speech segments
over the stream session envelope
([`protocol/README.md`](../../protocol/README.md#streaming-session-envelope)),
sequentially or in concurrent batches, with a timestamp at every boundary
a caller can see. It writes one record, and
`tools/validation/slice_measurement_record.py` checks it. The record's shape is
[`config/schemas/slice_measurement_record.json`](../../config/schemas/slice_measurement_record.json).

It is a measurement tool, not the SDK, and it imports nothing of the SDK. It
generates Python bindings from `session.proto` when it starts and speaks
the envelope directly, so what it times is the transport and the server.

A record states what one run observed on one machine. It evaluates no
latency gate and is never Production lifecycle evidence.

## What is measured

Every timestamp is read from the measuring process's monotonic clock and
counted from the run's start. A boundary is stamped when the client hands an
event to the gRPC library or reads one from it.

| Mode | Boundaries, in order |
| --- | --- |
| Speech-to-text | `call_start`, `ready`, `first_audio_sent`, `last_audio_scheduled`, `last_audio_sent`, `finalize_sent`, `endpoint`, `final_transcript`, `session_closed` |
| Text-to-speech | `call_start`, `ready`, `text_segment_scheduled`, `text_segment_sent`, `first_audio_chunk`, `segment_completed`, `finalize_sent`, `synthesis_completed`, `session_closed` |

`call_start` is taken immediately before the call is started. With the default
`--sessions 1`, connecting the channel happens once before the first session
and is recorded apart, as `target.connect_ns`. Concurrent sessions each own
a channel and record `connection.started_ns`, `connection.connect_ns` and
`connection.ended_ns`; these bracket the connection attempt through channel
cleanup, separately from call latency. The target's legacy `connect_ns` is
the first successful connection's duration in session-index order.
A send is stamped before the event is handed over, and a
received event when it is read, not when the client comes to handle it.
Each measurement is the difference of two boundaries and names the server
event that ended it:

| Measurement | From | To |
| --- | --- | --- |
| `session_initialization` | `call_start` | `ready` |
| `call_start_to_first_transcript` | `call_start` | the earliest nonblank partial or final transcript |
| `last_audio_to_final_transcript` | `last_audio_scheduled` | `final_transcript` |
| `call_start_to_first_audio` | `call_start` | `first_audio_chunk` |
| `segment_to_first_audio` | `text_segment_scheduled` | `first_audio_chunk` |
| `segment_to_completion` | `text_segment_scheduled` | `segment_completed` |

- A speech-to-text session sends one clip and then a client `Finalize`.
  On a voice-detecting deployment, pauses can yield several utterances in
  that same session: `EndpointDetected(SILENCE)` and
  `EndpointDetected(DURATION_LIMIT)` are valid before Finalize. Each
  utterance keeps its own endpoint, partials and final. A partial after
  its utterance's endpoint, a duplicate endpoint or final, or a final
  without its own endpoint stops the session as `unexpected_event`.
  The client cannot tell whether a deployment detects voice, so it does not
  reject a clip for exceeding `max_utterance_ms` before sending audio. On a
  deployment without voice detection that enforces this limit, an over-long
  clip now ends as the server's failure, recorded as `session_failed`.
- The session's existing `endpoint`, `final_transcript`, `server.endpoint`
  and `server.final` fields still describe the utterance ended by the
  client's Finalize. `last_audio_to_final_transcript` keeps its scheduled
  last-frame origin. `call_start_to_first_transcript` ends at the earliest
  nonblank partial or final across all utterances, including a server-ended
  utterance; its `to` field names that observation's path when it is outside
  the existing session boundaries. An empty Finalize response after an
  earlier transcript does not erase that first-transcript sample.
- A session with no nonblank partial or final counts as a miss for
  `call_start_to_first_transcript`. Empty and whitespace-only transcripts
  supply no sample for that measurement.
- The time to an answer is counted from when its input was due, not from
  when it went: `last_audio_scheduled` is the last frame's place in real
  time counted from the first frame, and `text_segment_scheduled` is the
  moment the session was ready for the segment. A server that holds back
  input credit therefore lengthens its own figure and cannot shorten it.
  `input_lag_ns` records how late that input was sent. Above 100 ms
  (`max_input_lag_ms` in the record's settings, a constant of the tool) the
  session gives no sample for the measurements that start there.
- Only a session that completed, and whose answers agree with what was
  sent, gives a sample to the summary. A session that failed, timed out or
  was stopped is a miss for every measurement, whatever boundaries it
  reached; its own timestamps stay in the record.
- `first_audio_chunk` is the first `AudioChunk` that holds whole samples in
  the format `Ready` reported. A server that synthesizes a whole segment
  before it sends the first chunk shows in `segment_to_first_audio` as
  that whole synthesis; the record does not tell the two cases apart.
- The serving stages (`ingress`, `queue`, `vad`, `preprocessing`, `backend`,
  `postprocessing`, `egress`, the registered values of the `stage` metric
  label) are named in every session and marked `not_measured`: a caller
  cannot time them, and the schema refuses a duration there.

Audio is sent in frames of `--frame-ms` (20 by default), each at its place
in real time counted from the first frame, whenever the frame before it
went. A frame or a text segment waits for input credit when the session has
none left, until an `Accepted` returns some; the record counts those waits
and gives the longest a frame was late. A `Ready` or an `Accepted` whose
input credit has no byte limit is not read as unlimited: the session stops
as `credit_missing`. The client sends `Ping` at the interval `Ready`
reports.

Each mode's summary gives, for every measurement, the count, the misses,
the minimum, the maximum and the 50th, 95th and 99th percentiles by nearest
rank on the sessions' raw values, all of which stay in the record.

## Per-utterance observations

New records use `schema_version: "0.3"`; the checker also accepts existing
`0.1` and `0.2` records. This is the measurement record's version, independent
of the stream protocol; the schema's `$id` keeps its v0.1 identifier.
Every STT session adds `utterances` and
`client.audio_frames`; TTS measurements and the existing session summaries
keep their fields and definitions.

Each utterance records its protocol `utterance_id`, `start_sample_offset`,
`endpoint` (reason and exclusive `end_sample_offset`), `final` (Finalize
sequence, end offset, segment count, text size and digest), and `partials`.
Each partial records `at_ns`, `revision`, stripped `text_bytes` and the
SHA-256 digest of the text as received. No transcript text is saved.

The utterance's input interval starts at zero for the first utterance and
at the previous endpoint's end offset thereafter. This is the wire input
interval, including silence; it does not claim to identify speech onset or
the server's trimmed decode window. `client.audio_frames` records each
frame's start and exclusive end sample offsets, `scheduled_ns` and
`sent_ns`. Offsets use the negotiated input sample rate, not wall time.
The first and last frames overlapping the utterance's half-open interval
supply its audio boundaries. An endpoint inside a frame assigns that frame
to both adjacent utterances. An empty interval (for example Finalize in
silence immediately after an endpoint) has no audio boundaries.

Each utterance has `boundaries`, `measurements` and `cadence`. Boundaries
are caller monotonic nanoseconds from run start; received events are
stamped when read, even if handled later. Every measurement carries
`from`, `to`, `ended_by` and `ns`:

| Measurement | From | To |
| --- | --- | --- |
| `endpoint_to_final` | this utterance's `endpoint` receipt | its `final_transcript` receipt |
| `last_audio_to_endpoint` | `last_audio_sent`: send time of the last frame overlapping this utterance's input interval | its `endpoint` receipt |
| `last_audio_to_final` | the same `last_audio_sent` | its `final_transcript` receipt |
| `first_audio_to_first_partial` | `first_audio_sent`: send time of the first overlapping frame | its `first_partial` receipt |

These per-utterance audio measurements use actual send times, unlike the
existing session measurement's scheduled origin. Frame schedule and send
times remain available to inspect credit delays. A delayed endpoint is
matched by its input offset, never by choosing the last frame sent before
its arrival. A missing boundary produces `ns: null`. An empty utterance
can still measure endpoint-to-final; a peer that emits only a final has no
first-partial sample. These missing optional observations do not add
misses to the existing session summaries or change the run's result.

`cadence.partial_count` counts every received partial, including empty or
whitespace hypotheses. Per-utterance `first_partial` also includes those
hypotheses; the session's first-transcript measurement still requires
nonblank text. `cadence.intervals_ns[i]` is the receipt time of
`partials[i + 1]` minus that of `partials[i]`. `cadence.revision_gaps[i]`
is the number of skipped revisions before `partials[i]`. The first gap
is `max(0, revision - 1)`, so an initial revision of either zero or one
has no gap; subsequent gaps are the revision difference minus one. For
revisions 1, 3, 4, the gaps are `[0, 1, 0]`: the server replaced one unsent
partial. Revision zero is valid, and revisions must increase within an
utterance. Counts, gaps, intervals and measurements are
recomputed by the checker, along with frame offsets and timestamps,
endpoint/final identity and the session's utterance totals. Observations
from failed or interrupted sessions remain diagnostic data, not successful
latency samples.

## What the record holds

Counts, sizes, digests and times, and a few values that identify no person
or machine. It has no field for audio, text, a transcript, a deployment id,
an address, a host name, a user name or a path: the input audio, the text,
the transcript and the audio received are recorded as SHA-256 digests and
sizes, and the endpoint only as whether it is a loopback address. Recorded
in the clear are:

- the language and the voice each `Open` named, which are values of the
  protocol;
- the target's generation and descriptor digest. A worker that reports
  neither is addressed by its deployment alone, and both are null. Such a
  record identifies no deployment by them; like every record of this tool
  its `record_kind` is `slice_measurement` and the schema admits no other.
  The `Open` of such a session carries a generation of zero and an empty
  descriptor digest, which the draft stream schema's comments say do not
  occur. That is the thin slice's case until the schema's next revision
  settles it, and a `Ready` that names a generation the `Open` did not, or
  omits one it did, stops the session as `ready_mismatch`;
- the worker's release and build as the operator stated them with
  `--worker-version` and `--worker-build`, marked `operator_stated`. The
  stream session envelope carries neither, so the tool cannot read them;
- the measuring machine's operating system, kernel release, architecture,
  CPU count and CPU model, and the model name of each GPU sampled;
- `provenance`: `recorded` only with `--source-commit`, the commit the tool
  was run from, and the worker's release or build. Without both the record
  is `synthetic`, and the check refuses a `recorded` record that lacks
  either.

Each session records how it ended:

| Outcome | Meaning |
| --- | --- |
| `completed` | every answer arrived, the session closed without a cause and the call ended `OK` |
| `session_failed` | the server ended the session as failed or with a cause; its reason and code are recorded |
| `call_failed` | the call ended without a `SessionClosed`, or with another status than `OK` after one |
| `timeout` | the next event the client needed did not arrive within `--event-timeout-ms`, counted again from each such event; a `Pong` is not one. The timeout is per event: a phase of a session has no overall bound |
| `stopped` | the client ended the call: a sequence gap, an event the session's state or mode does not allow, a `Ready` that names another generation, descriptor or input format, input credit without a byte limit, or input that fits neither the limits nor the whole input credit `Ready` reported |

A refused `Open` is a `call_failed` with no server event, read by its gRPC
status code alone. How the status carries the typed refusal is not fixed in
the stream schema yet, so `open_refusal` is always null.

The result is `failed` when the run did not end by itself, a session did
not complete or a completed session's answers contradict what was sent (a
`Finalize` answered under another sequence, totals that do not add up);
`incomplete` when a mode was not run or a measurement has a miss;
`complete` otherwise.

The record is written before the first session and again after each completed
attempt (successful or failed), each time as a whole file that replaces
the one before. A run that an
error or an interrupt ends writes it once more, with `run.ended_by` set to
`error` (and the error's type name) or `interrupt`; a run that was killed
leaves the last one, whose `ended_by` is null; SIGTERM is such a kill, the
tool does not catch it. The check reports each of them as `failed`.

## Concurrent sessions and aggregates

`--sessions N` requests 1 to 64 sessions per batch, for each requested mode;
the default is 1. Each concurrent session opens its own gRPC channel with
transport sharing disabled, against the same endpoint and resolved target.
Every STT session sends the same clip on its own real-time frame schedule;
every TTS session sends the same text segment. `--stagger-ms M` spaces the
start of connection attempts by at least M milliseconds (default 0).
Connection readiness and scheduling can delay the actual Open or audio.
`--iterations` repeats whole batches after every session in the previous
batch has ended. If both modes are requested, STT batches precede TTS batches,
as in the sequential tool; this does not measure mixed STT/TTS concurrency.
One GPU sampler covers the whole run.

Each mode's existing `sessions` array retains every finished attempt, sorted
by `index = iteration * N + slot`, even when completions arrive out of order.
A checkpoint can therefore have gaps in its indices; running attempts appear
when they end. A refusal, deadline or protocol violation affects only that
session. A connection that never becomes ready has a timeout entry with
`connection.connect_ns: null` and no events sent; its `call_start` holds the
attempt start, and all call latencies are null. An interrupt or unexpected
run error cancels outstanding concurrent calls and retains their completed
records. The CLI prints the clean and failed session counts.

The additive root `aggregate` contains:

- `sessions_recorded`, `sessions_opened` (Open attempts, including refusals),
  `sessions_clean`, and `sessions_failed`. Clean means a successful close,
  gRPC OK, and answers consistent with the sent input.
- `failed_by_cause`, a sorted array of `{cause, count}`. Causes distinguish
  `grpc:<STATUS>`, `session_failed:<reason>:<code>` (`none` for no code),
  `stopped:<stop_reason>`, `event_timeout`, `channel_timeout`, and
  `invalid_response`. Server detail strings are never recorded.
- `wall_time_ns`, from run start through session cleanup and sampler shutdown,
  equal to `run.finished_ns`; null in an unfinished checkpoint.
- `measurements.stt` and `measurements.tts`, each null for an unrequested mode.
  These include every existing session latency. STT also pools each of the
  four utterance latencies and every observed `partial_interval` from cadence.
  Each distribution contains `count`, `misses`, `p50_ns`, `p95_ns`, `p99_ns`,
  `min_ns` and `max_ns`. Failed sessions retain diagnostic timestamps but
  contribute misses, not latency samples. Missing utterance boundaries also
  contribute misses; no interval is invented for fewer than two partials.

Percentiles use **nearest rank**: sort the raw non-null samples and select
the sample of rank `ceil(p * count / 100)`, counting from 1. They are pooled
across sessions and utterances, never computed by averaging session
percentiles. With no samples, percentiles, minimum and maximum are null.
The checker recomputes aggregates, causes and counts and verifies connection,
batch, stagger and run timing against the observations.
The validation index retains nondefault concurrency, stagger and qualification
counts as settings; comparisons report differences as notes under the
existing comparison policy.

`--sessions 1` preserves the sequential observation path, session fields and
summary meanings. A reader that ignores additive fields can keep reading
the existing per-mode `sessions` and `summary`. A strict 0.2 schema reader
needs the new schema, or a 0.2 projection of a single-session configuration:
remove root `aggregate`, `run.finished_ns`, and `settings.sessions`,
`settings.stagger_ms`, `settings.expect_test_count`, then set `schema_version`
to `"0.2"`. Existing 0.1 and 0.2 records remain valid without changes.

Before a qualification run, the operator deploys with
`tensorplate deploy --qualification --test-count N` and uses
`tensorplate status` to inspect `admission=qualification` and `test_count=N`.
Pass `--sessions N --expect-test-count N` to the measurement tool.
`--expect-test-count` is optional, accepts 1 to 2048, and records only the
operator-supplied value beside `settings.sessions`; it queries no agent and
verifies no deployment configuration. It may differ from the session count
when probing refusals. The supported count and latency remain unqualified
until the operator's hardware measurements establish them.

## GPU readings

The readings are of the measuring machine. With `--gpu-sampling auto` (the
default) they are taken only when every target is a loopback address, where
the measuring machine is the one that serves; the record then marks them
`serving`. Against a target elsewhere nothing is sampled unless
`--gpu-sampling on` asks for it, and the record marks those readings
`client_only`: they say nothing of the device that serves. The check
refuses a record whose mark does not agree with its targets.
`--gpu-sampling off` measures without them.

Loopback is decided from the endpoint as it is written: the name
`localhost`, or an IP literal in a loopback range. The tool cannot see
where the connection ends, so two cases need the option spelled out:

- A port forwarded from another machine to `127.0.0.1` reads as loopback,
  and its samples would be marked `serving` though they are this
  machine's. Pass `--gpu-sampling off`.
- A unix socket (`unix:<path>`) reads as not loopback, though the worker is
  on this machine. Nothing is sampled by default; `--gpu-sampling on`
  samples, and the record marks the samples `client_only`.

Where sampling runs and the machine has `nvidia-smi`, the run records
device-wide utilization and memory once per `--gpu-interval-ms` (1,000 by
default), the first reading before the first session. The readings describe
the whole device, never one model or one session. They are taken by a
separate process, started before the run opens a channel, so the measuring
process starts nothing while it measures. A later query that fails or
cannot be read is counted in `failed_queries`; a query still running when
the run ends is ended with it. The record says whether sampling ran
(`sampled`, `unavailable` with a reason, or `not_requested`), and it says
`sampled` only when the sampling process was still running at the end.

## Running it

The tool needs Python 3.10 or later, `grpcio` and `protobuf` at the versions
the SDK's speech extra pins, and the `grpcio-tools` release of the same
number as `grpcio`; it refuses to start when the two differ. The SDK's
build pins hold that generator by hash. From a checkout:

```bash
python3 -m venv /var/tmp/slice-measure
/var/tmp/slice-measure/bin/pip install --require-hashes -r sdk/python/constraints/speech.txt
/var/tmp/slice-measure/bin/pip install --require-hashes -r sdk/python/constraints/build.txt
```

Give either mode or both. The endpoint, generation and descriptor digest
are those of the served deployment; the tool does not discover them. For a
worker that reports no generation and no descriptor digest, give the target
as `<deployment>` alone. The audio is a clip of mono 16-bit PCM in a
WAV file; it may contain pauses and exceed one utterance's duration limit
when the server endpoints it. The text file holds one segment of 1 to 4,096 bytes,
surrounding whitespace dropped.

```bash
/var/tmp/slice-measure/bin/python tools/validation/slice_measure.py run \
  --out /var/tmp/slice-record.json --iterations 20 \
  --source-commit "$(git rev-parse HEAD)" --worker-version <release> \
  --stt-endpoint 127.0.0.1:<port> --stt-target <deployment>:<generation>:<digest> \
  --stt-language <language> --audio <utterance.wav> \
  --tts-endpoint 127.0.0.1:<port> --tts-target <deployment>:<generation>:<digest> \
  --tts-language <language> --voice <voice> --text-file <segment.txt>
```

`run` writes the record, checks it and exits with the check's status. To
check a record again:

```bash
python3 tools/validation/slice_measurement_record.py check /var/tmp/slice-record.json
```

Both exit 0 for `complete`, 1 for `failed`, and 2 when the record is
`incomplete`, cannot be trusted, or could not be written. The check needs no
gRPC package. It recomputes every measurement, summary and the result from
the timestamps and counts and refuses a record whose own fields disagree
with them, as well as one whose timestamps are out of order, whose audio
was sent faster than real time, whose counts say less arrived than its
other fields record (a first partial with no partial counted, audio with
no chunk, a completed session with no event), or whose stage list is not
the registered one.

`test/validation/slice_measure_test.py` runs the client against a server
built in the test from the same generated bindings. That shows the client
speaks the envelope as the schema states it; it does not show how a serving
worker answers.
