// SPDX-License-Identifier: Apache-2.0
//
// How a serving worker with a deployment generation answers the agent's
// fence, activate and retire; shared by the unit and integration doubles.

#![allow(dead_code)]

use tensorplate_protocol::worker_control::{
    WorkerControlRequest, WorkerControlResponse, WorkerError, WorkerOp, WorkerStatusOutcome,
};
use tensorplate_protocol::{DomainQuotaBytes, ErrorCode, MemberQuota};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Open,
    Fenced,
    Retiring,
}

pub struct MemberTarget {
    pub admission: Admission,
    /// The stream sessions the built worker serves; none while it loads.
    pub ceiling: Option<u32>,
    /// Built before the agent sent anything but `ledger_status`.
    pub unimplemented: bool,
}

impl MemberTarget {
    /// A worker that has loaded its model, admitting from its start.
    pub fn built(ceiling: u32) -> Self {
        Self {
            admission: Admission::Open,
            ceiling: Some(ceiling),
            unimplemented: false,
        }
    }

    /// The stream sessions a worker started with the serving `config`
    /// serves once built. The worker has one only for a speech backend that
    /// runs stream jobs; the agent renders streaming for no other.
    pub fn ceiling_of(config: &serde_json::Value) -> u32 {
        u32::from(config["streaming"]["enabled"] == serde_json::Value::Bool(true))
    }

    /// The quota a worker with `sessions` stream sessions reports.
    pub fn quota(sessions: u32) -> MemberQuota {
        MemberQuota {
            session_count: sessions,
            domain_bytes: DomainQuotaBytes {
                guest_ram: (sessions > 0).then_some(0),
                ..DomainQuotaBytes::default()
            },
        }
    }

    pub fn answer(&mut self, request: &WorkerControlRequest) -> WorkerControlResponse {
        let responder = request
            .member
            .clone()
            .expect("a runtime request names its member");
        let refuse = |status, code, message: &str, context: &str| {
            let mut error = WorkerError::new(code, message);
            error.context = Some(context.into());
            WorkerControlResponse::answer(request, responder.clone(), status).with_error(error)
        };
        let unsupported = || {
            let message = format!("this worker does not implement {}", request.op);
            let (status, code) = (WorkerStatusOutcome::Unsupported, ErrorCode::Unsupported);
            refuse(status, code, &message, "control_operation_unsupported")
        };
        let ok = WorkerControlResponse::answer(request, responder.clone(), WorkerStatusOutcome::Ok);
        if self.unimplemented {
            return unsupported();
        }
        match request.op {
            WorkerOp::AdmissionFence => {
                if self.admission == Admission::Open {
                    self.admission = Admission::Fenced;
                }
                ok
            }
            WorkerOp::Activate => match (self.admission, self.ceiling) {
                (Admission::Retiring, _) => refuse(
                    WorkerStatusOutcome::Error,
                    ErrorCode::Unavailable,
                    "the member is retiring and admits nothing again",
                    "member_retiring",
                ),
                (_, None) => refuse(
                    WorkerStatusOutcome::NotReady,
                    ErrorCode::NotReady,
                    "the member is still loading",
                    "member_loading",
                ),
                (_, Some(ceiling)) => {
                    self.admission = Admission::Open;
                    ok.with_quota(Self::quota(ceiling))
                }
            },
            WorkerOp::Retire => {
                self.admission = Admission::Retiring;
                ok
            }
            _ => unsupported(),
        }
    }
}
