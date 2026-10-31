//! Entry point for the per-session shim process (see the `swarm-shim` crate).

fn main() {
    swarm_proto::paths::close_inherited_fds();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SWARM_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let result = swarm_shim::ShimConfig::from_args(std::env::args().skip(1)).and_then(swarm_shim::run);
    if let Err(e) = result {
        tracing::error!("shim failed: {e:#}");
        eprintln!("swarm-shim: {e:#}");
        std::process::exit(1);
    }
}
