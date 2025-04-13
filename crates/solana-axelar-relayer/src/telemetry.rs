use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry::{global, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{ExportConfig, ExporterBuildError, LogExporter, MetricExporter, SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::{trace as sdktrace, Resource};
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

use crate::{get_service_name, get_service_version};

// Aliases for readability
type TracerProviderResult = Result<sdktrace::SdkTracerProvider, ExporterBuildError>;
type MeterProviderResult = Result<opentelemetry_sdk::metrics::SdkMeterProvider, ExporterBuildError>;
type LoggerProviderResult = Result<SdkLoggerProvider, ExporterBuildError>;

pub(crate) fn init_telemetry(exporter_endpoint: Option<String>) -> eyre::Result<()> {
    if let Some(endpoint) = exporter_endpoint {
        let endpoint = endpoint.as_str(); // Convert once and reuse

        let tracer = init_tracer_provider(endpoint)?;
        let metrics = init_metrics(endpoint)?;
        let logs = init_logs(endpoint)?;

        global::set_tracer_provider(tracer);
        global::set_meter_provider(metrics);

        let logs_layer = OpenTelemetryTracingBridge::new(&logs);

        setup_subscriber(Some(logs_layer))?;
    } else {
        setup_subscriber(None)?;
    }

    Ok(())
}

fn init_tracer_provider(exporter_endpoint: &str) -> TracerProviderResult {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(exporter_endpoint)
        .build()?;
    Ok(SdkTracerProvider::builder()
        .with_resource(resources())
        .with_batch_exporter(exporter)
        .build())
}

fn init_metrics(exporter_endpoint: &str) -> MeterProviderResult {
    let export_config = ExportConfig {
        endpoint: Some(exporter_endpoint.to_owned()),
        ..ExportConfig::default()
    };

    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_export_config(export_config)
        .build()?;

    Ok(SdkMeterProvider::builder()
        .with_resource(resources())
        .with_periodic_exporter(exporter)
        .build())
}

fn init_logs(exporter_endpoint: &str) -> LoggerProviderResult {
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(exporter_endpoint)
        .build()?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resources())
        .with_batch_exporter(exporter)
        .build())
}

fn resources() -> Resource {
    Resource::builder()
        .with_attributes(vec![
        KeyValue::new(
            opentelemetry_semantic_conventions::resource::SERVICE_NAME,
            get_service_name(),
        ),
        KeyValue::new(
            opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
            get_service_version(),
        ),
    ]).build()
}

fn setup_subscriber(
    logs_layer: Option<OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger>>,
) -> eyre::Result<()> {
    let subscriber = Registry::default();
    let filter = EnvFilter::new("relayer_engine=info")
        .add_directive("solana_axelar_relayer=info".parse()?)
        .add_directive("relayer_amplifier_api_integration=info".parse()?)
        .add_directive("amplifier_api=info".parse()?)
        .add_directive("solana_listener=info".parse()?)
        .add_directive("solana_event_forwarder=info".parse()?)
        .add_directive("solana_gateway_task_processor=info".parse()?)
        .add_directive("effective_tx_sender=info".parse()?)
        .add_directive("file_based_storage=info".parse()?)
        .add_directive("hyper=error".parse()?)
        .add_directive("tonic=error".parse()?)
        .add_directive("reqwest=error".parse()?)
        .add_directive("rest-service=info".parse()?)
        .add_directive("tower_http=info".parse()?)
        .add_directive("axum=error".parse()?)
        .add_directive(EnvFilter::from_default_env().to_string().parse()?);

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stderr);

    // use json logging for release builds
    let subscriber = subscriber.with(filter).with(ErrorLayer::default());
    let subscriber = if cfg!(debug_assertions) {
        subscriber.with(output_layer.boxed())
    } else {
        subscriber.with(output_layer.json().with_current_span(true).boxed())
    };

    // init with the otlp logging layer
    if let Some(logs_layer) = logs_layer {
        subscriber.with(logs_layer).init();
    } else {
        subscriber.init();
    }

    Ok(())
}
