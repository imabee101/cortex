use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser)]
#[command(name = "cortex-api")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    #[arg(
        long,
        env = "CORTEX_API_DATABASE_URL",
        default_value = "postgres://127.0.0.1/cortex"
    )]
    database_url: String,
    #[arg(long, env = "CORTEX_API_CONTEXT_WINDOW", default_value_t = 98304)]
    context_window: u64,
    /// Share of the context window `/v1/models` reports. The harness sizes its conversation at
    /// bytes/4 and never reads the server's token count, which runs about twice that on code.
    #[arg(
        long,
        env = "CORTEX_API_CONTEXT_PERCENT",
        default_value_t = 40,
        value_parser = clap::value_parser!(u64).range(1..=100)
    )]
    context_percent: u64,
    #[arg(long, env = "CORTEX_API_MODEL", default_value = "Qwen3.5-9B")]
    model: String,
    #[arg(
        long,
        env = "CORTEX_API_BACKEND",
        default_value = "chat_completions",
        value_parser = ["chat_completions", "responses", "messages"]
    )]
    api_backend: String,
    #[arg(long, env = "CORTEX_API_RELEASE_VERSION", default_value = "0.0.0")]
    release_version: String,
    #[arg(long, env = "CORTEX_API_LLAMA")]
    llama: Option<String>,
    #[arg(long, env = "CORTEX_API_LLAMA_API_KEY")]
    llama_api_key: Option<String>,
    /// File holding the llama-server bearer token; wins over the environment value.
    #[arg(long, env = "CORTEX_API_LLAMA_API_KEY_FILE")]
    llama_api_key_file: Option<std::path::PathBuf>,
    #[arg(long, env = "CORTEX_API_EMBED")]
    embed: Option<String>,
    /// Model name the settings response advertises for memory embeddings.
    #[arg(long, env = "CORTEX_API_EMBED_MODEL")]
    embed_model: Option<String>,
    #[arg(long, env = "CORTEX_API_EMBED_DIMENSIONS", default_value_t = 1024)]
    embed_dimensions: u32,
    /// Directory with the install scripts and client builds served under /cli.
    #[arg(long, env = "CORTEX_API_DIST_DIR", default_value = "/opt/cortex/dist")]
    dist_dir: std::path::PathBuf,
    #[arg(long, env = "CORTEX_API_BRAVE_TOKEN")]
    brave_token: Option<String>,
    #[arg(
        long,
        env = "CORTEX_API_BRAVE_BASE",
        default_value = "https://api.search.brave.com/res/v1"
    )]
    brave_base: String,
    /// Concurrent inference requests; 0 reads `total_slots` from llama-server.
    #[arg(long, env = "CORTEX_API_PARALLEL", default_value_t = 0)]
    parallel: usize,
    #[arg(long, env = "CORTEX_API_QUEUE_MAX", default_value_t = 8)]
    queue_max: usize,
    #[arg(long, env = "CORTEX_API_QUEUE_WAIT_SECS", default_value_t = 30)]
    queue_wait_secs: u64,
    /// Ceiling on generated tokens per request; 0 leaves requests unbounded.
    #[arg(long, env = "CORTEX_API_MAX_OUTPUT_TOKENS", default_value_t = 24576)]
    max_output_tokens: u64,
    #[arg(long, env = "CORTEX_API_IP_RATE_PER_MIN", default_value_t = 1200)]
    ip_rate_per_min: u32,
    #[arg(long, env = "CORTEX_API_USER_RATE_PER_MIN", default_value_t = 1200)]
    user_rate_per_min: u32,
    #[arg(long, env = "CORTEX_API_INFERENCE_PER_MIN", default_value_t = 120)]
    inference_per_min: u32,
    #[arg(long, env = "CORTEX_API_DAILY_INFERENCE_QUOTA", default_value_t = 5000)]
    daily_inference_quota: i32,
    #[arg(long, env = "CORTEX_API_DAILY_SEARCH_QUOTA", default_value_t = 300)]
    daily_search_quota: i32,
    #[arg(
        long,
        env = "CORTEX_API_TELEMETRY_RETENTION_DAYS",
        default_value_t = 30
    )]
    telemetry_retention_days: i32,
    #[arg(long, env = "CORTEX_API_DRAIN_SECS", default_value_t = 900)]
    drain_secs: u64,
}

/// SIGINT from a terminal, SIGTERM from the service manager.
async fn wait_for_stop() -> std::io::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = term.recv() => Ok(()),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let mut config = cortex_api::Config::new(args.listen, args.database_url);
    config.context_window = args.context_window;
    config.context_percent = args.context_percent;
    config.model_id = args.model;
    config.api_backend = args.api_backend;
    config.release_version = args.release_version;
    config.llama_url = args.llama.filter(|url| !url.is_empty());
    config.llama_api_key = match args.llama_api_key_file {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|_| anyhow::anyhow!("llama api key file is unreadable"))?
                .trim()
                .to_owned(),
        ),
        None => args.llama_api_key,
    }
    .filter(|key| !key.is_empty());
    config.embed_url = args.embed.filter(|url| !url.is_empty());
    config.embed_model = args.embed_model.filter(|model| !model.is_empty());
    config.embed_dimensions = args.embed_dimensions;
    config.dist_dir = args.dist_dir;
    config.brave_token = args.brave_token.filter(|token| !token.is_empty());
    if !args.brave_base.is_empty() {
        config.brave_base = args.brave_base;
    }
    config.parallel = args.parallel;
    config.queue_max = args.queue_max;
    config.queue_wait_secs = args.queue_wait_secs;
    config.max_output_tokens = args.max_output_tokens;
    config.ip_rate_per_min = args.ip_rate_per_min;
    config.user_rate_per_min = args.user_rate_per_min;
    config.inference_per_min = args.inference_per_min;
    config.daily_inference_quota = args.daily_inference_quota;
    config.daily_search_quota = args.daily_search_quota;
    config.telemetry_retention_days = args.telemetry_retention_days;
    config.drain_secs = args.drain_secs;
    config.props_wait_tries = 90;
    let drain = std::time::Duration::from_secs(config.drain_secs);
    let running = cortex_api::serve(config).await?;
    tracing::info!(addr = %running.addr, "listening");
    wait_for_stop().await?;
    tracing::info!("draining open streams");
    match tokio::time::timeout(drain, running.shutdown()).await {
        Ok(result) => result?,
        Err(_) => tracing::warn!("drain window ended with streams still open"),
    }
    Ok(())
}
