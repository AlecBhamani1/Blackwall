//! Exact-action approvals tied to a live run. Dropping a wait invalidates its decision handle.
use crate::protocol::{AgentEvent, ApprovalDecision, ApprovalKind, ResolveApprovalRequest};
use rand::{rngs::OsRng, RngCore};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

struct Pending {
    request_id: String,
    sender: oneshot::Sender<ApprovalDecision>,
}
#[derive(Default)]
struct Registry {
    pending: HashMap<String, Pending>,
    allowed: HashSet<(String, String)>,
}
#[derive(Clone, Default)]
pub struct Approvals(Arc<Mutex<Registry>>);
struct Waiting {
    approvals: Approvals,
    id: String,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        if let Ok(mut state) = self.approvals.0.lock() {
            state.pending.remove(&self.id);
        }
    }
}
impl Approvals {
    pub async fn request(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        let (sender, receiver) = oneshot::channel();
        let id = format!("approval_{:016x}", OsRng.next_u64());
        {
            let mut state = self
                .0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?;
            if state
                .allowed
                .contains(&(request_id.to_owned(), detail.clone()))
            {
                return Ok(true);
            }
            if state.pending.len() >= 16 {
                return Err("Too many approvals are waiting.".into());
            }
            state.pending.insert(
                id.clone(),
                Pending {
                    request_id: request_id.into(),
                    sender,
                },
            );
        }
        let _guard = Waiting {
            approvals: self.clone(),
            id: id.clone(),
        };
        emit(AgentEvent::ApprovalRequest {
            request_id: request_id.into(),
            approval_id: id,
            kind,
            detail: detail.clone(),
        });
        let decision = tokio::time::timeout(Duration::from_secs(10 * 60), receiver)
            .await
            .map_err(|_| "Approval expired. Ask Blackwall to try again.")?
            .map_err(|_| "Approval was cancelled.")?;
        if decision == ApprovalDecision::AlwaysAllow {
            self.0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?
                .allowed
                .insert((request_id.into(), detail));
        }
        Ok(decision != ApprovalDecision::Deny)
    }
    pub fn resolve(&self, request: ResolveApprovalRequest) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?;
        if !state
            .pending
            .get(&request.approval_id)
            .is_some_and(|pending| pending.request_id == request.request_id)
        {
            return Err("This approval is no longer active.".into());
        }
        if let Some(pending) = state.pending.remove(&request.approval_id) {
            pending
                .sender
                .send(request.decision)
                .map_err(|_| "This approval is no longer active.")?;
        }
        Ok(())
    }
    pub fn clear_run(&self, request_id: &str) {
        if let Ok(mut state) = self.0.lock() {
            state
                .pending
                .retain(|_, pending| pending.request_id != request_id);
            state.allowed.retain(|(id, _)| id != request_id);
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wrong_run_cannot_authorize_and_dropped_wait_is_revoked() {
        let approvals = Approvals::default();
        let events = Arc::new(Mutex::new(vec![]));
        let captured = events.clone();
        let emit = move |e| captured.lock().unwrap().push(e);
        let wait = approvals.request("run1", ApprovalKind::Exec, "echo test".into(), &emit);
        tokio::pin!(wait);
        tokio::select! { _=&mut wait => panic!("must await decision"), _=tokio::time::sleep(Duration::from_millis(1)) => {} }
        let event = events.lock().unwrap()[0].clone();
        if let AgentEvent::ApprovalRequest { approval_id, .. } = event {
            assert!(approvals
                .resolve(ResolveApprovalRequest {
                    request_id: "wrong".into(),
                    approval_id: approval_id.clone(),
                    decision: ApprovalDecision::Allow
                })
                .is_err());
            approvals.clear_run("run1");
            assert!(approvals
                .resolve(ResolveApprovalRequest {
                    request_id: "run1".into(),
                    approval_id,
                    decision: ApprovalDecision::Allow
                })
                .is_err());
        }
    }
    #[tokio::test]
    async fn remembered_permission_is_exact_and_expires_with_run() {
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let allow = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::AlwaysAllow,
                    })
                    .unwrap();
            }
        };
        assert!(approvals
            .request("one", ApprovalKind::Exec, "exact command".into(), &allow)
            .await
            .unwrap());
        assert!(approvals
            .request(
                "one",
                ApprovalKind::Exec,
                "exact command".into(),
                &|_| panic!("identical action should use the run permission")
            )
            .await
            .unwrap());
        let decisions = approvals.clone();
        let deny = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Deny,
                    })
                    .unwrap();
            }
        };
        assert!(!approvals
            .request("one", ApprovalKind::Exec, "different command".into(), &deny)
            .await
            .unwrap());
        approvals.clear_run("one");
        assert!(!approvals
            .request("two", ApprovalKind::Exec, "exact command".into(), &deny)
            .await
            .unwrap());
    }
}
