//! Readiness use case.
//!
//! The readiness answer is computed here, not in the HTTP adapter: the adapter
//! only maps [`ReadinessReport::is_ready`] onto a status code
//! (`deployment-and-threat-model.md` §1.3).

use crate::port::HealthProbe;

/// Result of evaluating every registered [`HealthProbe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessReport {
    /// Probe name and failure reason for each failing dependency.
    failures: Vec<(&'static str, String)>,
    /// Names of the dependencies that answered successfully.
    healthy: Vec<&'static str>,
}

impl ReadinessReport {
    /// Returns `true` when every probe succeeded.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.failures.is_empty()
    }

    /// Returns the failing dependencies with their redacted reasons.
    #[must_use]
    pub fn failures(&self) -> &[(&'static str, String)] {
        &self.failures
    }

    /// Returns the dependencies that answered successfully.
    #[must_use]
    pub fn healthy(&self) -> &[&'static str] {
        &self.healthy
    }
}

/// Evaluates every probe and summarises the outcome.
pub async fn check_readiness(probes: &[&dyn HealthProbe]) -> ReadinessReport {
    let mut failures = Vec::new();
    let mut healthy = Vec::new();
    for probe in probes {
        match probe.check().await {
            Ok(()) => healthy.push(probe.name()),
            Err(reason) => failures.push((probe.name(), reason)),
        }
    }
    ReadinessReport { failures, healthy }
}

#[cfg(test)]
mod tests {
    use super::check_readiness;
    use crate::port::HealthProbe;

    struct StubProbe {
        name: &'static str,
        result: Result<(), String>,
    }

    #[async_trait::async_trait]
    impl HealthProbe for StubProbe {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn check(&self) -> Result<(), String> {
            self.result.clone()
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        // The application layer stays runtime agnostic: tests drive futures
        // with a minimal executor instead of depending on Tokio.
        pollster::block_on(future)
    }

    #[test]
    fn test_readiness_is_true_when_all_probes_pass() {
        let probe = StubProbe {
            name: "database",
            result: Ok(()),
        };
        let report = block_on(check_readiness(&[&probe]));
        assert!(report.is_ready());
        assert_eq!(report.healthy(), ["database"]);
    }

    #[test]
    fn test_readiness_is_false_when_a_probe_fails() {
        let ok = StubProbe {
            name: "database",
            result: Ok(()),
        };
        let failing = StubProbe {
            name: "outbox",
            result: Err("unavailable".to_owned()),
        };
        let report = block_on(check_readiness(&[&ok, &failing]));
        assert!(!report.is_ready());
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.failures()[0].0, "outbox");
    }
}
