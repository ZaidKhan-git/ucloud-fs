#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("Driveme Unified Cloud VFS daemon initializing...");
}
