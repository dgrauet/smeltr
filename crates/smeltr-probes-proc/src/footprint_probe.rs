//! Sonde d'empreinte mémoire de l'arbre du processus tracé.
//!
//! Contrairement à [`crate::probe::ProcProbe`], qui échantillonne le top 50
//! système en forkant `/usr/bin/top`, celle-ci ne cible que le processus
//! tracé et ses descendants, et n'utilise que des syscalls.

use crate::footprint::{descendants_of, list_processes, read_footprint};
use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cadence par défaut, alignée sur celle de `ProcProbe`.
const DEFAULT_PERIOD: Duration = Duration::from_secs(2);

/// Échantillonne l'arbre enraciné en `root_pid`. Les processus disparus
/// entre l'énumération et la lecture sont simplement omis.
pub fn sample_tree(root_pid: u32) -> Vec<Payload> {
    let all = list_processes();
    descendants_of(root_pid, &all)
        .into_iter()
        .filter_map(|node| {
            let f = read_footprint(node.pid)?;
            Some(Payload::ProcFootprint {
                pid: node.pid,
                name: node.name,
                phys_footprint_bytes: f.phys_bytes,
                lifetime_max_phys_footprint_bytes: f.lifetime_max_bytes,
                is_traced_root: node.pid == root_pid,
            })
        })
        .collect()
}

pub struct FootprintProbe {
    pid: u32,
    period: Duration,
}

impl FootprintProbe {
    pub fn new(pid: u32, period: Duration) -> Self {
        Self { pid, period }
    }

    /// Cadence par défaut, surchargeable par `SMELTR_FOOTPRINT_PERIOD_MS`.
    /// Une valeur illisible ou nulle retombe sur la valeur par défaut.
    pub fn default_period() -> Duration {
        std::env::var("SMELTR_FOOTPRINT_PERIOD_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PERIOD)
    }
}

#[async_trait]
impl Probe for FootprintProbe {
    fn name(&self) -> &'static str {
        "footprint"
    }

    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        if !cfg!(target_os = "macos") {
            return Err(ProbeError::Unavailable(
                "footprint probe requires macOS".into(),
            ));
        }
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            // Un arbre vide signifie que le processus tracé a disparu : on
            // continue à ticker, c'est au daemon de détacher la sonde.
            for payload in sample_tree(self.pid) {
                sink.emit(Source::Proc, Some(self.pid), payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Payload;

    #[test]
    fn default_period_is_two_seconds() {
        assert_eq!(FootprintProbe::default_period(), Duration::from_secs(2));
    }

    #[test]
    #[serial_test::serial]
    fn env_var_overrides_period() {
        std::env::set_var("SMELTR_FOOTPRINT_PERIOD_MS", "500");
        assert_eq!(FootprintProbe::default_period(), Duration::from_millis(500));
        std::env::remove_var("SMELTR_FOOTPRINT_PERIOD_MS");
    }

    #[test]
    #[serial_test::serial]
    fn invalid_env_var_falls_back_to_default() {
        std::env::set_var("SMELTR_FOOTPRINT_PERIOD_MS", "pas-un-nombre");
        assert_eq!(FootprintProbe::default_period(), Duration::from_secs(2));
        std::env::remove_var("SMELTR_FOOTPRINT_PERIOD_MS");
    }

    #[test]
    fn sample_tree_marks_root_and_reads_self() {
        let me = std::process::id();
        let payloads = sample_tree(me);
        assert!(!payloads.is_empty(), "au moins le processus courant");
        let Payload::ProcFootprint {
            pid,
            is_traced_root,
            phys_footprint_bytes,
            ..
        } = &payloads[0]
        else {
            panic!("attendu ProcFootprint, eu {:?}", payloads[0]);
        };
        assert_eq!(*pid, me);
        assert!(*is_traced_root, "la racine doit être marquée");
        assert!(*phys_footprint_bytes > 0);
    }

    #[test]
    fn sample_tree_of_dead_pid_is_empty() {
        assert!(sample_tree(0).is_empty());
    }
}
