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
| Speech-to-text | `call_start`, `ready`, `first_audio_sent`, `last_audio_sent`, `finalize_sent`, `endpoint`, `final_transcript`, `session_closed` |
| Text-to-speech | `call_start`, `ready`, `text_segment_sent`, `first_audio_chunk`, `segment_completed`, `finalize_sent`, `synthesis_completed`, `session_closed` |

`call_start` is taken immediately before the call is started; connecting the
channel happens once before the first session and is recorded apart, as
`connect_ns`. Each measurement is the difference of two boundaries and names
the server event that ended it:

| Measurement | From | To |
| --- | --- | --- |
| `session_initialization` | `call_start` | `ready` |
| `call_start_to_first_transcript` | `call_start` | the first nonempty partial transcript, or the final transcript when no partial arrived |
| `last_audio_to_final_transcript` | `last_audio_sent` | `final_transcript` |
| `call_start_to_first_audio` | `call_start` | `first_audio_chunk` |
| `segment_to_first_audio` | `text_segment_sent` | `first_audio_chunk` |
| `segment_to_completion` | `text_segment_sent` | `segment_completed` |

- A speech-to-text session sends one utterance and ends it with a client
  `Finalize`; it waits for no silence. An endpoint that arrives before the
  `Finalize` was sent ends the session as `unexpected_event`, because the
  input is then not one utterance.
- An empty final transcript is not a sample of
  `call_start_to_first_transcript`; the session counts as a miss for it.
- `first_audio_chunk` is the first `AudioChunk` that holds whole samples in
  the format `Ready` reported. A server that synthesizes a whole segment
  before it sends the first chunk shows in `segment_to_first_audio` as
  that whole synthesis; the record does not tell the two cases apart.
- The serving stages (`ingress`, `queue`, `vad`, `preprocessing`, `backend`,
  `postprocessing`, `egress`, the registered values of the `stage` metric
  label) are named in every session and marked `not_measured`: a caller
  cannot time them, and the schema refuses a duration there.

Audio is sent in frames of `--frame-ms` (20 by default), each at its place
in real time counted from the first frame. A frame or a text segment waits
for input credit when the session has none left, until an `Accepted`
returns some; the record counts those waits and gives the longest a frame
was late. The client sends `Ping` at the interval `Ready` reports. Each mode's summary gives, for every measurement, the count, the
misses, the minimum, the maximum and the 50th, 95th and 99th percentiles by
nearest rank on the sessions' raw values, all of which stay in the record.

## What the record holds

Counts, sizes, digests and times only. It has no field for audio, text, a
transcript, a voice name, a language tag, a deployment id or an address:
the input audio, the text, the language, the voice, the transcript and the
audio received are recorded as SHA-256 digests and sizes, the target by its
generation and descriptor digest, and the endpoint only as whether it is a
loopback address.

Each session records how it ended:

| Outcome | Meaning |
| --- | --- |
| `completed` | every answer arrived, the session closed without a cause and the call ended `OK` |
| `session_failed` | the server ended the session as failed or with a cause; its reason and code are recorded |
| `call_failed` | the call ended without a `SessionClosed`, or with another status than `OK` after one |
| `timeout` | an event the client needed did not arrive within `--event-timeout-ms` |
| `stopped` | the client ended the call: a sequence gap, an event the session's state does not allow, a `Ready` that names another generation, descriptor or input format, or input that fits neither the limits nor the whole input credit `Ready` reported |

A refused `Open` is a `call_failed` with no server event, read by its gRPC
status code alone. How the status carries the typed refusal is not fixed in
the stream schema yet, so `open_refusal` is always null.

The result is `failed` when a session did not complete or a completed
session's answers contradict what was sent (a `Finalize` answered under
another sequence, totals that do not add up); `incomplete` when a mode was
not run or a measurement has a miss; `complete` otherwise.

## GPU readings

With `--gpu-sampling auto` (the default) and `nvidia-smi` on the machine,
the run records device-wide utilization and memory once per
`--gpu-interval-ms` (1,000 by default), the first reading before the first
session. The readings describe the whole device, never one model or one
session. They are taken by a separate process, started before the run opens
a channel, so the measuring process starts nothing while it measures. The
record says whether sampling ran (`sampled`, `unavailable` with a reason, or
`not_requested`); pass `--gpu-sampling off` to measure without it.

## Running it

The tool needs `grpcio` and `protobuf` at the versions the SDK's speech
extra pins, and the `grpcio-tools` release of the same number as `grpcio`;
it refuses to start when the two differ. From a checkout:

```bash
python3 -m venv /var/tmp/slice-measure
/var/tmp/slice-measure/bin/pip install --require-hashes -r sdk/python/constraints/speech.txt
/var/tmp/slice-measure/bin/pip install \
  "grpcio-tools==$(sed -n 's/^grpcio==\([^ ]*\).*/\1/p' sdk/python/constraints/speech.txt)"
```

Give either mode or both. The endpoint, generation and descriptor digest
are those of the served deployment; the tool does not discover them. The
audio is one utterance as mono 16-bit PCM in a WAV file, and the text file
holds one segment of 1 to 4,096 bytes, surrounding whitespace dropped.

```bash
/var/tmp/slice-measure/bin/python tools/validation/slice_measure.py run \
  --out /var/tmp/slice-record.json --iterations 20 \
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
was sent faster than real time, or whose stage list is not the registered
one.

`test/validation/slice_measure_test.py` runs the client against a server
built in the test from the same generated bindings. That shows the client
speaks the envelope as the schema states it; it does not show how a serving
worker answers.
