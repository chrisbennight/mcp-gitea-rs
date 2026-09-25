//! The production log boundary admits only reviewed diagnostic events.

/// Dependency events may contain complete protocol bodies at any log level.
/// Apply this independently of the operator's verbosity filter so a target
/// directive cannot enable credential-bearing events or spans.
#[must_use]
pub fn safe_metadata(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() == "gitea_server::diagnostics"
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn verbosity_cannot_enable_protocol_events_or_spans() {
        for level in [
            "info",
            "debug",
            "trace",
            "trace,rmcp=trace,reqwest=trace,tower_http=trace",
        ] {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let writer = Buffer(Arc::clone(&bytes));
            let subscriber = tracing_subscriber::registry()
                .with(EnvFilter::new(level))
                .with(tracing_subscriber::filter::filter_fn(super::safe_metadata))
                .with(
                    tracing_subscriber::fmt::layer()
                        .without_time()
                        .with_ansi(false)
                        .with_writer(move || writer.clone()),
                );
            tracing::subscriber::with_default(subscriber, || {
                let span =
                    tracing::info_span!(target: "rmcp::service", "peer", value = "secret-peer");
                let _entered = span.enter();
                tracing::info!(target: "rmcp::service", notification = "secret-notification");
                tracing::debug!(target: "rmcp::service", result = "secret-token");
                tracing::trace!(target: "rmcp::service", request = "secret-input");
                tracing::warn!(target: "rmcp::service", error = "secret-error");
                tracing::error!(target: "reqwest", uri = "secret-uri");
                tracing::info!(target: "tower_http", header = "secret-authorization");
                tracing::info!(target: "gitea_server::diagnostics", code = "safe-event");
            });
            let logs = bytes.lock().unwrap();
            assert!(
                !logs
                    .windows(b"secret-".len())
                    .any(|part| part == b"secret-"),
                "dependency payload reached logs"
            );
            assert!(
                logs.windows(b"safe-event".len())
                    .any(|part| part == b"safe-event")
            );
        }
    }
}
