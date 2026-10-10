# Streaming slice measurement

`tools/validation/slice_measure.py` measures the streaming path from the
caller's side: paced speech-to-text sessions and text-to-speech segments
over the stream session envelope
([`protocol/README.md`](../../protocol/README.md#streaming-session-envelope)),
one after another, with a timestamp at every boundary a caller can see. It
writes one record, and `tools/validation/slice_measurement_record.py` checks
it. The record's shape is
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

`call_start` is taken immediately before the call is started; connecting the
channel happens once before the first session and is recorded apart, as
`connect_ns`. A send is stamped before the event is handed over, and a
received event when it is read, not when the client comes to handle it.
Each measurement is the difference of two boundaries and names the server
event that ended it:

| Measurement | From | To |
| --- | --- | --- |
| `session_initialization` | `call_start` | `ready` |
| `call_start_to_first_transcript` | `call_start` | the first partial transcript that holds more than whitespace, or the final transcript when none arrived |
| `last_audio_to_final_transcript` | `last_audio_scheduled` | `final_transcript` |
| `call_start_to_first_audio` | `call_start` | `first_audio_chunk` |
| `segment_to_first_audio` | `text_segment_scheduled` | `first_audio_chunk` |
| `segment_to_completion` | `text_segment_scheduled` | `segment_completed` |

- A speech-to-text session sends one utterance and ends it with a client
  `Finalize`; it waits for no silence. An endpoint that arrives before the
  `Finalize` was sent ends the session as `unexpected_event`, because the
  input is then not one utterance.
- A final transcript of nothing but whitespace is not a sample of
  `call_start_to_first_transcript`; the session counts as a miss for it. A
  partial transcript of nothing but whitespace is not a first transcript.
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
  its `record_kind` is `slice_measurement` and the schema admits no other;
- the worker's release and build as the operator stated them with
  `--worker-version` and `--worker-build`, marked `operator_stated`. The
  stream session envelope carries neither, so the tool cannot read them;
- the measuring machine's operating system, kernel release, architecture,
  CPU count and CPU model, and the model name of each GPU sampled;
- `provenance`: `recorded` only with `--source-commit`, the commit the tool
  was run from. Without one the record is `synthetic`, and the check
  refuses a `recorded` record that names no commit.

Each session records how it ended:

| Outcome | Meaning |
| --- | --- |
| `completed` | every answer arrived, the session closed without a cause and the call ended `OK` |
| `session_failed` | the server ended the session as failed or with a cause; its reason and code are recorded |
| `call_failed` | the call ended without a `SessionClosed`, or with another status than `OK` after one |
| `timeout` | the next event the client needed did not arrive within `--event-timeout-ms`, counted again from each such event; a `Pong` is not one |
| `stopped` | the client ended the call: a sequence gap, an event the session's state or mode does not allow, a `Ready` that names another generation, descriptor or input format, input credit without a byte limit, or input that fits neither the limits nor the whole input credit `Ready` reported |

A refused `Open` is a `call_failed` with no server event, read by its gRPC
status code alone. How the status carries the typed refusal is not fixed in
the stream schema yet, so `open_refusal` is always null.

The result is `failed` when the run did not end by itself, a session did
not complete or a completed session's answers contradict what was sent (a
`Finalize` answered under another sequence, totals that do not add up);
`incomplete` when a mode was not run or a measurement has a miss;
`complete` otherwise.

The record is written before the first session and again after each one,
each time as a whole file that replaces the one before. A run that an
error or an interrupt ends writes it once more, with `run.ended_by` set to
`error` (and the error's type name) or `interrupt`; a run that was killed
leaves the last one, whose `ended_by` is null. The check reports each of
them as `failed`.

## GPU readings

The readings are of the measuring machine. With `--gpu-sampling auto` (the
default) they are taken only when every target is a loopback address, where
the measuring machine is the one that serves; the record then marks them
`serving`. Against a target elsewhere nothing is sampled unless
`--gpu-sampling on` asks for it, and the record marks those readings
`client_only`: they say nothing of the device that serves. The check
refuses a record whose mark does not agree with its targets.
`--gpu-sampling off` measures without them.

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
number as `grpcio`; it refuses to start when the two differ. From a
checkout:

```bash
python3 -m venv /var/tmp/slice-measure
/var/tmp/slice-measure/bin/pip install --require-hashes -r sdk/python/constraints/speech.txt
/var/tmp/slice-measure/bin/pip install \
  "grpcio-tools==$(sed -n 's/^grpcio==\([^ ]*\).*/\1/p' sdk/python/constraints/speech.txt)"
```

Give either mode or both. The endpoint, generation and descriptor digest
are those of the served deployment; the tool does not discover them. For a
worker that reports no generation and no descriptor digest, give the target
as `<deployment>` alone. The audio is one utterance as mono 16-bit PCM in a
WAV file, and the text file holds one segment of 1 to 4,096 bytes,
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
