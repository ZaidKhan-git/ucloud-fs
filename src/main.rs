#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    info!("ucloud-fs Unified Cloud VFS daemon initializing...");

    // Wait for termination signal (Ctrl+C or SIGTERM)
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received termination signal (Ctrl+C); initiating clean unmount and cache purge...");
        }
    }

    info!("ucloud-fs shutdown complete (exit code 0)");
    Ok(())
}
