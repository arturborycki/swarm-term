//! Entry point for the per-session shim process (see the `swarm-shim` crate).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Asked by a running shim before an in-place update: can this program
    // take over a session from it?
    if args.first().map(String::as_str) == Some("--accepts-handoff") {
        let version = args.get(1).and_then(|v| v.parse::<u32>().ok());
        std::process::exit(if version.is_some_and(swarm_shim::handoff::accepts) { 0 } else { 1 });
    }
    swarm_proto::paths::close_inherited_fds_except(&swarm_shim::handed_over_fds(&args));
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SWARM_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let result = swarm_shim::ShimConfig::from_args(args).and_then(swarm_shim::run);
    if let Err(e) = result {
        tracing::error!("shim failed: {e:#}");
        eprintln!("swarm-shim: {e:#}");
        std::process::exit(1);
    }
}
