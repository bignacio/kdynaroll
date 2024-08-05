use lazy_static::lazy_static;
use prometheus::{Encoder, Gauge, IntCounter, IntCounterVec, Opts, TextEncoder};
use std::sync::{Arc, Mutex};

use axum::response::{IntoResponse, Response};
use axum::routing::{get, Router};

lazy_static! {
    static ref RECONCILE_CYCLE_COUNT: Arc<Mutex<IntCounterVec>> = Arc::new(Mutex::new(
        prometheus::register_int_counter_vec!(Opts::new("reconcile_cycle_completed", "Total number of completed reconciliation cycles"), &["result"]).unwrap()
    ));
    static ref RECONCILE_TIME: Arc<Mutex<Gauge>> = Arc::new(Mutex::new(
        prometheus::register_gauge!(Opts::new("reconcile_cycle_time", "Time in ms spent in the reconcile cycle")).unwrap()
    ));
    static ref LABELED_NAMESPACE_COUNT: Arc<Mutex<IntCounter>> = Arc::new(Mutex::new(
        prometheus::register_int_counter!(Opts::new("namespaces_labeled", "Number of namespaces configured for labeling")).unwrap()
    ));
    static ref LABELED_APP_COUNT: Arc<Mutex<IntCounterVec>> = Arc::new(Mutex::new(
        prometheus::register_int_counter_vec!(Opts::new("apps_labeled", "Number of apps configured for labeling per namespace"), &["namespace"]).unwrap()
    ));
    static ref LABELED_POD_COUNT: Arc<Mutex<IntCounterVec>> = Arc::new(Mutex::new(
        prometheus::register_int_counter_vec!(Opts::new("pods_labeled", "Number of pods effectively labeled per namespace and app"), &["namespace", "app"]).unwrap()
    ));
}

pub fn record_reconcile_cycle(result: &str) {
    RECONCILE_CYCLE_COUNT
        .lock()
        .unwrap()
        .with_label_values(&[result])
        .inc();
}

pub fn record_reconcile_time(time_ms: f64) {
    RECONCILE_TIME.lock().unwrap().set(time_ms);
}

pub fn record_namespace_count(count: u64) {
    LABELED_NAMESPACE_COUNT.lock().unwrap().inc_by(count);
}

pub fn record_app_count(namespace: &str, count: u64) {
    LABELED_APP_COUNT
        .lock()
        .unwrap()
        .with_label_values(&[namespace])
        .inc_by(count);
}

pub fn record_pod_count(namespace: &str, app: &str, count: u64) {
    LABELED_POD_COUNT
        .lock()
        .unwrap()
        .with_label_values(&[namespace, app])
        .inc_by(count);
}

async fn server_metrics() -> Response {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::default_registry().gather();

    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => buffer.into_response(),
        Err(e) => {
            println!("could not encode metrics: {}", e);
            Response::builder()
                .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body("Internal Server Error".into())
                .unwrap()
        }
    }
}

pub async fn start_http_server(metrics_bind_addr: String) -> Result<(), std::io::Error> {
    let app = Router::new().route("/metrics", get(server_metrics));

    let listener = tokio::net::TcpListener::bind(metrics_bind_addr)
        .await
        .unwrap();
    axum::serve(listener, app).await
}
