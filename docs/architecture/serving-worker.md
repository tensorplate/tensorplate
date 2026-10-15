# Serving worker

This document is the authoritative description of the v0.1.0
`tensorplate-serving` process. It is referenced from
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) and tested by the V01-E07
test tree.

## Process responsibility

`tensorplate-serving` is the data-plane process. It does **not** do
desired-state management, deploy transactions, or worker
supervision; the agent (V01-E08) owns those concerns and starts the
worker with a validated config. The worker:

1. Loads its config, validates it, and wires up runtime components
   in one explicit composition root (V01-E07-F01). A member of a
   deployment generation first takes the agent's control socket from
   fd 0 (see "Control channel").
2. Opens a loopback-only HTTP/1.1 listener (V01-E07-F02).
3. Routes `/infer`, `/policy/infer`, `/policy/result/<id>`,
   `/policy/cancel/<id>`, `/health`, and `/metrics` requests
   (V01-E07-F02..F06).
4. Connects each accepted request to the V01-E06 scheduler, the
   V01-E04 execution session, the V01-E03 buffer plane, and the
   V01-E05 adapter through their public interfaces only
   (V01-E07-F05).
5. Publishes serving state, scheduler accounting, buffer accounting,
   and latency histograms locally (V01-E07-F06).
6. Drains and exits deterministically on SIGTERM / SIGINT or on an
   agent-driven shutdown call (V01-E07-F07).

The worker is loopback-only by default. Setting `bind.allow_non_loopback`
in the config is rejected by the validator unless the test-only
environment variable `TP_E2E_ALLOW_NON_LOOPBACK=1` is set. Production
deployments rely on the agent for any remote exposure decisions.

## Composition root

The optional `TP_ENABLE_STREAMING_GRPC` build feature links the pinned static
gRPC/protobuf dependencies privately through `tp_runtime`. Configure defaults
it ON when the pinned vcpkg install tree provides the static libraries and
the two code generators, and OFF otherwise; an explicit ON fails without the
vcpkg feature, with shared libraries or with generators from elsewhere. The
checks and the build setup are documented in
[`cmake/features/README.md`](../../cmake/features/README.md).

`tensorplate-serving --version` prints, after the runtime, protocol and
bundle-format lines, `streaming-grpc on` or `streaming-grpc off`, from the
same CMake option the runtime's config check is compiled with.

`streaming.enabled` in the worker config defaults to false. A true value in
a feature-OFF build returns `unsupported` before runtime construction, and so
does a true value in a worker with `deployment.generation`: `Ready` would have
to name the descriptor that generation serves, which the worker is not given.
With `streaming.enabled`, `deployment.endpoint` must be a deployment id (1 to
128 of letters, digits, `.`, `_` and `-`). `streaming.port` (0 to 65535, 0 for
an ephemeral port) is the stream listener's port, which must differ from
`bind.port`; its host is `bind.host`, so the HTTP bind's loopback rule covers
it. Null, non-boolean or out-of-range
values and unknown keys in `streaming` return `config_invalid`. A config with
neither field set serializes without the optional block, preserving the
existing agent-rendered config shape; the agent renders no `streaming` block,
so a worker with streaming enabled is started by hand from a config. A
feature-OFF build compiles no stream listener and links no gRPC code.

### Stream listener

In a feature-ON build with `streaming.enabled`, `start()` opens the gRPC
`SessionService` of the draft stream schema
(`protocol/proto/tensorplate/stream/v1/session.proto`) on
`bind.host`:`streaming.port`, after the HTTP listener, and logs the bound port
as `stream listener bound`. The listener (`runtime/src/serving/stream/`, all
internal) owns a `SessionManager` that admits one session at a time with the
protocol's default timers, and drives the [session
dispatch](#session-dispatch) it is given. For a speech deployment on a
backend that builds its session with a job bridge the worker gives it the
speech dispatch
([The speech dispatch in the worker](#the-speech-dispatch-in-the-worker)) and
the log line says `serves_sessions: true`. For any other deployment it gives
none: every `Open` is refused with `backend_unsupported_capability`, and the
line says `serves_sessions: false`.

- **API and threads.** The listener uses gRPC's callback API: no thread per
  call. gRPC's own threads run each call's reactions, which apply one client
  event to the manager and never wait on a job, plus the manager's timer
  thread and one thread of the listener's own, which refuses calls whose
  `Open` is overdue (see "Calls waiting to open") and cancels calls that
  outlive their session (see "Calls that outlive their session"). gRPC
  sizes its pool by the machine's cores. The method is registered raw:
  gRPC hands the listener each message as bytes, and the listener parses
  client events and serializes server events itself, with the calls
  gRPC's typed path makes. A message that does not parse therefore reaches
  the call's reader instead of failing its read.
- **Write path.** Each call writes through a small writer interface
  (`stream_output.hpp`), so the path is tested without a transport. The
  queue's consumer captures the path, never the call, and starts the next
  write when none is in flight. A write that completes is confirmed to the
  queue and told to the dispatch before the next write can start, so
  deliveries are confirmed in the order written. Task output is encoded
  when the path takes it from the queue. A body the listener cannot encode
  (the dispatch contract lets `TaskOutput` grow) ends the call `INTERNAL`:
  no result or audio is dropped silently.
- **Target.** The worker serves no generation and no descriptor digest, so an
  `Open` must name the worker's `deployment.endpoint` with generation zero and
  an empty digest; `Ready` names neither. A selector is refused as
  `target_unresolved`, another deployment or any digest as `target_mismatch`,
  any generation as `stale_generation`. Internally the manager binds sessions
  to a fixed non-zero generation that never reaches the wire, because the
  session layer refuses generation zero.
- **Calls waiting to open.** The listener waits on a call from the moment
  gRPC hands it over until the call's first message has been read: on at
  most 32 calls at one time, and on each for at most the open handshake
  timeout, 5 s on the listener's clock. A call that arrives while 32 are
  waited on is refused when it arrives with `session_count_limit`, and
  nothing of it is read; the reason covers both limits, the sessions a
  worker serves and the calls it waits on. A call whose first message has
  not been read when the timeout passes is refused with `idle_timeout`:
  before `Open`, the handshake timeout is the bound on a client's silence.
  Both are refusals of a call that was never admitted (see "Refusals").
  Once a call's first message has been read it is no longer waited on and
  the timeout no longer applies: the answer, `Ready` or a refusal, is made
  in the reaction that read it, which waits on nothing. The limit therefore
  counts calls whose first message has not arrived, not an `Open` that is
  being answered. The listener's own thread checks the timeout. It sleeps
  until the instant the earliest wait ends, which it takes from the
  listener's clock as an instant of the steady clock; nothing is polled.
  A clock that is moved by hand, as a test's is, tells the listener that
  it has moved (`notify_clock_advanced()`). Two tests run on the system
  clock and wait out the five seconds, one for a call waiting to open and
  one for a call whose session has ended (see "Calls that end"), so that
  the thread waking by itself is held too. At any instant at most 32 calls are waited on, and none for
  longer than the timeout, so clients that send nothing whole hold at most
  32 such calls however many they open, and a call beyond them is refused
  on arrival. Each call waited on costs a reactor, no session, and what
  the transport holds of a first message that is still arriving, up to
  the 64 KiB receive limit (see "The server's window"). A
  call is waited on only once its first read has started, and a call
  refused on arrival starts none, so gRPC widens no window for it. When
  admission closes, every call waited on
  is refused at once with `admission_closed`, and so is every call that
  arrives later, without its first message being read; a malformed `Open`
  that arrives at a worker that is shutting down is therefore answered
  `admission_closed`, not `invalid_event`. None of the three refusals
  here started anything: see "Refusals" for what that means for a retry.
- **Refused calls and the calls of a connection.** A refused call is not
  over when it is refused: it lasts, with its reactor and what gRPC keeps
  of the call, until gRPC reports it done, which follows the writing of
  its status, and a peer that does not read can put that off. Two bounds
  hold what such calls can add up to.
  The first is gRPC's, on a connection: the server announces at most 64
  concurrent streams (`grpc.max_concurrent_streams`; a test reads the
  value off the server's first SETTINGS frame), and gRPC refuses a stream
  beyond them before the listener has a call for it. A client with fewer
  than 64 calls open on a channel sees nothing of it; with more, its own
  channel queues the surplus until a call ends. The value is above the 32
  calls waited on, so a connection that opens a 33rd silent call is still
  told `session_count_limit`. This is the bound on one connection. The
  second is the listener's, over all connections, and it is a bound on
  memory: the listener keeps at most 4,096 refused calls that are not
  done. A call takes a place when it is refused, by a bound or for its
  `Open`, and gives it back when gRPC reports the call done, however it
  ended. It holds the place for that whole time whether or not its peer
  reads, so a channel with 64 calls open at once can hold 64 places at
  one time. A refused call that is not done was measured at about 23.6 KB
  (the review of this change, before either bound existed: one connection
  that did not read held 180,822 refused calls and 4.26 GB), so the 4,096
  places hold roughly 100 MB. A call refused while all 4,096 places are
  taken is cancelled instead of finished with its status: it takes no
  place, and its client reads `CANCELLED` and no reason, which is less
  than "Refusals" promises a refused call. A client that reads its calls
  sees that only while 4,096 refused calls are outstanding over the whole
  listener at one time. The bound was 32 at first, below the 64 streams of
  one connection, and a single channel with 64 calls open then read bare
  cancels. What the two bounds leave open: the number of
  connections is not limited, so the 64 calls bound each connection and
  not their sum (the 4,096 places do bound the refused calls of all of
  them together); a refused call has no time limit, so a peer that does
  not read keeps the places it took for as long as its connection lasts;
  and what gRPC itself keeps for a call that was cancelled, or for a
  stream it refused, is gRPC's to bound. What ends such a connection is
  not the listener's: gRPC sets the TCP user timeout on a server's
  listening socket, 20 s unless `grpc.keepalive_timeout_ms` says
  otherwise, and the listener sets neither; by that option's description
  the kernel ends a connection whose peer has acknowledged or taken no
  bytes for that long. That is read from the source of the gRPC this tree
  builds (1.81.1), not shown by a run here. The tests: against a writer
  whose calls end when the test says so, the 4,096th refused call keeps
  its status, the next is cancelled, and the place of a call that ended is
  used again; one thread ends a refused call while another refuses one,
  which the thread sanitizer leg holds to the places' lock; with a real
  client, 64 calls opened at once on one channel, twenty rounds without a
  pause, each read a refusal with its reason; a listener that is given one
  place refuses calls one after another and each reads its reason; and a
  listener that is given no place cancels a call its watch thread refuses,
  a call refused when admission closes and a call refused on arrival. No
  test has a real peer that does not read.
- **Compression.** The server enables no compression algorithm. A call
  whose client announces one for its messages (gzip or deflate) is ended
  by gRPC itself, with status `UNIMPLEMENTED` and no event: the listener
  is handed no message of it, so its `Open` opens no session. The status
  is the transport's and names no failure reason, and gRPC writes one
  error line of its own to the worker's stderr for each such call.
- **What it serves.** From the client: `Open` (answered by `Ready`),
  `Audio`, `TextSegment`, `Finalize`, `Cancel`, `Ping` (answered by `Pong`),
  `StatusRequest` (answered by `SessionStatus`) and a half-close, which
  drains and closes the session. To the client: `Ready`, `Accepted`,
  `SessionStatus`, `Pong`, `CancelAccepted`, `EndpointDetected`,
  `FinalTranscript`, `AudioChunk`, `SegmentCompleted`, `SynthesisCompleted`
  and `SessionClosed`. No client event of the schema is refused as
  unsupported. A message that does not parse as a client event (a
  truncated one, or a `TextSegment` whose text is not UTF-8), a body the
  schema does not define, a second `Open`, or an event out of sequence or
  naming another session fails the session with `invalid_event`, and
  nothing behind it is read. No `PartialTranscript` is sent: the dispatch
  contract has no partial result yet. `Ready` reports the deployment's
  limits and the session's initial credit.
- **Input.** The listener checks each `Audio` and `TextSegment` before it
  offers it to the manager, against the terms the dispatch granted and
  what the session accepted before. A check that fails fails the session
  with `invalid_event`, and the cause's `detail` names the check:
  `wrong_input_kind` (audio in a text session, or the reverse),
  `frame_size_mismatch` (the data is not `sample_count` samples of the
  input format), `frame_too_long`, `sample_offset_mismatch` (the offset is
  not the number of samples accepted so far), `audio_after_short_frame`,
  `segment_id_out_of_range`, `segment_id_not_increasing`,
  `segment_too_long`, `synthesis_text_limit_exceeded` (with the segments
  accepted since the last Finalize the session started on, the text
  exceeds the limit) and `turn_id_malformed`. An `Audio` without data, and
  a segment whose text is empty or only ASCII whitespace, fail it with
  `empty_input`. A frame shorter than the smallest granted is accepted, as
  the last before a Finalize or a half-close: audio after it fails the
  session, also when an automatic endpoint came between, and only the
  client's own Finalize makes audio valid again. Accepted input is handed
  to the dispatch once the manager's call has returned, one item at a time
  in the order accepted. Input that a drain does not take is not handed
  over, and from the first input or Finalize a drain does not take the
  listener checks no more input for the rest of the call: the history its
  checks compare against stops there, while a client cannot know of a
  drain the worker started and goes on sending.
- **Credit and totals.** `Accepted` and the status reply are built in the
  manager's sink from the transition they belong to, never from the result
  of a call, because only the sink's statuses are in order (see [Logical
  sessions](#logical-sessions)). Each accepted input queues an `Accepted`
  with its sequence and the credit as it then stood. Each return of credit
  queues one under the sequence of the `Accepted` before, while the
  session is `active` or `finalizing` and not otherwise: in any other
  state no client can use credit. `Accepted`, `Pong` and `SessionStatus`
  each replace their own unsent instance in the queue, whose newest says
  everything, so a stream that is not read holds one of each and replies
  do not use up the reserve kept for lifecycle messages. `SessionClosed`
  names the last input accepted and carries the session's totals: the
  input as the sink counted it on acceptance, and the output as the write
  path took it, which is what was written on that stream before it.
- **Finalize.** The listener hands each Finalize the manager started on to
  the dispatch with that event's own sequence, and writes the
  `finalize_sequence` the dispatch puts on a final unchanged. The
  hand-over follows the result of the listener's own call, so the
  `start_finalize` that an automatic endpoint shows the sink is never
  handed over. The listener keys on nothing while a Finalize is
  unanswered: following a Finalize to the final that carries its sequence
  is the client's. While the session is `finalizing` for a Finalize, the
  manager refuses input (`illegal_transition`) and ignores a repeated
  Finalize. The dispatch may complete the finalization shortly before it
  queues the answer; input or a Finalize read in that window belongs to
  the next utterance or synthesis. A client that sends nothing between a
  Finalize and its answer meets neither case.
- **Cancel.** A `Cancel` of a live session is answered by `CancelAccepted`.
  Output stops at once, and `SessionClosed` follows with
  `client_cancelled` and status `CANCELLED` when the dispatch has released
  the session.
- **Calls that end.** A read that fails is applied as a half-close, except
  the first read of a call that a bound has refused meanwhile, which is
  dropped (see "Calls waiting to open"). A read
  also fails when the call was cancelled or its transport lost, and gRPC
  reports that cancellation in a reaction of its own, before or after the
  failed read: a vanished client's session may have begun to drain when
  the cancellation aborts it (`client_cancelled`). A read fails, too, once
  the listener has ended a call itself for output it cannot send. A
  session still live when its call is done is failed (`internal`), so that
  it does not stay draining with no stream to deliver to. A message that
  does not parse is not a failed read: the listener parses each message
  itself and fails the session (see "What it serves").
- **Transport window.** A delivery is a completed write, and gRPC completes
  a write when HTTP/2 flow control lets the bytes pass, not when the
  client's application reads them. The no-progress limit and the output
  budgets therefore trail a reader that has stopped by the receive window
  its own channel announced, and no server setting bounds that window.
  Listener tests hold both halves with a text session whose client reads
  nothing. With `grpc.http2.bdp_probe=0` the client keeps the HTTP/2
  default window of 65,535 bytes: chunk writes complete until that window
  is full to within two chunks and no further, and the session then ends
  `slow_consumer` when the 5 s limit passes on an injected clock. A default
  client channel widens its window with its bandwidth probe, and more than
  the default window passes unread; how much is the client's choice and is
  not asserted. A client that wants the limit to follow its reading turns
  the probe off.
- **The server's window.** The listener turns gRPC's bandwidth probe off
  on its own side (`grpc.http2.bdp_probe=0`). With the probe, the pinned
  gRPC announced a window of 4 MiB for each stream and for the connection
  in the first frames of a connection; without it a connection starts
  with the HTTP/2 default of 65,535 bytes for both, and a test reads that
  off the first frames of a raw connection. Those are the windows of a
  call the listener is not reading. They are not a bound on a connection:
  while a read is outstanding, gRPC widens the stream's window and the
  connection's by the length of the message it is receiving, so each call
  with a read outstanding can hold one unfinished message of up to the
  64 KiB receive limit in the transport, on top of its window. Two
  measurements made by hand, which no test holds, show both halves. With
  a call's reader held, a gRPC client completed 16 writes of 8,000-byte
  messages (128,224 bytes: one window in the server's transport, the rest
  in the client's own write buffer), against 523 (4,191,719 bytes) with
  the probe on. And 32 calls on one connection, each waited on and each
  sent 60,000 bytes of a 65,000-byte message by a raw HTTP/2 client, were
  all taken (1,920,000 bytes), the server widening each stream by 65,005
  bytes and the connection by the sum; for a 33rd, refused on arrival,
  it widened neither. The number of connections is not bounded.
- **Calls that outlive their session.** A write in flight completes only
  when the peer takes it, the session's `SessionClosed` waits behind it,
  and the call is finished only after that event is written. A reader that
  stops would therefore hold its call, though no longer a session slot,
  for as long as it liked. Once a session has ended its call is given the
  same 5 s no-progress limit again, counted from the end and then from
  each write that completes; a call that outruns it is cancelled through
  its server context, which fails the write in flight and ends the call
  with status `CANCELLED` and no `SessionClosed`. So a client that stopped
  reading for the limit, lost its session to `slow_consumer` and reads
  again within the next 5 s still receives the outcome and status
  `RESOURCE_EXHAUSTED`; one that never reads again has its call ended
  without it. The restarts are bounded: nothing is queued behind a
  session's `SessionClosed`, so the limit restarts at most once for each
  write that was still to complete when the session ended, and the call is
  over, or cancelled, within one limit for each of those writes and one
  more for its status. The rule does not depend on why the session ended:
  a session that failed behind a write that never completes is covered
  too. A finished call is watched until its reactor is done, since the
  transport may be unable to send the status either. The listener's thread
  sleeps until the earliest such limit, as it does for the calls waiting
  to open. A cancel is sent at most once per call, and never
  after the reactor is done: the write path holds a lock through the
  cancel that the reactor's last reaction takes before the reactor is
  destroyed.
- **Refusals.** A refused `Open` creates no session: the call ends with no
  event and a status whose code follows the reason (`invalid_event` is
  `INVALID_ARGUMENT`; unsupported, target and generation reasons are
  `FAILED_PRECONDITION`; `session_count_limit` is `RESOURCE_EXHAUSTED`;
  `admission_closed` is `UNAVAILABLE`; `idle_timeout` is
  `DEADLINE_EXCEEDED`) and whose message names the reason.
  A first message that does not parse is refused as `invalid_event`. A
  call refused by one of the bounds under "Calls waiting to open" ends the
  same way, unless no place is left for it, when it reads `CANCELLED` (see
  "Refused calls and the calls of a connection"). A refused call starts no
  read: one refused on arrival never
  had one, and one that a bound refuses later has only its first read
  outstanding, whose answer the reader drops. A refusal is of a call that
  was never admitted (`OpenRefused` with admission `NOT_ADMITTED` in the
  schema): nothing was started for it, so the same `Open` is safe to send
  again on a new call, whatever the failure-reason taxonomy's `Retryable`
  column says of the reason. That column describes a reason as the end of
  a session, which is why it lists `idle_timeout` as not retryable.
  How the status will carry the `OpenRefused` detail is not decided; one
  function builds the status, and it attaches no detail yet.
- **Final status.** A session's typed outcome is its `SessionClosed`. The
  call then ends with status OK only when the session closed without a
  cause (the client half-closed and it drained). With a cause the status
  code follows the cause's reason: invalid events and input are
  `INVALID_ARGUMENT`; unsupported, target, generation and illegal-transition
  reasons `FAILED_PRECONDITION`; count, credit, slow-consumer and
  out-of-memory reasons `RESOURCE_EXHAUSTED`; timeouts and deadlines
  `DEADLINE_EXCEEDED`; shutdown, retirement and backend or sidecar loss
  `UNAVAILABLE`; `client_cancelled` `CANCELLED`. A reason outside that list
  takes the class of the error code the failure-reason taxonomy pairs it
  with, and anything else is `INTERNAL`. A session drained by shutdown is
  therefore `SESSION_STATE_CLOSED` with cause `worker_shutdown` and status
  `UNAVAILABLE`. The status follows the cause and not the state because a
  session ended through the cancel path (a client `Cancel`, a timeout, a
  slow consumer) is also `SESSION_STATE_CLOSED`.
- **Not enforced yet.** A refused call is counted (see "Refused calls and
  the calls of a connection") but not timed: it keeps its place until
  gRPC reports it done or its connection ends, and at shutdown
  such a call still holds the gRPC server until the drain window ends. A
  connection that opens no call
  is neither counted nor expired, and the number of connections is not
  limited: nothing here bounds transport state that has no call yet. A
  client event larger than the 64 KiB the listener
  receives is rejected by the transport's own limit, below the listener's
  parse: the call ends with a transport status and no `SessionClosed`, and
  its session ends as a cancelled call's does, not failed with a typed
  reason: aborted `client_cancelled`, unless it had nothing in progress
  and the half-close of the failed read closed it first. Failing it with a
  typed reason would mean receiving it first, and gRPC buffers a whole
  message before it hands it over. A text
  segment is refused as empty when it holds nothing but ASCII whitespace;
  other Unicode whitespace is not recognized, so such a segment is accepted
  and synthesized. The listener's sources build only with the
  feature, which the clang-tidy job leaves off, so CI runs no static
  analysis on `runtime/src/serving/stream/`. What covers them is the build's
  warnings; the address and undefined-behaviour sanitizer leg, which runs
  the listener's tests with manual memory poisoning off because the
  dependency tree's gRPC and abseil are not instrumented; and the thread
  sanitizer leg for the write path and the wait for a call's first message
  alone, since that leg cannot run a gRPC call.
- **Shutdown order.** `shutdown()` stops admission and drains every live
  session with `worker_shutdown`, so later `Open`s are refused with
  `admission_closed`, also by a worker that has no dispatch, and a call
  that has not sent its `Open` is refused at once, so it does not hold the
  gRPC server through the drain unless the transport cannot deliver its
  status. `stop()` stops the stream listener first:
  it waits for the
  stream sessions to close until `shutdown.drain_deadline` after the
  shutdown was asked, then shuts the gRPC server down with what is left of
  that window (a call still open is cancelled, which aborts its session),
  stops the dispatch and destroys the manager. Only then does it stop the
  HTTP listener and run the HTTP drain
  below, which waits only for what is left of the same window. One value
  holds that window (`runtime/src/serving/drain_window.hpp`): whichever
  drain is asked first begins it, and each later one is given the same
  end. The worker logs that end as `drain_ends_ns`, on the clock of the
  lines' `ts_ns`, when it stops the stream listener (`stream listener
  stopping`) and when it starts the HTTP drain (`draining requests`); a
  test reads both and holds them to the instant shutdown was asked.
  The HTTP port stays open until the stream listener has stopped:
  `/health` answers 503 from the moment shutdown was asked, and the router
  refuses new inference requests with `not_ready`. The HTTP listener's
  stop then closes the port at once and waits for its request handlers,
  with no deadline of its own, so a unary request in flight still holds
  the stop, now after the stream sessions have had their window, and
  nothing answers on the HTTP port while it does. The two waits add up.
  In the worst case a stop takes the whole drain window, which stream
  sessions that drain slowly use up, and then the HTTP listener's wait for
  its handlers, for as long as a unary request in flight runs; the HTTP
  drain after that has nothing left of the window to wait in. Queued unary
  requests are cancelled (step 3 of the shutdown flow below) only after
  the stream listener and then the HTTP listener have stopped. A test
  holds the order of the two ports, with a unary request held at one gate
  and the stream listener's stop held at another: `/health` answers 503
  once shutdown is asked; the stream listener's port closes while the
  request is still in flight; with that port closed and the stream
  listener still stopping, the HTTP port accepts and `/health` still
  answers 503; the HTTP port closes next, with the request still held.
  The second gate is a connection that opens no call and does not answer
  the ping gRPC sends a connection it is shutting down: gRPC's shutdown
  closes the port first and then waits for its connections to end, such a
  one for up to 20 s (gRPC's limit on that ping, inside the drain window),
  and the test ends the connection itself. A stop whose HTTP port closes
  first fails the test. No test holds the HTTP port open during a stream
  drain: a worker gives its listener no dispatch yet, so none of its
  sessions lives long enough to hold one. The stopped
  listener stays in place until the worker is destroyed, so a `shutdown()`
  on another thread never finds it gone. The control channel and the signal
  pipe are unchanged and still end with the worker.

The `ServingWorker::create(config)` factory builds, in this order:

1. `BufferManager` (V01-E03), sized to `config.buffer.capacity_bytes`.
2. `BackendRegistry` populated by `register_builtin_backends` unless
   the caller supplied a registry (tests do).
3. `ExecutionSession`. When `deployment.use_mock_session` is true,
   the in-process `MockServingSession` is constructed; otherwise the
   registry resolves `deployment.model.backend_hint` and the worker
   calls `load` + `prime`. `deployment.model.runner_profile`, when
   present, reaches the backend on the `ModelSpec`; the `python_pytorch`
   adapter starts its sidecar in that installed profile's environment.
   A speech deployment that serves stream sessions gets its session
   together with its job bridge
   ([The speech dispatch in the worker](#the-speech-dispatch-in-the-worker)).
4. `InferScheduler` through `make_scheduler` (V01-E06), with the
   system steady-clock and a scheduler-event sink that mirrors
   metrics and health. The speech dispatch, when the session came with a
   bridge, is constructed once that clock exists.
5. `AsyncPolicyStore`, `ServingPipeline`, `RequestRouter`, and the
   `HttpServer`.

Component construction is deterministic. If any step fails, the
factory returns the typed error from the failing layer and no
listener is opened. The binary's exit codes are:

| Code | Reason |
| ---- | ------ |
| 0    | Normal shutdown after start. |
| 64   | Configuration parse / validation failure. |
| 65   | Component build / session load failure. |
| 66   | Listener bind / accept failure. |
| 70   | Internal error, and a worker that stopped itself because its speech backend stalled ([A stalled speech backend](#a-stalled-speech-backend)). |

A worker that exits with 64 or 65 ends its stderr with one JSON line, the
startup failure record, written whether or not `enable_stderr_logs` is
set:

```json
{"component":"serving","fields":{"code":"unsupported","message":"..."},"level":"error","message":"worker startup failed","ts_ns":0}
```

`fields.code` is the typed error of the step that failed, in its wire
spelling; when a sidecar runner refuses a load it is the runner's code and
message unchanged. If the record itself cannot be built the worker writes
the plain line `worker startup failed` instead. The agent reads this line to answer the deploy that
started the worker (see [agent.md](agent.md#serving-worker-handoff-v01-e08-f05)).

## Loopback HTTP server

The HTTP server is a small in-tree implementation (see
`runtime/src/http/http_server.cpp`). Selection rationale:

- **Loopback by default.** The server refuses to bind anything
  outside the documented loopback set (`127.0.0.1`, `::1`,
  `localhost`) unless `allow_non_loopback = true` is set in config
  *and* the validator's test-only environment opt-in is present.
  The listener honors the address family the host literal names: an
  IPv4 literal binds an `AF_INET` socket, `::1` binds an `AF_INET6`
  socket (IPv6-only), and `localhost` maps to the IPv4 loopback.
- **Request limits.** `max_body_bytes`, `max_header_bytes`, and
  `request_timeout` are enforced inline by the parser. Oversized
  requests return 413 before any buffer-plane allocation is
  attempted.
- **Graceful shutdown.** The server has a dedicated `stop()` method
  that closes the listening socket and joins the worker pool; the
  composition root drains the scheduler after the server stops.
- **Testability.** The server can bind an ephemeral port and surface
  the assigned port through `bound_port()`. Tests connect without
  racing on a fixed port.
- **Dependency weight.** Nothing beyond POSIX sockets and
  `nlohmann::json`.

The server does *not* implement keep-alive, HTTP/2, TLS, multipart,
SSE, or chunked transfer encoding. Adding any of those is a
deliberate v0.2+ decision.

## Route contract

| Method | Path | Body | Response |
| ------ | ---- | ---- | -------- |
| POST   | `/infer` | `serving_http_envelope.InferRequest` | `InferResponseSuccess` (200) or `InferResponseFailure` (4xx/5xx, or 200 when the backend itself reports the failure). |
| POST   | `/policy/infer` | `serving_http_envelope.InferRequest` | `AsyncAccepted` (202) with `result_url` and `cancel_url`, or 501 when the resolved backend lacks `supports_async`. |
| GET    | `/policy/result/<request_id>` | _empty_ | `AsyncResult` (200) — `status` discriminates `pending`/`in_flight`/`completed`/`cancelled`/`stale`/`failed`/`expired`, or 501 when the resolved backend lacks `supports_async`. |
| POST   | `/policy/cancel/<request_id>` | _empty_ | `AsyncCancelResponse` (200 if cancelled, 404 otherwise), or 501 when the resolved backend lacks `supports_async`. |
| GET    | `/health` | _empty_ | `serving_health` (200 ready/degraded, 503 otherwise). |
| GET    | `/metrics` | _empty_ | Prometheus 0.0.4 text body or `serving_metrics` JSON. |

Every response carries the `x-correlation-id` header. Clients that
do not supply one receive a server-generated `cid-<hex>` value.

Routes that match a known path but a different method return 405;
unknown paths return 404. Loopback binding is enforced server-side.

## Request normalization

`/infer` and `/policy/infer` share the same decoder
(`tensorplate::serving::decode_infer_request`). The decoder validates
the envelope shape *before* any buffer-plane allocation:

1. Required fields (`request_id`, `endpoint`, `inputs`) must be
   present and non-empty.
2. Per-input dtype / shape / layout must be parseable.
3. Base64 payload must decode to at least `byte_offset + byte_size`
   bytes.
4. Metadata strings, if present, must be non-empty.

Validation errors map to typed `Error::Code` values. A typed error that
ends a request before or instead of an inference result is answered with
the status its code maps to below; a few routes answer a specific
condition with a fixed status instead (413 for an oversized payload, 501
when the backend lacks async support, 404 for an unknown async request
id), and a failure the backend itself reports comes back as a 200
`InferResponseFailure` carrying its code:

- `config_invalid` (400)
- `shape_mismatch` (400)
- `unsupported` (415)
- `oom_error` / `resource_exhausted` (429)
- `timeout` (504)
- `not_ready` / `unavailable` (503)
- `cancelled` (499, Client Closed Request)
- `inference_failed` / `load_failed` / `internal` (500)

Only after structural validation passes do the input payloads cross
into `BufferManager` via `build_named_inputs`; the buffer plane owns
the bytes from that point forward.

## LeRobot-compatible async path

The async-policy routes implement the LeRobot PolicyServer-compatible
shape directly. The request envelope is identical to `/infer`; the
response shape is `AsyncAccepted` instead of `InferResponseSuccess`.
Clients poll `result_url` until `status` is one of `completed`,
`cancelled`, `stale`, `failed`, or `expired`.

The route family is enabled only when the resolved backend capability
advertises `supports_async=true`. Sync-only real adapters, including
the v0.1.0 Python/PyTorch sidecar, return 501 before request buffers
are retained or scheduler entries are admitted. This prevents a client
from receiving cancellation acknowledgement while the backend keeps
executing work that it cannot cancel.

Stale-request behavior: when an incoming `/policy/infer` request
carries `metadata.stale_after_sequence = N`, the router tags every
async entry whose `action_chunk_sequence <= N` as `stale` and
dispatches a `cancel(StaleSequence)` to the scheduler. The scheduler
releases the buffers of any queued requests; in-flight requests
suppress their result publishing.

`/policy/cancel/<id>` flips the entry to `cancelled` and dispatches
`cancel(ClientRequest)`.

`/policy/result/<id>` releases the entry's buffers as soon as a
completed result is delivered; subsequent reads return 404.

## Health and metrics

`HealthState` is a thread-safe state container updated by the
composition root, by the scheduler event sink, by the shutdown
controller, and by a stalled speech backend, which sets `failed`. The
schema mirrors `protocol/schemas/serving_health.json`.

`ServingMetrics` is a bounded counter / histogram bag with four
labels: `endpoint`, `model_class`, `model_name`, `backend`. The
Prometheus exposition format is the default; JSON mode mirrors
`protocol/schemas/serving_metrics.json`. That schema also admits the
optional labels `row` and `mode`, each held to the value list registered
in [`docs/observability/metrics.md`](../observability/metrics.md#labels),
and defines the `outcome` and `stage` value lists for per-series labels;
`ServingMetrics` sets none of them.

`/metrics` refreshes the scheduler gauges from `InferScheduler::metrics()`
on every request: `scheduler_queue_depth`, `scheduler_in_flight` (the
logical in-flight count under its protocol 0.1 name),
`scheduler_in_flight_logical` (the same value),
`scheduler_in_flight_physical` and `scheduler_in_flight_physical_cancelled`;
the Prometheus names add the `tensorplate_serving_` prefix. A request
cancelled while the dispatcher is inside `infer` leaves the logical count at
once and the physical count only after `infer` returns and the pipeline
reports its completion. `/health` reports `in_flight` as the logical count.
[`scheduler.md`](scheduler.md#metrics-and-events) defines the counts.

Latency histograms use the same bucket boundaries everywhere:
`0.5, 1, 2, 5, 10, 25, 50, 100, 250, 300, 500, 600, 1000, 1500, 5000, +Inf` ms.
They are chosen so that a streaming speech latency threshold of 100, 250,
300, 500, 600, 1000 or 1500 ms falls on a boundary and is read from one
bucket instead of being interpolated inside a wider one. A scraper that
stored the boundaries from a build without 300, 500, 600 and 1500 sees four
more `le` values. The JSON body counts per bucket, so its 1000 and 5000 ms
buckets no longer include observations at or below 600 and 1500 ms; the
Prometheus body is cumulative and its existing buckets keep their counts.

## Graceful shutdown

The binary installs SIGINT/SIGTERM handlers before creating the worker.
Each handler only writes the signal number to a nonblocking self-pipe and
preserves `errno`; it never touches the worker, allocates, logs, or locks.
The main thread waits on that pipe after `start()`, then calls
`shutdown("SIGINT" | "SIGTERM")` and `stop()` in ordinary thread context.
While it waits it reads the worker's health once a second: a worker that
reads `failed`, which only a stalled speech backend sets, is shut down the
same way (the worker has already logged why) and exits with the code its
`stop()` returns.
A second signal during shutdown cannot re-enter either call. SIGPIPE
remains ignored through worker teardown.

The pipe retains a signal received while the model loads. After a successful
load, the main thread shuts down without opening the listener if a signal
is already queued. Load failures retain their typed startup failure record
and exit status. The pipe is close-on-exec, lives above the standard
descriptors, and outlives the worker and its threads. The control channel's
teardown order is unchanged: its thread stops before the components it may
reach are destroyed. Library callers can still use `serve_forever()` and
request shutdown from an ordinary thread.

Shutdown flows through `ShutdownController` (Running → Stopping →
Draining → Stopped):

1. Composition root flips the controller to `Stopping`. The router
   and pipeline refuse new admission with `not_ready` / 503; the
   HTTP server keeps serving in-flight responses.
2. The composition root stops the stream listener, then the HTTP listener,
   as described under [Stream listener](#stream-listener).
3. If `shutdown.cancel_queued_immediately` is true, `InferScheduler::shutdown`
   cancels every queued request and releases their input buffers. This
   follows step 2: the stream drain and the HTTP listener's stop come first.
4. The composition root waits up to `shutdown.drain_deadline` for
   the scheduler's queue depth and logical in-flight count to reach zero.
5. `InferScheduler::shutdown` is called again to clear anything left.
6. `AsyncPolicyStore::cancel_all` releases retained input / completed
   buffers.
7. `ExecutionSession::unload` is called exactly once.
8. The controller transitions to `Stopped` and the dispatcher /
   evictor threads join.

ASAN-clean: the buffer manager reports zero active buffers after
shutdown in the V01-E07-F08 integration tests.

## Control channel

A serving config may name the deployment generation its worker serves
(`deployment.generation`, an integer from 1 to 2^53 - 1). A worker
with a generation is a member the agent controls, and the agent passes
it one end of a socket pair as fd 0. The agent's member registry writes
one for every worker it starts. A config without one, which a worker
started by hand or by the agent's separate `WorkerSupervisor` has, starts
a worker with no control channel and fd 0 is not touched.

**Taking the descriptor.** `ServingWorker::create` takes the socket
before it builds anything, while the process has one thread and no
child (`take_inherited_control_socket`,
`runtime/src/serving/control_channel.hpp`). The config is validated
first, and a member's `deployment.endpoint` must be a deployment id (1
to 128 of letters, digits, `.`, `_` and `-`), because it is the name the
worker answers by. fd 0 must be a connected stream socket of the local
family. It is duplicated to a private descriptor above stderr
that is close-on-exec from its first instant, and fd 0 is reopened on
`/dev/null`: the sidecar inherits fd 0, so the socket must not stay
there. A member whose fd 0 is anything else, or whose agent has already
closed its end, does not start: `create` returns `unavailable`
(`control_descriptor_absent` or `control_peer_closed`), the binary ends
its stderr with the startup record and exits with the load-error
status, and no listener was ever opened.

**The control thread.** `ControlChannel` starts next, before the model
loads, so that the agent's polls are answered during a long load. It is
the only code that reads the socket, and it never calls into an
execution session: a backend job that does not return cannot delay an
answer. Requests are `worker_control.json` frames, one JSON object per
line, at most 65,536 bytes each. A request that names another
deployment or generation is answered `member_mismatch` with this
worker's member and goes no further. `ledger_status` and the other five
operations are answered through `ControlTarget`, whose methods return
at once. The worker's target implements none of them yet: it reports an
empty ledger and refuses `admission_fence`, `activate`, `retire`,
`quota_assign` and `pressure_directive` as `unsupported`, so the agent
is never told that an operation took effect. An answer the wire format
refuses is sent as an `error`. A complete frame that does not decode is
skipped; a line longer than the frame limit ends the channel, and so
does a response the agent does not take within one second.

**Contact.** The agent is in contact from the start. Three seconds
without a decodable request is loss of contact, which the next request
ends; a closed channel is loss of contact for good. While contact is
lost `ControlChannel::admission` refuses with `unavailable`
(`control_contact_lost` or `control_channel_closed`). Nothing consumes
it yet: it is the admission check logical sessions will use, and
sessions already admitted are not affected. Losing the agent does not
stop a worker that is serving. A worker whose channel ended while it was
being built does not start serving: `start` returns `unavailable`
(`control_channel_closed`) and opens no listener.

**The sidecar's descriptors.** The Python sidecar launcher closes every
descriptor above stderr between `fork` and `exec` (`close_range`, or a
scan when the kernel has none), and the sidecar's environment is built
before `fork`: a member has a thread by then, so the child makes system
calls only. The sidecar connects to its transport socket by path, so it
needs nothing the worker holds.

## Logical sessions

A logical session is one client stream pinned to the deployment
generation its worker serves. Every session of a worker shares the
worker's single loaded backend: opening one never loads weights or
starts a process. `include/tensorplate/serving/session.hpp` defines its
lifecycle as `LogicalSessionMachine`, a pure state machine. It holds no
timers, clocks, queues, threads or I/O. Its owner applies one session's
events in order and carries out the effects of each accepted event, in
ascending `LogicalSessionEffect` order, before applying the next. The
serving worker does not create logical sessions yet; this is the
lifecycle the streaming serving modes build on.

`SessionManager` owns those machines, their count reservations and their
input credit. It opens only for the worker's generation, admits at most
the configured count (2,048 by default), and offers an admission check
for a separate memory quota. An optional bounded initialization step
runs after slot reservation and before `emit_ready`; failure emits one
terminal error and holds the slot for physical cleanup. Its sink applies
each machine's effects in order; the manager serializes event
application and sink calls across client and timer threads. Each
transition names the event that caused it. Besides every transition with
effects, the sink receives each return of input credit (a transition
with no event and no effects, carrying the status after it) and each
`finalize_completed` that made a session `active` again, inside the same
serialized section, so the status a sink last saw is never older than
one it saw before. That does not hold for the status a call returns,
which reaches its caller after the section ends: a stream takes the
credit it reports from the sink only. A slot remains held after cancel,
failure or drain until `release_acknowledged` produces `release_slot`.
`stop_admission_and_drain` closes admission, starts each live session's
drain, and waits for `drain_completed` followed by physical release
before closure. The manager does not reload a backend or schedule a
physical job.

Each session is opened with `SessionBudgets`
(`runtime/src/serving/session/credits.hpp`), derived from the byte rate
of the audio format negotiated at open. A rate of zero is refused, as is
one whose window would exceed the largest audio payload of a backend
job:

| Budget | Audio in, transcripts out | Text in, audio out |
|---|---|---|
| Input credit | 1 second of accepted, unconsumed audio | 2 waiting segments of 8 KiB combined, plus 1 being executed or delivered |
| Output PCM | none | 2 seconds |
| Output control and transcript metadata | 16 KiB, the last 1 KiB for lifecycle messages only | the same |

**Input credit.** `accept_input` applies `data` for one audio chunk or
text segment and charges its bytes in the same step, so input is charged
only when the state accepts it; a drain ignores input without charging
it. Input above the credit fails the session with
`input_credit_exceeded`. Audio credit returns as the pipeline consumes
it (`release_audio_input`). A text segment's waiting credit returns when
it takes the single active slot (`start_text_segment`), and the slot
stays occupied until the caller finishes the segment
(`finish_text_segment`), which it does only once the segment's output
has been delivered or discarded, not when synthesis ends: at most three
accepted segments exist at once. A release the credit cannot match is a
defect in the caller and fails the session.
Every transition the sink is shown carries, beside its status, how many
text segments wait against the two that may (`waiting_segments`), taken
in the same step. With the credit bytes of the status it is the input
credit a stream reports; the public status type does not carry it.

**Output queue.** Every server message of a stream passes through its
`BoundedOutputQueue`, which the manager creates at open and hands to the
sink with each transition. The queue has several producers and one
consumer, the stream's writer, which registers a callback once
(`set_consumer`; a second registration is refused). The callback runs
outside the queue's lock on the offering thread after an offer was
queued, may stand for several items, and must not block or call the
session manager, whose sink may be the one offering; if something is
already unsent it also runs inside the registration, on the caller's
thread. There is no unregistering it, and the queue outlives the stream,
so the callback captures shared or weak state and never a stream object
that may end first. Two producers may call it at the same time (the
dispatch's thread and a sink that queues a lifecycle message), and a
call may find nothing to take, because another call's `take` or a
suppression got there first. A producer of task output offers an item; when its budget has no
room the offer answers `full`, the producer keeps the item and pauses
until output drains, so task output is never dropped while the session
lives. Task output may fill the metadata budget only up to its last 1
KiB, which is kept for lifecycle messages so that a backlog of
transcripts cannot keep a reply or `CancelAccepted` out. A lifecycle
reply is answered `full` only when the peer has left the whole metadata
budget unread; the sink, which cannot wait, does not queue that reply.
For a live or draining session the no-progress limit below is already
running; a cancelled session ends with its terminal outcome. The stream
assigns a message its sequence only when the transport takes it, so an
unsent partial hypothesis can still be replaced in place by its newer
revision, and a final supersedes the unsent partial of its utterance; a
final is never replaced. A lifecycle reply whose newest instance says
everything can carry a key and is replaced the same way, which keeps
such replies from accumulating. An item stays charged from the offer
until the transport confirms its delivery, so what the transport holds
counts against the same budget. On `suppress_output` the manager
discards the unsent task output before the sink runs and the queue
refuses task output from then on, while lifecycle messages still pass.
The terminal outcome is accepted once, even when the budget is full, and
nothing is accepted after it. The queue outlives the session's slot,
because lifecycle messages and the terminal outcome are still delivered
after release.

**No-progress limit.** The queue keeps a stall clock: it starts when
output first awaits delivery, moves only when a delivery is confirmed,
and stops when nothing awaits delivery. Offering more output, a ping or
a status request does not move it. A live or draining session whose
clock reaches 5 seconds is aborted with `slow_consumer` by the same
timer thread that enforces the limits below.

**Status.** Each transition carries a `LogicalSessionStatus`: the state,
the accepted input items still owned, and the use of the input credit
and of both output budgets. Output use counts every undelivered byte,
queued or held by the transport. `SessionManager::status` returns the
same for a live session.

**Tombstones.** When a slot is released the manager keeps the session's
key, generation, final state and end cause (its code, and at most 64
bytes of its reason) for 60 seconds, at most 1,024 per worker with the
oldest dropped first. A call naming such a session is refused with
`session_ended`, so a report that raced the release can be told from an
`unknown_session`.

**Finalize deadline.** A session may owe a finalization or a drain only
for a bounded time, set by what it takes as input: 10 seconds for audio
input, where a transcript is finished, and 120 seconds for text input,
where accepted segments are synthesized and delivered (two waiting and
one running, at most 30 seconds of audio each, at the pace of playback).
The deadline starts when an accepted event first leaves the session in
`finalizing` or `draining`, and stops when the session is `active` again
or `drain_completed` is applied. Nothing in between moves it: not a
delivery, a further automatic endpoint, a client Finalize taking over,
or a half-close during a finalization. A session that reaches it is
aborted through the cancel path like the other timeouts, with
`finalize_timeout` or, for a drain the worker started, with that drain's
own cause. The no-progress limit above is separate and ends a stalled
reader sooner. Once a drain has completed, the session waits for the
backend to acknowledge release without this deadline; bounding that wait
belongs to backend cleanup.

**Generation of later messages.** `open` refuses a generation other than
the worker's. For a later client message or a backend report,
`check_generation` compares the generation it names with the session's:
a different one, zero for an absent field included, fails the session
with `stale_generation` and the caller discards the message. After the
outcome is fixed the check is still refused but changes nothing.

`test/unit/fixtures/session_lifecycle.json` holds the deadline rows and
step-by-step lifecycle cases (deadlines, generation, segment completion,
the ways a session closes, expiry) that the unit tests replay against
the manager; a transport binding can replay the same cases.

`SessionLimits` defaults to 60 seconds idle, a 10-second client heartbeat
cadence, 30 seconds liveness, and a 60-minute absolute duration. Data
and client control refresh idle activity; only `ping` refreshes heartbeat
liveness. Draining suspends idle and heartbeat expiry so accepted work
can finish after the client write-half-closes; the absolute duration
and the finalize deadline still apply. A dedicated timer thread finds due sessions using the
injected monotonic `SchedulerClock`, independent of the serving
worker's existing request evictor. Unit tests advance
`FakeSchedulerClock` without sleeping. The current HTTP composition
root does not instantiate `SessionManager`; the [stream
listener](#stream-listener) does, supplies its effect sink, chooses each
session's budgets and drives its output queue, with a [session
dispatch](#session-dispatch) beside it for the task work.

States: `opening`, `active`, `finalizing`, `draining`,
`cancel_requested`, and the terminal `closed` and `failed`. Events come
from the client (`open`, `data`, `finalize`, `cancel`, `half_close`,
`ping`, `status_request`) and from the owner: admission (`admitted`),
the pipeline and backend (`automatic_endpoint`, `finalize_completed`,
`drain_completed`, `backend_reset`, `release_acknowledged`), timers,
pressure and worker control (`drain`, `abort`, `fail`). Effects, in
execution order: `suppress_output`, `emit_cancel_accepted`,
`request_cleanup`, `emit_ready`, `accept_input`, `emit_reply`,
`start_finalize`, `start_drain`, `release_slot`, `emit_terminal`.

The machine distinguishes ten configurations: the public states, with
`finalizing` split by whether a client Finalize or an automatic endpoint
started it (only the latter still accepts input), `draining` split by
whether release was requested, and `failed` split by whether release was
acknowledged. The table below is the machine's complete transition
function; the unit tests hold it to the same table in
`test/mocks/session_transition_fixtures.hpp`.

Configurations: O `opening`, A `active`, FF finalizing after a client
Finalize, FE finalizing after an automatic endpoint, DW draining with
work in progress, DR draining with release requested, CR
`cancel_requested`, CL `closed`, FH failed holding its reservation, FR
failed and released. Effects: Rdy `emit_ready`, In `accept_input`, Rep
`emit_reply`, Fin `start_finalize`, Drn `start_drain`, Cln
`request_cleanup`, Sup `suppress_output`, Ack `emit_cancel_accepted`,
Rel `release_slot`, Term `emit_terminal`. `=` stays in the same
configuration, followed by its effects if any; `X` refuses.

| | open | data | finalize | cancel | half_close | ping | status_request | admitted | automatic_endpoint | finalize_completed | drain_completed | drain | abort | fail | backend_reset | release_acknowledged |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| O | X | X | X | CR Sup Cln | X | X | X | A Rdy | X | X | X | CR Sup Cln | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| A | X | = In | FF Fin | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | FE Fin | X | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| FF | X | X | = | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | = Fin | A | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| FE | X | = In | FF Fin | CR Sup Ack Cln | DW Drn | = Rep | = Rep | X | = Fin | A | X | DW Drn | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| DW | X | = | = | CR Sup Ack Cln | = | = Rep | = Rep | X | = Fin | = | DR Cln | = | CR Sup Cln | FH Sup Cln Term | FH Sup Cln Term | X |
| DR | X | = | = | CR Sup Ack | = | = Rep | = Rep | X | X | X | X | = | CR Sup | FH Sup Term | FH Sup Term | CL Rel Term |
| CR | X | X | X | = | = | X | X | = | = | = | = | = | = | = | FH Term | CL Rel Term |
| CL | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | = |
| FH | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | FR Rel |
| FR | X | X | X | = | = | X | X | = | = | = | = | = | = | = | = | = |

Rules the table encodes:

- **Refusals.** A refused event changes nothing. A client message the
  state does not permit, including a client request after the outcome is
  fixed, is a protocol violation: `not_ready` with context
  `illegal_transition`. An owner report that its producer cannot emit in
  that state is a defect in the owner: `internal` with context
  `unexpected_report`. The owner answers a refusal by applying `fail`
  with the refusal as its cause; `fail` is ignored once cancellation or a
  terminal outcome has fixed the result, so doing so is always safe.
- **Accepted without effect.** Events that can legitimately arrive late
  or race a state change, for example: a repeated cancel, half-close,
  drain, abort or fail, or a Finalize repeated while finalizing; input or
  a Finalize racing a drain; late pipeline reports after cancellation or
  failure; a duplicate release acknowledgement.
- **Finalization.** The owner applies `finalize_completed` before it
  publishes the message that completes the client's Finalize, so the
  client's next input finds the session active.
- **Exactly one terminal outcome.** `emit_terminal` is produced once per
  session. `closed` writes it after physical release; `failed` writes it
  on entry and keeps its reservation until release is acknowledged.
  Nothing leaves `closed` or `failed`, and no task output follows
  `suppress_output`.
- **Cancellation is not cleanup.** `emit_cancel_accepted` answers a
  client Cancel after Ready, while the outcome is still open, at once;
  the reservation is returned (`release_slot`) only after the backend
  acknowledges physical release. A Cancel before Ready ends the session
  without an acknowledgement, and a Cancel after a worker abort or a
  failure is answered by the terminal outcome alone. A backend reset
  while cancellation waits for release fails the session.
- **Generation binding.** `LogicalSessionMachine::open` binds a session
  to the worker's generation and refuses a mismatched or absent
  generation (`not_ready`, `stale_generation`); a worker without a
  generation is `config_invalid` (`invalid_generation`). The owner checks
  every later message or backend report with `check_generation`,
  discards a stale one and applies `fail`.

How sessions end, and the code their terminal outcome carries:

| Situation | Owner applies | Ends | Code (reason) |
|---|---|---|---|
| Client Cancel | `cancel` | `closed` | `cancelled` |
| Client half-close | `half_close`, then `drain_completed` and `release_acknowledged` | `closed` | none |
| Input above the session's credit | `accept_input` (the manager applies `fail`) | `failed` | `resource_exhausted` (`input_credit_exceeded`) |
| Input item without bytes | `accept_input` (the manager applies `fail`) | `failed` | `config_invalid` (`empty_input`) |
| Credit release that does not match what is held (a defect) | a release call (the manager applies `fail`) | `failed` | `internal` (`input_release_mismatch`, `segment_stage_violation` or `wrong_input_kind`) |
| Output undelivered past the no-progress limit | the manager's timer applies `abort` | `closed` | `resource_exhausted` (`slow_consumer`) |
| A finalization or a client's drain not finished by the finalize deadline | the manager's timer applies `abort` | `closed` | `timeout` (`finalize_timeout`) |
| Backend process reset or reaped | `backend_reset` | `failed` | `unavailable` (`backend_reset`) |
| Deployment generation retiring | `drain`, then `abort` at the deadline | `closed` | `unavailable` (`deployment_retired`) |
| Worker shutting down | `drain`, then `abort` at the deadline | `closed` | `unavailable` (`worker_shutdown`) |
| Stale generation in a later message | `check_generation` (the manager applies `fail`) | `failed` | `not_ready` (`stale_generation`) |
| Protocol violation | `fail` | `failed` | `not_ready` (`illegal_transition`) |
| Owner report the state does not permit (a defect) | `fail` | `failed` | `internal` (`unexpected_report`) |

A drain the worker starts is held to the session's finalize deadline like
any other, and keeps its own end cause when that deadline cuts it: a
retiring or shutting-down worker's audio session that has not drained in
10 seconds closes with `unavailable` (`deployment_retired` or
`worker_shutdown`), so that its client knows to reopen elsewhere. The
deadline decides when such a session ends, not why. A drain the client
started, and any finalization, ends with `timeout` (`finalize_timeout`).
Another limit that ends a worker's drain first, such as the no-progress
limit, reports itself.

The reason strings are the failure reasons in
[`failure-reasons.md`](../observability/failure-reasons.md) where one
exists. The owner keeps the end cause: the error carried by the most
recent accepted event that moved the session into `draining`,
`cancel_requested` or `failed`, which can only move from a drain to a
cancel to a failure.

## Session dispatch

`SessionDispatch` (`runtime/src/serving/session/dispatch.hpp`, internal)
is the seam between a streaming transport binding and the work a
deployment does for its sessions. It sits beside the `SessionManager`,
not around it: the binding stays the manager's sink and the only writer
of lifecycle messages, and the dispatch is the only producer of task
output. The header names no wire schema type and no backend job type, so
the binding owns the whole wire mapping and the dispatch owns the whole
mapping from session input to backend work. One dispatch serves one
worker process and therefore one deployment generation, fixed when it is
built; the manager refuses any other generation at open, so a dispatch
never sees one.

The effects of the table above divide between the two:

| Effect | Carried out by |
|---|---|
| `emit_ready`, `emit_reply`, `emit_cancel_accepted`, `emit_terminal` | the binding, as lifecycle messages in the session's output queue |
| `accept_input` | the binding, which hands the accepted frame or segment to the dispatch (`audio`, `text_segment`) |
| `start_finalize` | for a client Finalize, the binding, which calls `finalize` with that message's client sequence; for an automatic endpoint, the dispatch that raised it |
| `start_drain`, `request_cleanup` | the dispatch, from the transitions the binding forwards (`on_transition`) |
| `suppress_output`, `release_slot` | the manager: it suppresses the queue before the sink runs and returns the slot itself |

**Opening.** Before admission the binding asks `negotiate` what the
deployment would grant a request: the audio format and the limits the
stream reports when the session is ready, or a typed refusal that
reserves nothing (`unsupported`, with `unsupported_input_kind`,
`unsupported_audio_format`, `unsupported_language` or
`unsupported_voice`). `budgets_for` derives the session's budgets from
the granted terms. The binding opens the session and calls
`open_session` from the manager's initialize hook, so the dispatch knows
a session before any transition names it. `open_session` runs under the
manager's lock: it records, and refuses only what it can tell without
waiting; anything the backend decides fails the session later.

**What the binding checks.** Wire validation is the binding's, before
`accept_input`: a frame's size, sample count and offset against the
granted frame bounds, a segment's text against the text limits
(including the limit over the segments between two Finalizes), and that
a segment id is above every earlier one of the session; the dispatch
carries the id unchanged and does not check it. A frame shorter than the
smallest the terms grant is valid only as the last before a Finalize or
a half-close, which the binding cannot know when it arrives: it accepts
the frame and fails the session if more audio follows first. An
automatic endpoint between the short frame and the next audio does not
make the short frame valid: the client cannot have known of it when it
sent, and only its own Finalize or half-close ends an utterance on a
short frame. A frame
found invalid after it was accepted fails the session: the binding
applies `fail` with the cause its own validation would have given before
acceptance. That requests the session's cleanup, after which nothing is
owed, so the frame is not handed over. The limits on produced audio are
the dispatch's to keep, since only it sees the audio.

**Calls and their order.** Every method a binding calls is thread-safe
and returns without waiting on a job, on the output queue or on the
manager. One session's hand-overs (`audio`, `text_segment`, `finalize`)
are made one at a time, in the order the manager accepted them, each
after exactly one `accept_input` charged the item's size or one accepted
Finalize; two hand-overs for one session at once have no defined order
and are not allowed. Calls for different sessions may run concurrently,
and so may `on_transition` or `output_delivered` for the same session.
`on_transition` arrives on whatever thread made the manager act: a
transport thread, the manager's timer thread, the thread that drains the
worker, or a dispatch thread inside its own report; it and
`open_session` therefore only record. The manager calls its sink inside
the call that caused the transition and before that call returns: when
`apply` or `accept_input` returns to a transport thread, the sink has
already run for that client event on the same thread, so whatever the
binding queues from the sink precedes whatever it does with the call's
result. An event that changes nothing reaches no sink: a repeated
Finalize, or input that a drain does not take. Hand-overs return nothing. A
dispatch that cannot use one fails the session through the manager, and
a call for a session that was never opened, whose cleanup was requested,
or after `stop()`, is ignored: that is the whole error contract.

**Reports.** A dispatch reports to the manager and offers output from
threads of its own. For one session it makes one report or offer at a
time, in order; nothing is promised across sessions, and a dispatch may
use one thread for all of them or several. It applies
`finalize_completed` (before it offers the output that answers a
Finalize), `drain_completed`, `release_acknowledged`,
`automatic_endpoint`, `backend_reset`, `fail` and the credit releases,
and no other event: client events, admission, `drain` and `abort` belong
to the binding, the worker and the manager's timers. Because the
manager's sink shows credit returns and completed finalizations in
order, a binding takes every credit it reports from the sink, the
unsolicited update included, and never from the status a call returned.
Credit returns keep arriving for as long as the session holds its slot:
also while it drains, after a cancel or a failure, and after its
terminal outcome was queued, because the dispatch lets go of input as it
cleans up. No client can use credit then, so a binding reports a credit
return only while the transition's state is `active` or `finalizing`
and drops every other one. In those two states there is no transition
whose status a binding must hold back: each was taken in order, inside
the serialized section.

A client's Finalize that arrives while the finalization of an automatic
endpoint is running takes that finalization over, and the manager counts
the two as one. It does the same the other way round and between
endpoints: an automatic endpoint applied while a finalization is running
joins it, although the sink shows `start_finalize` again. The dispatch
therefore applies exactly one `finalize_completed` for all of them; a
second finds the session active again and fails it. The sink's order tells a dispatch which case it is in: a
`start_finalize` it is shown after a completion it applied itself has
come back through the sink starts a new finalization, and one it is
shown before that was answered by the completion.

A Finalize that joined a finalization is still answered by a final of
its own: the final of the utterance it ends, which carries its client
sequence. The final of an utterance an automatic endpoint ended carries
none. The sequence reaches the dispatch only with the hand-over, and
until then the finalization is what covers the Finalize: its deadline
ends a session whose hand-over never arrives. A dispatch therefore does
not begin the report of the one completion while a client Finalize it
has recorded from the sink is still to be handed over. The session stays
`finalizing` across the endpoint's final; more input then fails the
session, and the deadline is the one that started at the endpoint, which
a take-over does not restart. Once the Finalize was handed over the
completion is reported, at the latest before the Finalize's final is
offered. It may precede that final by the length of a backend job, with
the session `active` and no finalize deadline over the answer still
owed. A dispatch that had begun the report before it recorded the
Finalize cannot hold it back: the manager applies the completion after
the Finalize, and the session is `active` again before the hand-over.
In every case a binding hands the Finalize over, and reads the final
that carries the sequence as its answer, never the return to `active`.

**Hand-overs and drains.** Input and a client Finalize are handed over
after the manager's call returns, outside its serialized section, so a
drain can reach the dispatch first. A drain is not complete while the
manager still counts accepted input that was not handed over or worked
through, while an accepted client Finalize is still to be handed over,
or while output of accepted work is undelivered. A binding therefore
hands over everything the manager accepted while the session lives,
whatever happened in between; a hand-over that never arrives leaves the
drain to the session's finalize deadline. For audio input, a client's
half-close finalizes the open utterance, if it holds audio, with an
endpoint of its own reason. That finalization waits, as the drain's
completion does, until no frame the manager accepted is still to be
handed over: a frame accepted before the half-close belongs to the
utterance, so a dispatch that ended it at the half-close would cut the
transcript short and start a second utterance with the frame that
arrives later. A drain the worker starts completes only finalizations
already asked: audio that was accepted and not finalized is not
transcribed. For text input every accepted segment is synthesized
and delivered in either drain.

**Input.** Input belongs to the dispatch from the call that hands it
over; `audio` copies the frame before it returns. Audio credit returns
when a frame joins the utterance being collected, not when the utterance
is decoded. The dispatch keeps that utterance within the longest the
terms grant by ending it itself: an `automatic_endpoint`, with the
duration-limit reason on its endpoint, as soon as the utterance holds
that much and without waiting for more audio. What it collected stays readable
for as long as a backend job may read it, which is until the backend
reports that job released, and is dropped then. A text segment's waiting
credit returns when its work starts and its active slot when the last
item of its output was delivered.

**Output.** The dispatch queues `TaskOutputBody` items only, as plain
values the binding encodes: an utterance's endpoint and then its one
final transcript; a segment's audio chunks and then its completion; and
the answer to a text Finalize, once every segment accepted before it was
delivered. `TaskOutput` will gain alternatives, a partial transcript
first, so a binding's visitor over it has a default case. A chunk carries 20 ms of the session's audio format, the last
of a segment possibly less. A result is charged its text plus a fixed
number of budget units per message and per transcript segment. An offer
answered `full` leaves the item with the dispatch, which produces nothing
further for that session until the binding reports a delivery
(`output_delivered`, after every confirmed delivery of any kind of
item) and then offers it again; no timer covers a report the binding
misses. Nothing is offered after `suppress_output`.

**Cleanup.** A `request_cleanup` is answered by one
`release_acknowledged`, once the backend has released what the session
held, also for a session whose `open_session` was refused or never
called. A backend that never confirms a release leaves the
acknowledgement unsent and the slot held, and whatever shuts the worker
down has to tolerate that.

**Shutdown.** The order is: stop accepting streams;
`stop_admission_and_drain`, after which sessions drain and those that
overrun their finalize deadline are aborted and cleaned up; wait for the
held slots to reach zero or for the shutdown deadline; `stop()` on the
dispatch; destroy the manager; destroy the dispatch last, because the
manager's timer thread calls the sink, and through it the dispatch,
until the manager is gone. `stop()` waits for the report in progress and
none follows; it is safe to repeat and to call while binding calls run.
It must not be called from the manager's sink or initialize hook or from
a queue's consumer callback: they hold the manager's serialized section,
which the report `stop()` waits for needs, or run on the reporting
thread itself. Slots still held at `stop()` are released by nobody: they
end with the manager.

`SyntheticSessionDispatch` is a dispatch with no backend, for exercising
a binding. It is not part of the runtime: it lives in
`test/mocks/synthetic_session_dispatch.hpp` and is built as the test
library `tp_synthetic_session_dispatch`, which a test of a binding links.
What it yields is a function of the bytes handed over, so a binding that
hands over the wrong bytes of the right size is caught: an utterance's
transcript is a digest of its audio (`transcript_of`), and a segment's
PCM follows its text byte by byte (`sample_of`). It serves both input
kinds on one thread under the credit, backpressure, drain and cleanup
rules above, grants a fixed set of languages and voices and refuses any
other, and ends an utterance at the longest its terms grant with an
automatic endpoint. It can be paused at two points. Before work: the
thread parks at the next hand-over, so input stays accepted with its
credit not returned. In a job: an utterance being transcribed or a
segment being synthesized does not complete, and the cleanup of a
session that has one stays unanswered, as with a backend that has not
confirmed a release.

`ReferenceBinding` (`test/mocks/session_dispatch_binding.hpp`) is a
binding with no wire. It keeps what the cases need of the binding's side
of this section: the calls and their order, credit reported from the
sink while input is taken, and no audio after a short frame. It does not
check frame bounds, sample offsets, segment ids or text limits. The
cases written against it (`SessionDispatchContract`, in
`test/unit/session_dispatch_test.cpp`) hold for every dispatch: that
test binary instantiates the suite once for each dispatch it builds,
with what the dispatch serves, what it yields for given input and how
its jobs are held. They take every frame size, sample width and limit
from the terms the dispatch grants, and assert the order of what the
sink shows rather than a count of it. The cases that need the synthetic
dispatch's own gates or reason strings are its own, outside the suite.
The binding can run a hook inside the sink call of a chosen transition,
which is how a case releases a held job inside the manager call that
applies a Finalize: the synthetic dispatch then reports the completion
before it has recorded the Finalize, and another dispatch may take
either order.

### The speech dispatch

`SpeechJobDispatch` (`runtime/src/serving/session/speech_dispatch.hpp`,
internal) is the dispatch that runs backend jobs: it turns session input
into typed jobs through one `BoundedJobBridge`
([backend-registry.md](backend-registry.md), "Sessions with a job
bridge") and their results into task output. The serving worker constructs
it for a speech deployment
([The speech dispatch in the worker](#the-speech-dispatch-in-the-worker)).

It is **provisional**: the least that serves one transcription and one
synthesis, standing in for the mode handlers that will own windows,
partial transcripts and endpointing.

- **Audio input** (16 kHz mono 16-bit PCM, frames of 20 to 320 ms). A
  client Finalize, a half-close or the 30-second limit ends the
  utterance, and its accumulated audio is one `stt_decode`: copied into
  one buffer, the window the whole utterance, `start_sample` its first
  sample on the session's timeline. The result's text becomes the one
  segment of the final transcript, over the whole utterance. An utterance
  without audio runs no job (a job request cannot carry an empty window)
  and yields a final without segments, as does a result without text.
  An ended utterance keeps its audio until its job has it, and nothing
  paces the client behind a slow decode, so the dispatch bounds what
  waits: three utterances may have ended without their final (the one
  being decoded and two behind it), and a fourth, ended by the limit or
  by a Finalize, fails the session with `resource_exhausted`
  (`utterance_backlog_exceeded`). No stream reason names that, so a
  stream reports it as `internal`.
- **Text input.** A segment is one `tts_synthesis` with the session's
  language and voice at normal speed. Its single audio result stays in
  the backend's buffer and is cut from there into chunks of 20 ms: a
  chunk is cut only when the one before it was queued, so backpressure
  holds one chunk and nothing grows. The buffer is released when the last
  chunk was cut, and the segment's completion follows. The longest audio
  of one segment is kept by a job result's own ceiling; the audio of the
  segments between one Finalize and the next is kept here: a segment
  whose audio takes them over the terms' `max_synthesis_audio` fails the
  session with `resource_exhausted` (`synthesis_audio_limit_exceeded`)
  before any of it is queued; a stream reports that as `internal` too.
  A reader that takes no audio leaves its session holding that one
  result, at most about 1.4 MB of the worker's buffer pool, until the
  session manager's no-progress limit ends the session. Nothing here
  bounds the sum: enough such readers exhaust the pool, and other
  sessions' jobs then fail for want of a buffer. Only the number of
  sessions against the pool's size bounds it.
- **One lane.** One job is in flight for the whole dispatch, and sessions
  are served in the order their work became ready. A job keeps the lane
  until the backend ends it, also when its session was cancelled
  meanwhile, or failed at the job's deadline (below): that is where a
  cancel waits behind the backend. With one job in flight nothing else is
  queued behind it in the backend. The
  dispatch records the longest wait between a session's release and the
  backend's acknowledgement (`stats()`), for measuring that cost. The
  scheduler's public contract is not involved: no speech job passes
  through `InferScheduler`.

**The job's deadline and the release's bound.** Both are
**provisional**: `SpeechDispatchOptions` defaults, which no configuration
key sets, sized by arithmetic on one latency measurement of a short clip
on an L4, with margins of several times what it took. A measurement of
long input will resize them.

- A job that has not ended `job_deadline_base` (10 s) plus
  `job_deadline_per_audio_second` (0.5 s) for each second of its audio
  after its submit has missed its deadline. A decode counts its input,
  at most 30 s, so its deadline is 10 to 25 s; a synthesis counts the 30
  s a segment may yield, since its length is known only once it ran, so
  25 s. A live session whose job missed its deadline fails with
  `timeout` (`deadline_missed`), and its release cancels the job; a
  session that has already ended is left as it is. For audio input a
  decode usually runs while its session is finalizing or draining, so the
  finalize deadline, 10 s, comes first: a live session whose decode does
  not end reads `finalize_timeout`, and the job's deadline then only
  starts the release's bound. Not always: an utterance that the 30-second
  limit ended while an earlier one was decoded is decoded once the session
  is active again, and a decode that does not end there reads
  `deadline_missed`. Not measured: a 30-second utterance, and
  three of them back to back as the backlog allows, each in the first
  session after a load. Whisper pads every window to 30 s, so a long
  utterance costs the same encoder pass as the measured one and more
  decoder tokens; the time is not linear in the audio.
- A session's release must be acknowledged within `release_bound` (10 s)
  of its request or, when its job is the one in flight, of that job's
  deadline, whichever is later. The bound is fixed when the release is
  asked, so a job that ends before the backend acknowledges the release
  does not bring it forward. A release the bridge refused counts as
  never acknowledged. When a release passes its bound the dispatch calls
  `on_stall` once, on its thread, and the slot stays held: only the
  backend's acknowledgement frees it, or the backend's reset that the
  worker then performs ([A stalled speech backend](#a-stalled-speech-backend)).
- The bounds are read on the manager's clock. The thread waits for the
  next command or the next bound; a test that moves the clock by hand
  calls `notify_clock_advanced()`, and once a timed wait has ended short of
  its bound, which only such a clock causes, the thread waits for the
  notice alone. Once a bound is due, the commands queued by then are
  handled ahead of its check and none that arrive later: a job's end
  already queued is not taken for a deadline missed, and a stream of
  commands cannot put the check off. `stats()` counts `jobs_past_deadline`
  and `releases_past_bound`.

**What it cannot refuse before admission.** `negotiate` refuses an input
kind the dispatch was not built for, an audio format other than the one
above, and a language or voice that a job request could not carry. The
bridge has no query for what the loaded profile permits, so a job class,
a language or a voice the backend refuses fails the session at its first
job, after it was reported ready.

**Threads.** One thread of the dispatch makes every report to the
manager, every offer to an output queue and every bridge call but two:
`create()` registers the sink, and `stop()` releases the sessions still
open, and cancels the job of one whose release was refused, on its
caller's thread, once that thread has ended. The
binding's calls and the bridge's callbacks only queue for it under one
mutex, which is held for nothing else: no call to the manager, a queue,
the bridge or the buffer manager is made with it held. A callback
therefore never waits on the manager or the backend, never calls
`health()`, and never unloads or destroys the backend session or the
bridge. The bridge keeps the object its callbacks run on, so a callback
that arrives after `stop()` still settles its buffers. That object holds
the input buffer of every job not yet reported released, and releases
what is left when it goes, which is when the dispatch and the bridge are
both gone: whoever owns the bridge destroys it only after the backend
session was unloaded, when no job reads its input any more.

**Releases.** Every session opened here is released through the bridge
exactly once, whether or not it ran a job: at its cleanup, or at
`stop()` for a session still open then, because an unload cancels
nothing and would otherwise wait behind a running job. The cleanup is
acknowledged when the backend reports the session released. A release
the bridge answers `backend_unavailable` is a released session and is
acknowledged at once; any other refusal leaves the slot held. A session
never opened here is acknowledged without a bridge call. A refused
release cancelled nothing and is not asked again: if that session's job
is still in flight at `stop()`, the job is cancelled there, so that the
unload that follows has no job of the dispatch's to wait for.

**Buffers.** The buffer of an audio result is released exactly once:
when its last chunk was cut, or when the result is dropped (its session
is ending or gone, the job is not the one in flight, or the dispatch has
stopped). A job's input buffer is released when the backend reports the
job released, also after `stop()`; a job never reported released keeps
it. The dispatch's jobs ask for no progress, so a progress fragment that
reaches its sink is another submitter's: it is dropped and its audio
buffer released as it arrives.

**Backend outcomes.** A failed job, or a submit the bridge refuses,
fails the session with a cause whose reason is the stream's:

| Bridge outcome | Session failure |
|---|---|
| `failed`, `unavailable` / `backend_reset` | the `backend_reset` event, reason `backend_reset` |
| `failed`, `unavailable` / `session_unloaded`; `submit` refused `backend_unavailable` | `backend_unavailable` |
| `job_class_unsupported` or `job_not_permitted`, from `submit` or as `failed` | `backend_unsupported_capability` |
| `job_capacity_exhausted`, from `submit` or as `failed` | the bridge's error unchanged (`resource_exhausted`; `internal` on a stream) |
| any other failure | the bridge's error unchanged; its code decides the stream's reason |

A job the backend's loss ends while its session's cancel still waits
for the release applies the `backend_reset` event too: that session ends
as failed, and keeps its slot until the release is confirmed. After a
backend was lost with no job open, nothing is delivered: the
session learns of it at its next job, from the refused submit. A
capacity refusal is not a load condition here: one job is in flight and
the dispatch is the bridge's only submitter, so it means that invariant
was broken, and it fails the session the same way however it arrives.
The table's last row covers the bridge's generic refusals, which pass
through as they are and read on a stream as their code does:
`backend_not_ready` as `worker_not_ready`; an identity the backend's
wire cannot carry, a buffer it does not hold and a duplicate job id as
`config_invalid`; a pool without room for an utterance's buffer as
`oom`.
The dispatch never ends a session as `deployment_retired` or
`worker_shutdown`; those are the causes the worker gives a drain.

`test/unit/session_dispatch_test.cpp` runs the contract cases against it
over the fixture job bridge, once for each outcome that backend can be
told to choose (capacity refused by `submit`; a cancelled job that
completes) and once with every callback run before the bridge call that
caused it returns, with results that follow their input.
`test/unit/speech_dispatch_test.cpp` holds what the cases leave to it:
the job requests, the lane's order, each row of the table above, one
release a session, the buffer accounting of every path, and the job's
deadline and the release's bound on the fake clock, each one nanosecond
before and at its instant; a job's end queued as its deadline passes is
taken first, one queued after the check fell due is not, and a release
whose job ends before the backend acknowledges it keeps its bound (the
dispatch's thread held by a gate in the manager's sink or in the bridge
double, which can also hold acknowledgements back).

### The speech dispatch in the worker

In a feature-ON build, a worker with `streaming.enabled` whose
`deployment.model` has the model class `speech` asks the registry for its
session together with its job bridge
(`BackendRegistry::create_session_with_bridge`), loads and primes the
session as any other, constructs a `SpeechJobDispatch` over the bridge, the
worker's buffer manager and the clock the listener's session manager runs
on, and gives it to the stream listener. The listener then admits an `Open`
and the dispatch serves it. Every job identity carries generation 1: a
worker with `streaming.enabled` has no `deployment.generation`.

The model class is part of the condition because a session built with its
bridge loads with speech jobs enabled, and a backend refuses that load for a
model that runs none.

| Deployment | Session | Stream sessions |
| --- | --- | --- |
| `speech` model, backend registered with a bridge factory | with its bridge, loaded with speech jobs | served |
| the same, on a runner that offers no speech jobs | the load with speech jobs is refused | none: `create()` fails with the load's error (`unsupported`; `speech_jobs_not_offered` when the runner lists no such capability) |
| `speech` model, backend without one (`job_bridge_unsupported`) | `create_session`, loaded without jobs | every `Open` refused (`backend_unsupported_capability`) |
| another model class | `create_session`; the bridge factory is not called | every `Open` refused; unary requests served as before |
| the mock session (`deployment.use_mock_session`) | built in process; no factory of the registry is called | every `Open` refused; unary requests served as before |
| `streaming.enabled` false | `create_session`, loaded without jobs | no listener |

Any other failure to build the pair fails `create()` with the factory's
error, and so does a bridge that refuses the dispatch as its event sink,
after the session was loaded; the session goes with the failed worker.

Two things differ from a worker without the dispatch. A speech deployment
with `streaming.enabled` whose runner offers no speech jobs is not built
(the second row); without `streaming.enabled` it loads as before. And a
session built with its bridge takes no tensor request, so a worker that
serves stream sessions answers a unary `/infer` with a failure
(`unsupported`, `tensor_inference_unsupported`).

**A start that fails.** When the stream listener cannot start, for example
because its port is taken, it stops a dispatch it has attached, and a
dispatch is attached to a listener once. `start()` returns the listener's
error, and every later `start()` of that worker returns the same error
without opening either listener: another stream listener would bind and
admit sessions that nothing serves. The worker does not tell a failure
before the attach apart; any failed start of the listener with the dispatch
is final. A worker without a dispatch starts again as before.

**A worker is not restarted.** `stop()` unloads the session and stops the
dispatch, and nothing brings either back: a worker that was stopped is
destroyed, and a new one is built to serve again. Today `start()` after a
`stop()` returns success and opens no listener, because the flag a start
sets is not cleared by a stop (older than the dispatch). By reading, not
run: a `start()` that failed at the HTTP listener, then a `stop()`, then
another `start()` would open the stream listener over the unloaded
session. Refusing a start once the worker was stopped is a change of its
own.

It is **provisional**, as the dispatch is. The worker serves one stream
session at a time. It grants both input kinds whatever the model runs: its
configuration does not say which job classes a deployment has, so a session
whose mode the backend does not run is admitted and fails at its first job:
the bridge refuses a job of a class the backend did not declare when it is
submitted, and the session ends as `backend_unsupported_capability`.
Nothing is refused at negotiation beyond what a job request could not
carry, and one job is in flight. The dispatch's two bounds on a session
(`utterance_backlog_exceeded`, `synthesis_audio_limit_exceeded`) have no
reason of their own in the stream schema: the listener's mapping names such
a failure `internal`, which no test drives through a worker.

**What one client can hold.** With one session at a time, the client whose
session is open holds the worker's only place, and every other `Open` is
refused with `session_count_limit` until that place is free. What bounds
it today, for three clients:

- *One that opens and then only pings* holds the place until the maximum
  session duration, 60 minutes by default: a ping restarts both the idle
  limit and the liveness limit, and no other limit runs for a session that
  owes nothing. Nothing keeps it from opening again when that session
  ends.
- *One that cancels, or goes away, while its job runs* holds the place
  until the backend releases the job: at most the job's deadline and then
  the release's bound, 20 s and 50 ms for a decode of 100 ms of audio and
  35 s for a synthesis or a 30-second utterance. A session that was
  cancelled or has failed has no timer of the session manager's left; the
  dispatch's bound is what ends the wait, by stopping the worker ([A
  stalled speech backend](#a-stalled-speech-backend)).
- *One that reads a chunk every few seconds* never meets the 5-second
  no-progress limit, whose clock restarts at every write that completes.
  Once it has sent a Finalize or closed its sending half, the finalize
  deadline ends its session (120 seconds for text input). Before that the
  idle limit does, and for a reader that also pings only the maximum
  duration. Until then it holds the place and the synthesis result of the
  segment it is reading.

Neither the first client nor the third is bounded by the dispatch: more
than one session per worker is what would keep either from holding the
only place.

**A stalled speech backend.** The worker gives the dispatch its
`on_stall`. When a session's release passes its bound, the worker records
the error in its health, logs `speech backend stalled` (level `error`,
`code` `timeout`, `reason` `release_bound_exceeded`), asks for its own
shutdown (`shutdown requested`, reason `speech backend stalled`) and only
then reads `failed`, all without waiting on the dispatch's thread. The
binary's main thread, which reads the health once a second, or
`serve_forever()` then runs the ordinary stop, and `stop()` returns 70:

1. The stream listener stops. The stalled session keeps its slot, so the
   listener waits out the drain window (`shutdown.drain_deadline`, 5 s by
   default) and cancels the calls still open.
2. The HTTP listener stops and the unary drain runs. A worker that serves
   stream sessions takes no unary inference (it answers `/infer` with
   `unsupported`), so no inference is cut; requests that arrive during
   the stop are refused with 503 `not_ready`, and those in flight finish.
3. The unload waits behind the job for up to the adapter's startup
   timeout (`TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS`, 15 s by default) and
   then ends the sidecar, `SIGTERM` and `SIGKILL` 500 ms later: that is
   the backend's reset.

In `worker.mode: process` the agent's member registry takes a worker whose
process exited out of service and starts its successor on the same port, 1,
2 and 4 s after the stop for the first, second and third restart inside a
sliding five-minute window; a member that stops with three restarts inside
the last five minutes is failed, and the agent reports it (`agent_state:
degraded`, `last_error`; [worker-supervision.md](worker-supervision.md)).
So an input that makes the runner hang every time ends in a failed member
that the agent reports when the stops come close enough: four inside five
minutes, a cycle of about 100 s at most from one stop to the next (the
restart's delay, the successor's load and prime, a client's next job, the
job's deadline and the bound, the stop). A longer cycle restarts the member
after every stall and never fails it. A worker started by hand exits 70
and nothing starts another.

The worst case a client sees, end to end: its own session ends at the
job's deadline (`deadline_missed`; for audio input usually
`finalize_timeout`) or at its cancel; every other `Open` is refused with
`session_count_limit` for up to the job's deadline and the release's bound
(at most 35 s) and the second the main thread may take to notice, then with
`admission_closed` during the drain window, and its connection is refused
from then on: up to the startup timeout and 0.5 s of the unload, the
process's exit, and until the successor has loaded, primed and bound the
port, which this page does not measure.

**Order of construction and of destruction.** The bridge's destructor
detaches its event sink: once it returns no callback runs or follows, and
what the session's unload or destructor ends afterwards reaches nobody. The
sink is the dispatch's, the bridge keeps it, and it holds the input buffer
of every job the backend has not reported released. The worker therefore
keeps this order:

1. Construction: the buffer manager; the session with its bridge, `load`,
   `prime`; the clock; the dispatch, which starts its thread and registers
   its sink with the bridge; the scheduler and the unary components. The
   listener is started by `start()` with the dispatch.
2. `stop()`: the stream listener stops, which stops the dispatch; the
   dispatch releases through the bridge every session still open, and the
   bridge cancels that session's job. After the HTTP drain the worker
   unloads the session, with the stopped dispatch and the bridge still in
   place: the last events of the jobs the unload ends reach the sink, and
   each job's input buffer is given back. An unload cancels nothing and
   waits behind a job the backend is running, up to the adapter's startup
   timeout (`TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS`, 15 seconds by default),
   which bounds each of its load, prime and unload exchanges. That wait
   comes after the drain window, not inside it, and raising the timeout
   for a slow load raises this wait with it.
3. Destruction, after `stop()`: the listener, the dispatch, the bridge,
   the unary components, the session, the buffer manager.

`unload` is reached only from `stop()`, on its caller's thread, never from
a sink callback.

**A drain.** `shutdown()` drains the open session with `worker_shutdown`. A
segment already accepted is still synthesized and delivered inside the
drain window, and the session then closes with that cause. A session whose
job outlasts the window has its call cancelled by the listener's stop and
is released through the bridge once by the dispatch, at the cleanup the
abort asks for or, when its thread has not reached that, at its stop. Its
job is ended by the unload. The dispatch itself gives no session
`worker_shutdown`.

**Known, not changed here: a stop past the drain window sends no
`SessionClosed`.** The client of such a session reads the events already
written and then the end of its call, with status `UNAVAILABLE` or
`CANCELLED` and no terminal event, neither `worker_shutdown` nor any
other. The stream schema says `SessionClosed` is always a session's last
event; the listener cancels the call without writing one. The stop case
asserts what the client reads today. Writing the terminal event before the
cancel is the listener's change.

**What the worker logs.** `stream listener bound` carries
`serves_sessions`. Once the listener has stopped, `speech dispatch stopped`
carries the dispatch's counters as of the last step of its thread:
`jobs_submitted`, `jobs_completed`, `jobs_failed`, `jobs_discarded`,
`releases_refused`, `jobs_past_deadline`, `releases_past_bound` and
`longest_release_wait_ms`. A job the unload ends is in none of
`jobs_completed`, `jobs_failed`, `jobs_discarded` and `releases_refused`,
and a job the bridge refuses when it is submitted is in none of the
counters. No metric is exported for them. A stall adds the `speech backend
stalled` line above.

**Tests.** `test/integration/speech_worker_test.cpp` drives a worker over
gRPC in process: one transcription and one synthesis segment, each against
a scripted sidecar and against the in-tree sidecar runner with its fixture
backend, where every event the client reads is held to its place in the
stream (the `sequence`, what `Ready` grants, each `Accepted` and its
credit, nothing ahead of `SessionClosed` that the session did not owe) and
the runner cases hold that one runner process lives from the load to the
stop and not past it; a client cancel while the backend holds the segment's job (the
session is released once, the next session is served); a lost client while
its utterance is decoded behind a held backend (released once, the job's
audio given back only when the backend lets go of the job), and a lost
client with an utterance open, in both orders described below; a job the
backend takes and refuses, and a session of a mode the backend does not
run; a drain that delivers the segment in flight, and a stop at the end of
the drain window whose unload ends the job, with the job's audio still out
while the unload waits behind the job and back when the stop returns, and
whose client reads no `SessionClosed`; a
start that could not bind and is not retried, and the same start of a
worker without a dispatch, which is; a worker that is never started; the
refused unary request; each row of the table above; and a session
cancelled under a job the backend does not end, which fails the worker
once its release passes the bound, in process (`stop()` returns 70) and as
the worker binary over the runner with a backend that never ends a job
(`test/mocks/worker_held_job_sidecar.py`), which exits 70 after its unload
gave up on the job. Both wait out the provisional figures, about 20 and 36
seconds. Where a session's
release is what cancels its job, the scripted sidecar counts how often
that job was asked to be cancelled: once, and by no `job_cancel` frame.
The file is part of the listener's test binary, so the release and the
address-sanitizer legs run it and the thread-sanitizer leg, which cannot
run a gRPC call, does not. The worker's lines for the dispatch compile only
with the feature, which the clang-tidy job leaves off.

What the tests hold of the order is step 2: destroying the dispatch and the
bridge ahead of the unload gives the job's audio back while the backend
still runs the job, and the stop case fails. The order of step 3 has no
test: a worker's destructor always runs `stop()` first, after which the
listener no longer calls the dispatch and the session's destructor has
nothing left to end, so no order of those three can be told apart from
outside. Step 3 is declared as the registry's header describes a pair
destroyed in order. That the `speech dispatch stopped` line is written
after the listener's stop, not before it, has no case either: no case moves
a counter during that stop.

A call the client cancels or loses with an utterance open can reach the
listener as the end of the client's stream before the cancellation: the
utterance is then ended as a half-close ends it and its decode is submitted
ahead of the release that cancels it. The lost-client case holds both
orders.

## Test surface

- Host-CI mock path: `deployment.use_mock_session = true`. No real
  backend is involved. Used by the F08 integration suite and the
  benchmarks.
- Real-adapter smoke tests are gated by the V01-E05 adapter feature
  flags (`TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`,
  `TP_ENABLE_PYTHON_PYTORCH_SIDECAR`).

See V01-E07-F08 fixtures for canonical end-to-end coverage.
