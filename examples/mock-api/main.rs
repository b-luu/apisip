//! The mock API used by the tests and the README quickstart:
//!
//! ```sh
//! cargo run --example mock-api            # serves on 127.0.0.1:8080
//! cargo run --example mock-api -- 9000    # or another port
//! ```

#[path = "server.rs"]
mod server;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = server::serve(([127, 0, 0, 1], port).into()).await?;
    println!("Mock API on http://{addr}");
    println!("  spec:  http://{addr}/openapi.json (v2: /v2/openapi.json)");
    println!("  token: {}", server::TOKEN);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
