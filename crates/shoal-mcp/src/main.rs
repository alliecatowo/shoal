fn main() {
    let mut config = shoal_mcp::Config {
        socket: std::env::var_os("SHOAL_SOCKET")
            .map(Into::into)
            .unwrap_or_default(),
        session: std::env::var("SHOAL_SESSION").ok(),
        token: std::env::var("SHOAL_TOKEN").ok(),
        local_auth: shoal_mcp::LocalAuthMode::RestrictedAgent,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => config.socket = args.next().map(Into::into).unwrap_or_else(|| usage()),
            "--session" => config.session = Some(args.next().unwrap_or_else(|| usage())),
            "--token" => config.token = Some(args.next().unwrap_or_else(|| usage())),
            "--local-human" => config.local_auth = shoal_mcp::LocalAuthMode::LocalHuman,
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    if config.token.is_some() && config.local_auth == shoal_mcp::LocalAuthMode::LocalHuman {
        eprintln!("shoal-mcp: --token and --local-human are mutually exclusive");
        usage();
    }
    if config.socket.as_os_str().is_empty() {
        let session = config.session.as_deref().unwrap_or("default");
        config.socket = shoal_mcp::discover_socket(session);
    }
    if let Err(error) = shoal_mcp::run_stdio(&config) {
        eprintln!("shoal-mcp: {error}");
        std::process::exit(1);
    }
}
fn usage() -> ! {
    eprintln!("usage: shoal-mcp [--socket PATH] [--session NAME] [--token TOKEN | --local-human]");
    std::process::exit(2)
}
