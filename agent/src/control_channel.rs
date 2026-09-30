// SPDX-License-Identifier: Apache-2.0

//! Agent-owned worker socket and independent, bounded control polling.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tensorplate_protocol::worker_control::{
    encode_frame, LedgerStatus, MemberRef, PressureDirective, WorkerControlRequest,
    WorkerControlResponse, WorkerStatusOutcome, WORKER_CONTROL_MAX_FRAME_BYTES,
};
use tensorplate_protocol::{decode_with_version_check, MemberQuota, ValidatePayload};

use crate::error::ControlChannelError;

type Result<T> = std::result::Result<T, ControlChannelError>;
const POLL_PERIOD: Duration = Duration::from_secs(1);
const FAILURE_AFTER: Duration = Duration::from_secs(10);
const IO_QUANTUM: Duration = Duration::from_millis(2);
const QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactState {
    InContact,
    OutOfContact,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactEvent {
    OutOfContact,
    ContactRestored,
    MemberFailed,
}

/// Pure monitor; times are elapsed from channel creation, not wall-clock time.
#[derive(Debug)]
pub struct ContactMonitor {
    state: ContactState,
    missed: u32,
    last_good: Duration,
}

impl Default for ContactMonitor {
    fn default() -> Self {
        Self {
            state: ContactState::InContact,
            missed: 0,
            last_good: Duration::ZERO,
        }
    }
}

impl ContactMonitor {
    pub fn state(&self) -> ContactState {
        self.state
    }
    pub fn missed_polls(&self) -> u32 {
        self.missed
    }

    /// A valid, paired ledger answer proves contact even when it reports an error.
    pub fn observe(&mut self, elapsed: Duration, success: bool) -> Option<ContactEvent> {
        if self.state == ContactState::Failed {
            return None;
        }
        let old = self.state;
        if elapsed.saturating_sub(self.last_good) >= FAILURE_AFTER {
            self.state = ContactState::Failed;
        } else if success {
            self.last_good = elapsed;
            self.missed = 0;
            self.state = ContactState::InContact;
        } else {
            self.missed = self.missed.saturating_add(1);
            if self.missed >= 3 {
                self.state = ContactState::OutOfContact;
            }
        }
        if old == self.state {
            return None;
        }
        Some(match self.state {
            ContactState::InContact => ContactEvent::ContactRestored,
            ContactState::OutOfContact => ContactEvent::OutOfContact,
            ContactState::Failed => ContactEvent::MemberFailed,
        })
    }
}

/// Requests cannot name another member, choose reusable IDs, or send legacy stages.
#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    AdmissionFence {
        transaction_id: String,
    },
    Activate {
        transaction_id: String,
    },
    Retire {
        transaction_id: String,
        drain_timeout_ms: u64,
    },
    QuotaAssign(MemberQuota),
    LedgerStatus,
    PressureDirective(PressureDirective),
}

impl RuntimeCommand {
    fn request(self, member: MemberRef, id: String) -> WorkerControlRequest {
        match self {
            Self::AdmissionFence { transaction_id } => {
                WorkerControlRequest::admission_fence(transaction_id, id, member)
            }
            Self::Activate { transaction_id } => {
                WorkerControlRequest::activate(transaction_id, id, member)
            }
            Self::Retire {
                transaction_id,
                drain_timeout_ms,
            } => WorkerControlRequest::retire(transaction_id, id, member, drain_timeout_ms),
            Self::QuotaAssign(quota) => WorkerControlRequest::quota_assign(id, member, quota),
            Self::LedgerStatus => WorkerControlRequest::ledger_status(id, member),
            Self::PressureDirective(pressure) => {
                WorkerControlRequest::pressure_directive(id, member, pressure)
            }
        }
    }
}

/// A bounded watch value. Slow readers see the latest state and transition revision.
#[derive(Clone, Debug)]
pub struct ChannelSnapshot {
    pub member: MemberRef,
    pub contact: ContactState,
    pub missed_polls: u32,
    pub ledger: Option<LedgerStatus>,
    pub ledger_received_at: Option<Duration>,
    pub last_error: Option<ControlChannelError>,
    pub event_revision: u64,
    pub last_event: Option<ContactEvent>,
}

struct Queued {
    command: RuntimeCommand,
    expires: Instant,
    reply: mpsc::SyncSender<Result<WorkerControlResponse>>,
}

/// Owns no process: the registry handles kill/reap/restart after `MemberFailed`.
pub struct ControlChannel {
    member: MemberRef,
    requests: mpsc::SyncSender<Queued>,
    snapshot: Arc<Mutex<ChannelSnapshot>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ControlChannel {
    pub fn start(stream: UnixStream, member: MemberRef) -> Result<Self> {
        RuntimeCommand::LedgerStatus
            .request(member.clone(), "probe".into())
            .validate_payload()
            .map_err(|_| ControlChannelError::InvalidRequest)?;
        stream
            .set_nonblocking(true)
            .map_err(|e| ControlChannelError::Io(e.kind()))?;
        let (requests, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let snapshot = Arc::new(Mutex::new(ChannelSnapshot {
            member: member.clone(),
            contact: ContactState::InContact,
            missed_polls: 0,
            ledger: None,
            ledger_received_at: None,
            last_error: None,
            event_revision: 0,
            last_event: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&snapshot);
        let stopping = Arc::clone(&stop);
        let peer = member.clone();
        let thread = thread::Builder::new()
            .name("worker-control".into())
            .spawn(move || run(stream, &peer, &receiver, &shared, &stopping))
            .map_err(|e| ControlChannelError::Io(e.kind()))?;
        Ok(Self {
            member,
            requests,
            snapshot,
            stop,
            thread: Some(thread),
        })
    }

    /// One second includes queue time. Dropping the receiver does not cancel an issued operation.
    /// A deadline means its outcome is unknown; the client never automatically reapplies it.
    pub fn submit(
        &self,
        command: RuntimeCommand,
    ) -> Result<mpsc::Receiver<Result<WorkerControlResponse>>> {
        command
            .clone()
            .request(self.member.clone(), "probe".into())
            .validate_payload()
            .map_err(|_| ControlChannelError::InvalidRequest)?;
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(Queued {
                command,
                expires: Instant::now() + POLL_PERIOD,
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ControlChannelError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ControlChannelError::Stopped,
            })?;
        Ok(result)
    }

    pub fn snapshot(&self) -> ChannelSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for ControlChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Transport {
    stream: UnixStream,
    buffer: Vec<u8>,
    open: bool,
    sequence: u64,
}

impl Transport {
    fn exchange(
        &mut self,
        command: RuntimeCommand,
        member: &MemberRef,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<WorkerControlResponse> {
        if !self.open {
            return Err(ControlChannelError::Closed);
        }
        if Instant::now() >= deadline {
            return Err(ControlChannelError::Deadline);
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ControlChannelError::Stopped)?;
        let request = command
            .request(member.clone(), format!("ctl-{:016}", self.sequence))
            .with_timeout_ms(
                u64::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap_or(1000)
                .clamp(1, 1000),
            );
        let result = self.round_trip(&request, deadline, stop);
        if matches!(result, Err(ref e) if *e != ControlChannelError::Deadline) {
            self.open = false;
            let _ = self.stream.shutdown(Shutdown::Both);
        }
        result
    }

    fn round_trip(
        &mut self,
        request: &WorkerControlRequest,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<WorkerControlResponse> {
        let frame = encode_frame(request).map_err(|_| ControlChannelError::Protocol)?;
        let mut written = 0;
        while written < frame.len() {
            if let Err(error) = ready(deadline, stop) {
                return Err(if written == 0 {
                    error
                } else {
                    ControlChannelError::PartialWrite
                });
            }
            match self.stream.write(&frame[written..]) {
                Ok(0) => return Err(ControlChannelError::Closed),
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => pause(deadline),
                Err(e) => return Err(ControlChannelError::Io(e.kind())),
            }
        }
        loop {
            let bytes = self.read_frame(deadline, stop)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| ControlChannelError::Protocol)?;
            let response: WorkerControlResponse =
                decode_with_version_check(text).map_err(|_| ControlChannelError::Protocol)?;
            if response.member != request.member
                || response.status == WorkerStatusOutcome::MemberMismatch
            {
                return Err(ControlChannelError::MemberMismatch);
            }
            if response.correlation_id != request.correlation_id {
                // Expired operations are never retried, even after a subsequent ledger poll.
                let old = response
                    .correlation_id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("ctl-"))
                    .and_then(|id| id.parse::<u64>().ok());
                if old.is_some_and(|id| id > 0 && id < self.sequence) {
                    continue;
                }
                return Err(ControlChannelError::Protocol);
            }
            response
                .answers(request)
                .map_err(|_| ControlChannelError::Protocol)?;
            return Ok(response);
        }
    }

    fn read_frame(&mut self, deadline: Instant, stop: &AtomicBool) -> Result<Vec<u8>> {
        loop {
            ready(deadline, stop)?;
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                return Ok(self.buffer.drain(..=end).collect());
            }
            if self.buffer.len() >= WORKER_CONTROL_MAX_FRAME_BYTES {
                return Err(ControlChannelError::FrameTooLarge);
            }
            let mut chunk = [0; 4096];
            let capacity = chunk
                .len()
                .min(WORKER_CONTROL_MAX_FRAME_BYTES - self.buffer.len());
            match self.stream.read(&mut chunk[..capacity]) {
                Ok(0) if self.buffer.is_empty() => return Err(ControlChannelError::Closed),
                Ok(0) => return Err(ControlChannelError::Protocol),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => pause(deadline),
                Err(e) => return Err(ControlChannelError::Io(e.kind())),
            }
        }
    }
}

fn ready(deadline: Instant, stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(ControlChannelError::Stopped);
    }
    if Instant::now() >= deadline {
        return Err(ControlChannelError::Deadline);
    }
    Ok(())
}

fn pause(deadline: Instant) {
    thread::sleep(IO_QUANTUM.min(deadline.saturating_duration_since(Instant::now())));
}

fn run(
    stream: UnixStream,
    member: &MemberRef,
    receiver: &mpsc::Receiver<Queued>,
    shared: &Mutex<ChannelSnapshot>,
    stop: &AtomicBool,
) {
    let start = Instant::now();
    let mut next_poll = start;
    let mut monitor = ContactMonitor::default();
    let mut transport = Transport {
        stream,
        buffer: Vec::new(),
        open: true,
        sequence: 0,
    };
    while !stop.load(Ordering::Acquire) && monitor.state() != ContactState::Failed {
        let now = Instant::now();
        let loss_deadline = start + monitor.last_good + FAILURE_AFTER;
        if now >= next_poll || now >= loss_deadline {
            // Requests that cannot start before this poll get explicit backpressure.
            for queued in receiver.try_iter().take(QUEUE_CAPACITY) {
                let error = if now >= queued.expires {
                    ControlChannelError::Deadline
                } else {
                    ControlChannelError::QueueFull
                };
                let _ = queued.reply.try_send(Err(error));
            }
            let deadline = (next_poll + POLL_PERIOD).min(loss_deadline);
            let answer = transport.exchange(RuntimeCommand::LedgerStatus, member, deadline, stop);
            let ledger = answer
                .as_ref()
                .ok()
                .filter(|r| r.status == WorkerStatusOutcome::Ok)
                .and_then(|r| r.ledger.clone());
            if answer.is_err() {
                while ready(deadline, stop).is_ok() {
                    pause(deadline);
                }
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            let elapsed = start.elapsed();
            let event = monitor.observe(elapsed, answer.is_ok());
            let mut snapshot = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.contact = monitor.state();
            snapshot.missed_polls = monitor.missed_polls();
            snapshot.last_error = answer.err().or_else(|| {
                if ledger.is_none() {
                    Some(ControlChannelError::Rejected)
                } else {
                    None
                }
            });
            if let Some(ledger) = ledger {
                snapshot.ledger = Some(ledger);
                snapshot.ledger_received_at = Some(elapsed);
            }
            if let Some(event) = event {
                snapshot.event_revision = snapshot.event_revision.saturating_add(1);
                snapshot.last_event = Some(event);
            }
            next_poll += POLL_PERIOD;
        } else {
            match receiver.recv_timeout(
                (next_poll - now)
                    .min(loss_deadline - now)
                    .min(Duration::from_millis(20)),
            ) {
                Ok(queued) => {
                    let deadline = queued.expires.min(next_poll).min(loss_deadline);
                    let answer = transport.exchange(queued.command, member, deadline, stop);
                    let _ = queued.reply.try_send(answer);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    let _ = transport.stream.shutdown(Shutdown::Both);
    for queued in receiver.try_iter().take(QUEUE_CAPACITY) {
        let _ = queued.reply.try_send(Err(ControlChannelError::Stopped));
    }
}

#[cfg(test)]
mod tests;
