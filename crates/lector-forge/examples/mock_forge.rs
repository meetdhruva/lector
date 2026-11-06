//! Serves recorded forge JSON for offline demos and screenshots:
//! `cargo run -p lector-forge --example mock_forge -- <fixtures dir> <port>`.

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8765);
    let server = lector_forge::mock::MockServer::from_dir(&dir, port)?;
    println!("mock forge on {}", server.url());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
