use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use llm_harness_runtime_platform::tracer::{SpanEvent, TraceExporter};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::SpanExporter as _;
use tokio::sync::Mutex;

pub struct OtlpConfig {
    pub endpoint: String,
    pub service_name: String,
    pub timeout: Duration,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318/v1/traces".to_string(),
            service_name: "llm-harness".to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

pub struct OtlpTraceExporter {
    inner: SpanExporter,
    mutex: Mutex<()>,
}

impl OtlpTraceExporter {
    pub fn new(config: &OtlpConfig) -> Result<Self> {
        let mut exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(&config.endpoint)
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(config.timeout)
            .build()?;
        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(config.service_name.clone())
            .build();
        exporter.set_resource(&resource);
        Ok(Self {
            inner: exporter,
            mutex: Mutex::new(()),
        })
    }
}

impl TraceExporter for OtlpTraceExporter {
    fn export<'a>(&'a self, events: &'a [SpanEvent]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _guard = self.mutex.lock().await;
            let batch: Vec<_> = events.iter().map(crate::convert::convert_span).collect();
            let _ = self.inner.export(batch).await;
            Ok(())
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let _ = self.inner.force_flush();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_values() {
        let c = OtlpConfig::default();
        assert_eq!(c.endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(c.service_name, "llm-harness");
        assert_eq!(c.timeout, Duration::from_secs(10));
    }

    #[test]
    fn exporter_constructs_with_default_config() {
        let config = OtlpConfig::default();
        let exporter = OtlpTraceExporter::new(&config);
        assert!(
            exporter.is_ok(),
            "construction should succeed even without a running collector"
        );
    }
}
