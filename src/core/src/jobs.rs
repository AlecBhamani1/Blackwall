//! Per-request cancellation shared by downloads and model runs.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Default)]
struct Registry {
    active: HashMap<String, watch::Sender<bool>>,
    cancelled: VecDeque<String>,
}
#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<Registry>>);
pub struct Job {
    id: String,
    owner: Jobs,
    signal: watch::Receiver<bool>,
}
impl Jobs {
    pub fn cancel_all(&self) {
        if let Ok(registry) = self.0.lock() {
            for sender in registry.active.values() {
                let _ = sender.send(true);
            }
        }
    }
    pub fn start(&self, id: &str) -> Result<Job, String> {
        if id.is_empty() || id.len() > 128 {
            return Err("Invalid request identifier.".into());
        }
        let mut registry = self
            .0
            .lock()
            .map_err(|_| "The request registry is unavailable.")?;
        if registry.active.contains_key(id) || registry.active.len() >= 8 {
            return Err("Another request is already using this slot.".into());
        }
        if registry.cancelled.iter().any(|item| item == id) {
            return Err("Request stopped.".into());
        }
        let (sender, signal) = watch::channel(false);
        registry.active.insert(id.into(), sender);
        Ok(Job {
            id: id.into(),
            owner: self.clone(),
            signal,
        })
    }
    pub fn cancel(&self, id: &str) -> Result<(), String> {
        if id.is_empty() || id.len() > 128 {
            return Err("Invalid request identifier.".into());
        }
        let mut registry = self
            .0
            .lock()
            .map_err(|_| "The request registry is unavailable.")?;
        if let Some(sender) = registry.active.get(id) {
            let _ = sender.send(true);
        }
        if !registry.cancelled.iter().any(|item| item == id) {
            registry.cancelled.push_back(id.into());
        }
        while registry.cancelled.len() > 64 {
            registry.cancelled.pop_front();
        }
        Ok(())
    }
}
impl Job {
    pub async fn cancelled(&mut self) {
        if *self.signal.borrow() {
            return;
        }
        let _ = self.signal.changed().await;
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.owner.0.lock() {
            registry.active.remove(&self.id);
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancel_before_and_during_registration() {
        let jobs = Jobs::default();
        jobs.cancel("early").unwrap();
        assert!(jobs.start("early").is_err());
        let mut job = jobs.start("active").unwrap();
        assert!(jobs.start("active").is_err());
        jobs.cancel("active").unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), job.cancelled())
            .await
            .unwrap();
    }
}
