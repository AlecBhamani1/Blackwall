//! Built-in public-web permission is separate from generic network and owner HTTP approvals.
use super::*;

impl Approvals {
    /// Called only by the built-in fetcher, for each validated URL including redirects.
    pub(crate) async fn request_web(
        &self,
        request_id: &str,
        host: &str,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        let snapshot = self.policy(request_id)?;
        // Refresh only owner rules so revocation (also from another process) applies to the
        // next web request, while trusted project settings remain the run's original snapshot.
        let policy = match snapshot {
            Some(policy) => Some(policy.refresh_web_policy().await),
            None => None,
        };
        let verdict = policy.as_ref().map(|policy| policy.evaluate_web(host));
        let reusable = policy
            .as_ref()
            .zip(verdict)
            .is_none_or(|(policy, verdict)| policy.offers_web_permission(verdict));
        let run_allowed = reusable
            && self
                .0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?
                .web_runs
                .contains(request_id);
        let mut saved = false;
        let mut permission = "once";
        let allowed = match verdict.map(|verdict| verdict.decision) {
            Some(Decision::Allow) => {
                permission = "policy";
                true
            }
            Some(Decision::Deny) => false,
            _ if run_allowed => {
                permission = "run";
                true
            }
            _ => {
                let choices = reusable.then_some(if policy.is_some() {
                    WebAccessApproval::Persistent
                } else {
                    WebAccessApproval::Run
                });
                let detail = match policy.as_ref().and_then(|policy| policy.error.as_ref()) {
                    Some(error) => format!("{error}\n\n{detail}"),
                    None if !reusable => format!(
                        "Your rules require approval for this request. Reusable web permission does not override them.\n\n{detail}"
                    ),
                    None => detail,
                };
                let answer = self
                    .ask(
                        request_id,
                        ApprovalKind::Network,
                        None,
                        detail,
                        emit,
                        false,
                        false,
                        false,
                        choices,
                    )
                    .await?;
                match answer {
                    ApprovalDecision::AllowWeb => {
                        let Some(policy) = &policy else {
                            return Err(
                                "Web permission cannot be saved without owner settings.".into()
                            );
                        };
                        let directory = policy.scope().0.to_path_buf();
                        let result = tokio::task::spawn_blocking(move || {
                            crate::policy::allow_web(&directory)
                        })
                        .await
                        .map_err(|_| "Web permission could not be saved.")?;
                        if let Err(error) = result {
                            if let Some(verdict) = verdict {
                                policy.record_web(host, verdict, false, false, "once").await;
                            }
                            return Err(format!("{error} The web request was not run."));
                        }
                        saved = true;
                        permission = "saved";
                        true
                    }
                    ApprovalDecision::AllowWebForRun => {
                        self.0
                            .lock()
                            .map_err(|_| "The approval registry is unavailable.")?
                            .web_runs
                            .insert(request_id.into());
                        permission = "run";
                        true
                    }
                    ApprovalDecision::Deny => false,
                    _ => true,
                }
            }
        };
        if let Some((policy, verdict)) = policy.as_ref().zip(verdict) {
            policy
                .record_web(host, verdict, allowed, saved, permission)
                .await;
        }
        Ok(allowed)
    }
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
