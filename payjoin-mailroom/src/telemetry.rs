//! OTLP metrics export over HTTP.

use std::time::Duration;

use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::Resource;

use crate::config::TelemetryConfig;

/// Matches the OTLP default (`OTEL_EXPORTER_OTLP_TIMEOUT`), which the
/// exporter can no longer apply itself once a custom client is injected.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP client injected into the OTLP exporter.
///
/// `opentelemetry-otlp` is built with no HTTP client feature because its
/// bundled clients pull in a second reqwest (0.13) whose rustls links
/// aws-lc-rs, clashing with the ring-only rustls stack used everywhere
/// else in this crate. Without an injected client the exporter builder
/// fails at runtime with `ExporterBuildError::NoHttpClient`.
///
/// A blocking client is used because `PeriodicReader` exports from its own
/// non-tokio thread, where an async reqwest client would panic; blocking
/// reqwest drives an internal runtime instead.
#[derive(Debug, Clone)]
struct ExporterClient(reqwest::blocking::Client);

impl ExporterClient {
    fn new() -> anyhow::Result<Self> {
        // Blocking reqwest clients must not be created on a tokio runtime
        // thread (this is called from async main), so build on a fresh one.
        let client = std::thread::spawn(|| {
            reqwest::blocking::Client::builder().timeout(EXPORT_TIMEOUT).build()
        })
        .join()
        .map_err(|_| anyhow::anyhow!("telemetry HTTP client builder thread panicked"))?
        .context("failed to build telemetry HTTP client")?;
        Ok(Self(client))
    }
}

#[async_trait::async_trait]
impl HttpClient for ExporterClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let request = request.try_into()?;
        let mut response = self.0.execute(request)?.error_for_status()?;
        let headers = std::mem::take(response.headers_mut());
        let mut http_response =
            Response::builder().status(response.status()).body(response.bytes()?)?;
        *http_response.headers_mut() = headers;
        Ok(http_response)
    }
}

/// Build a meter provider exporting OTLP metrics to the configured endpoint.
pub fn build_meter_provider(telemetry: &TelemetryConfig) -> anyhow::Result<SdkMeterProvider> {
    let resource = Resource::builder()
        .with_service_name("payjoin-mailroom")
        .with_attribute(KeyValue::new("operator.domain", telemetry.operator_domain.clone()))
        .build();

    let headers: std::collections::HashMap<String, String> =
        [("Authorization".to_string(), format!("Basic {}", telemetry.auth_token))].into();

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_http_client(ExporterClient::new()?)
        .with_endpoint(format!("{}/v1/metrics", telemetry.endpoint))
        .with_headers(headers)
        .build()
        .context("failed to build OTLP metric exporter")?;

    Ok(SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OTLP exporter resolves its HTTP client when it is built, not
    /// when the crate is compiled: with no client feature enabled on
    /// `opentelemetry-otlp` and no client injected, `build()` returns
    /// `ExporterBuildError::NoHttpClient` and the server panics at
    /// startup while CI stays green. Constructing the provider here makes
    /// that failure a test failure instead.
    #[test]
    fn meter_provider_builds() {
        let config = TelemetryConfig {
            // Discard port: the provider's final flush on drop fails fast
            // with connection refused instead of touching the network.
            endpoint: "http://127.0.0.1:9".to_string(),
            auth_token: "dGVzdDp0ZXN0".to_string(),
            operator_domain: "example.com".to_string(),
        };
        build_meter_provider(&config).expect("OTLP meter provider should build");
    }
}
