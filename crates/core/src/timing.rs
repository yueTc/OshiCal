//! Low-overhead refresh timing events. No payloads, URLs or credentials.
use std::future::Future;
use std::time::Instant;

pub(crate) struct PhaseTimer {
    phase: &'static str,
    started: Instant,
    outcome: &'static str,
}

impl PhaseTimer {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: Instant::now(),
            outcome: "incomplete",
        }
    }

    pub(crate) fn finish(mut self, success: bool) {
        self.outcome = if success { "ok" } else { "error" };
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        tracing::info!(
            phase = self.phase,
            elapsed_us = self.started.elapsed().as_micros() as u64,
            outcome = self.outcome,
            "刷新阶段耗时"
        );
    }
}

pub(crate) async fn measure<F, T, E>(phase: &'static str, future: F) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    let timer = PhaseTimer::start(phase);
    let result = future.await;
    timer.finish(result.is_ok());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::instrument::WithSubscriber;
    use tracing::{span, Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<HashMap<String, String>>>>);
    struct Fields(HashMap<String, String>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
    }
    impl Subscriber for Capture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(HashMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
        fn enter(&self, _span: &span::Id) {}
        fn exit(&self, _span: &span::Id) {}
    }

    #[tokio::test]
    async fn records_success_and_failure_without_changing_the_result() {
        let capture = Capture::default();
        let success = measure("fixture_success", async {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            Ok::<_, &str>(7)
        })
        .with_subscriber(capture.clone())
        .await;
        let failure = measure("fixture_failure", async { Err::<(), _>("fixture error") })
            .with_subscriber(capture.clone())
            .await;
        assert_eq!(success, Ok(7));
        assert_eq!(failure, Err("fixture error"));
        let records = capture.0.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["phase"], "fixture_success");
        assert_eq!(records[0]["outcome"], "ok");
        assert!(records[0]["elapsed_us"].parse::<u64>().unwrap() >= 1000);
        assert_eq!(records[1]["outcome"], "error");
    }

    #[test]
    fn uncompleted_timer_is_not_reported_as_success() {
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            drop(PhaseTimer::start("fixture_incomplete"));
        });
        let records = capture.0.lock().unwrap();
        assert_eq!(records[0]["outcome"], "incomplete");
    }
}
