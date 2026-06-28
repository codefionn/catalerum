//! OpenTelemetry subscriber/exporter wiring and flush-on-shutdown guard.

use std::collections::HashMap;
use std::future::Future;

use anyhow::{bail, Context};
use base64::Engine as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;

use catalerum_api::{OtelExporterConfig, TelemetryConfig};
use catalerum_llm::{LANGFUSE_LLM_TARGET, OTEL_LLM_TARGET};

/// Owns the SDK providers so pending batches can be flushed at process exit.
#[derive(Default)]
pub(crate) struct TelemetryGuard {
    providers: Vec<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        for provider in &self.providers {
            let _ = provider.shutdown();
        }
    }
}

pub(crate) fn init(config: &TelemetryConfig) -> anyhow::Result<TelemetryGuard> {
    validate(config)?;
    global::set_text_map_propagator(TraceContextPropagator::new());

    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,catalerum=debug"))
    };
    let fmt = || {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr)
    };

    let provider = build_provider(config)?;
    match &provider {
        None => tracing_subscriber::registry()
            .with(filter())
            .with(fmt())
            .try_init()
            .context("initializing tracing subscriber")?,
        Some(provider) => {
            let tracer = provider.tracer("catalerum");
            // `target` is also the per-exporter routing key for the two LLM
            // content policies; keep it even if the layer's default changes.
            let layer = tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_target(true);
            tracing_subscriber::registry()
                .with(filter())
                .with(fmt())
                .with(layer)
                .try_init()
                .context("initializing OTLP tracing subscriber")?;
        }
    }

    Ok(TelemetryGuard {
        providers: provider.into_iter().collect(),
    })
}

fn validate(config: &TelemetryConfig) -> anyhow::Result<()> {
    if config.service_name.trim().is_empty() {
        bail!("[telemetry].service_name must not be empty");
    }
    if !(0.0..=1.0).contains(&config.sample_ratio) || !config.sample_ratio.is_finite() {
        bail!("[telemetry].sample_ratio must be between 0 and 1");
    }
    if config.otlp.enabled && config.otlp.endpoint.trim().is_empty() {
        bail!("[telemetry.otlp].endpoint must not be empty when enabled");
    }
    if config.langfuse.enabled {
        if config.langfuse.endpoint.trim().is_empty() {
            bail!("[telemetry.langfuse].endpoint must not be empty when enabled");
        }
        if config.langfuse.public_key.is_empty() || config.langfuse.secret_key.is_empty() {
            bail!("[telemetry.langfuse].public_key and secret_key are required when enabled");
        }
    }
    Ok(())
}

fn build_otlp_exporter(
    destination: &OtelExporterConfig,
) -> anyhow::Result<opentelemetry_otlp::SpanExporter> {
    let headers = destination
        .headers
        .iter()
        .map(|(key, value)| (key.clone(), value.expose().to_string()))
        .collect();
    build_exporter(destination.endpoint.trim(), headers, "OTLP")
}

fn build_langfuse_exporter(
    config: &TelemetryConfig,
) -> anyhow::Result<opentelemetry_otlp::SpanExporter> {
    let credentials = format!(
        "{}:{}",
        config.langfuse.public_key.expose(),
        config.langfuse.secret_key.expose()
    );
    let authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    );
    let headers = HashMap::from([
        ("Authorization".to_string(), authorization),
        ("x-langfuse-ingestion-version".to_string(), "4".to_string()),
    ]);
    build_exporter(config.langfuse.endpoint.trim(), headers, "Langfuse")
}

fn build_exporter(
    endpoint: &str,
    headers: HashMap<String, String>,
    name: &str,
) -> anyhow::Result<opentelemetry_otlp::SpanExporter> {
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .build()
        .with_context(|| format!("building {name} OTLP/HTTP exporter"))
}

fn build_provider(config: &TelemetryConfig) -> anyhow::Result<Option<SdkTracerProvider>> {
    if !config.otlp.enabled && !config.langfuse.enabled {
        return Ok(None);
    }
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
        .build();
    let mut builder = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(Sampler::TraceIdRatioBased(config.sample_ratio));
    if config.otlp.enabled {
        builder = builder.with_batch_exporter(RoutingExporter {
            inner: build_otlp_exporter(&config.otlp)?,
            excluded_target: LANGFUSE_LLM_TARGET,
        });
    }
    if config.langfuse.enabled {
        builder = builder.with_batch_exporter(RoutingExporter {
            inner: build_langfuse_exporter(config)?,
            excluded_target: OTEL_LLM_TARGET,
        });
    }
    Ok(Some(builder.build()))
}

/// Keeps ordinary spans in every configured backend while routing each
/// content-policy-specific generation span only to its intended destination.
#[derive(Debug)]
struct RoutingExporter {
    inner: opentelemetry_otlp::SpanExporter,
    excluded_target: &'static str,
}

impl SpanExporter for RoutingExporter {
    fn export(&self, mut batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        batch.retain(|span| !has_target(&span.attributes, self.excluded_target));
        async move {
            if batch.is_empty() {
                Ok(())
            } else {
                self.inner.export(batch).await
            }
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

fn has_target(attributes: &[KeyValue], target: &str) -> bool {
    attributes
        .iter()
        .any(|attribute| attribute.key.as_str() == "target" && attribute.value.as_str() == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exporter_routing_uses_the_tracing_target_attribute() {
        let attributes = [
            KeyValue::new("target", LANGFUSE_LLM_TARGET),
            KeyValue::new("gen_ai.request.model", "test"),
        ];
        assert!(has_target(&attributes, LANGFUSE_LLM_TARGET));
        assert!(!has_target(&attributes, OTEL_LLM_TARGET));
        assert!(!has_target(&attributes[1..], LANGFUSE_LLM_TARGET));
    }

    #[test]
    fn validation_requires_langfuse_credentials_and_a_valid_ratio() {
        let mut config = TelemetryConfig {
            sample_ratio: 1.1,
            ..TelemetryConfig::default()
        };
        assert!(validate(&config).is_err());

        config.sample_ratio = 1.0;
        config.langfuse.enabled = true;
        assert!(validate(&config).is_err());
        config.langfuse.public_key = "pk-lf-test".into();
        config.langfuse.secret_key = "sk-lf-test".into();
        assert!(validate(&config).is_ok());
    }
}
