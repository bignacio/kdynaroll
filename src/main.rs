use kdynaroll::controller;
use kdynaroll::metrics;
use kube::Client;
use tracing::info;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    info!("starting metrics service");

    tokio::spawn(async {
        let metrics_bind_addr = std::env::var("KDYNAROLL_METRICS_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:5085".to_string());
        if let Err(e) = metrics::start_http_server(metrics_bind_addr).await {
            eprintln!("Error starting HTTP server: {}", e);
        }
    });

    info!("Starting controller");
    let client = Client::try_default().await?;

    controller::start_controller(client).await?;

    Ok(())
}
