use tracing_subscriber::EnvFilter;

pub(super) fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("venticode=info,warn"));
    let json_format =
        std::env::var("VENTICODE_LOG_FORMAT").is_ok_and(|v| v.trim().eq_ignore_ascii_case("json"));
    if json_format {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_writer(std::io::stderr)
            .json()
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_writer(std::io::stderr)
            .try_init();
    }
}
