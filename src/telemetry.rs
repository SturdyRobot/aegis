//! Tracing setup, plus an optional OpenTelemetry (OTLP) export layer.
//!
//! Everything in Aegis is already instrumented with `tracing` spans; this module
//! decides where those spans go. By default they go to the human `fmt` logger and
//! nothing else — no OpenTelemetry code is even compiled in.
//!
//! Build with `--features otel` and set `OTEL_EXPORTER_OTLP_ENDPOINT` (e.g.
//! `http://localhost:4317`) to *also* export spans to an OTLP collector
//! (Jaeger/Datadog/Honeycomb). If the feature is off, or the env var is unset, or
//! the exporter fails to build, the CLI runs exactly as before — the fmt logger
//! stays up and OTLP silently stays down.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Guard returned by [`init`]. Hold it for the lifetime of the program; on drop it
/// flushes any pending OTLP spans. A no-op unless OTLP export is actually active.
#[must_use = "hold the TelemetryGuard until program exit so spans get flushed"]
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
}

/// Install the global subscriber. Always wires the `fmt` logger; additionally
/// wires an OTLP layer when the `otel` feature is on and an endpoint is configured.
pub fn init() -> TelemetryGuard {
    // Logs go to stderr, never stdout: stdout is reserved for data (`--json`
    // output, and the `mcp` server's JSON-RPC stream, which a stray log line
    // would corrupt).
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);

    #[cfg(feature = "otel")]
    {
        // Only stand up the exporter when the operator explicitly points us at a
        // collector — presence of the standard OTLP endpoint env var is the switch.
        match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
            Ok(endpoint) if !endpoint.trim().is_empty() => match otel::build_provider(&endpoint) {
                Ok(provider) => {
                    use opentelemetry::trace::TracerProvider as _;
                    let layer =
                        tracing_opentelemetry::layer().with_tracer(provider.tracer("aegis"));
                    tracing_subscriber::registry()
                        .with(env_filter())
                        .with(fmt_layer)
                        .with(layer)
                        .init();
                    tracing::debug!(endpoint, "OTLP span export enabled");
                    return TelemetryGuard {
                        provider: Some(provider),
                    };
                }
                // Degrade silently to fmt-only; one line to stderr, never a panic.
                Err(e) => eprintln!("aegis: OTLP export disabled ({e})"),
            },
            _ => {}
        }
    }

    tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt_layer)
        .init();
    TelemetryGuard {
        #[cfg(feature = "otel")]
        provider: None,
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            // Flush the batch processor so in-flight spans reach the collector
            // before the process exits.
            let _ = provider.shutdown();
        }
    }
}

#[cfg(feature = "otel")]
mod otel {
    use anyhow::Context;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use opentelemetry_sdk::Resource;

    /// Build a batch OTLP tracer provider pointed at `endpoint`. Honors
    /// `OTEL_SERVICE_NAME` (default `"aegis"`).
    pub fn build_provider(endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aegis".to_string());

        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("building OTLP span exporter")?;

        let resource = Resource::builder().with_service_name(service_name).build();

        Ok(SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build())
    }
}
