use clap::{Parser, Subcommand};
use futures::StreamExt;
use serde_json::{json, Value};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use lanehub_core::{
    catalog,
    catalog::Lane,
    config::Config,
    daemon,
    egress::EgressPool,
    health::HealthTracker,
    router::{RoutedResponse, Router},
    server::{build_app, AppState},
};
const DEFAULT_BIND: &str = "127.0.0.1:18788";
const DEFAULT_PROBE_INTERVAL: u64 = 300;
const DEFAULT_QUOTA_WAIT_SECS: u64 = 120;

#[derive(Parser)]
#[command(
    name = "lanehub",
    version,
    about = "LaneHub — High-Performance Model Router & Multi-Homed Gateway"
)]
struct Cli {
    /// Comma-separated or repeated egress specs: `auto` (IPv6+IPv4 dual-stack), `direct`,
    /// `ipv6`, `ipv4`, `http://host:port`, `socks5://host:port`, `bind://192.168.x.x`, or
    /// `relay://https://relay-host#key=SECRET` (serverless egress, fresh IP per cold boot)
    #[arg(long = "egress", short = 'e', global = true, value_delimiter = ',', env = "LANEHUB_EGRESS")]
    egress: Option<Vec<String>>,

    /// Require gateway clients to present `Authorization: Bearer <key>` (env: LANEHUB_API_KEY)
    #[arg(long = "api-key", global = true, env = "LANEHUB_API_KEY")]
    api_key: Option<String>,

    /// Disable cross-model automatic failover when the requested model is unavailable on all exits
    #[arg(long = "no-failover", global = true)]
    no_failover: bool,

    /// Optional JSON config file (env: LANEHUB_CONFIG); CLI flags override its values
    #[arg(long = "config", global = true, env = "LANEHUB_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Probe all catalog models concurrently across configured egress paths
    Probe,
    /// Run a single prompt from the CLI
    Run {
        #[arg(short = 'm', long = "model", default_value = "beam-free")]
        model: String,
        /// Prompt text
        prompt: String,
    },
    /// Start the local OpenAI-compatible HTTP gateway server
    Serve {
        #[arg(short = 'b', long = "bind", env = "LANEHUB_BIND",
              help = "Listen address [default: 127.0.0.1:18788]")]
        bind: Option<SocketAddr>,
        /// Run detached in the background (log: ~/.local/share/lanehub/lanehub.log)
        #[arg(long = "daemon")]
        daemon: bool,
        /// Background health prober interval in seconds, 0 disables (env: LANEHUB_PROBE_INTERVAL)
        #[arg(long = "probe-interval", env = "LANEHUB_PROBE_INTERVAL")]
        probe_interval: Option<u64>,
        /// When all exits are quota-429'd, wait up to this many seconds for the
        /// shortest Retry-After and retry once instead of erroring (0 disables)
        #[arg(long = "quota-wait", env = "LANEHUB_QUOTA_WAIT")]
        quota_wait: Option<u64>,
    },
    /// Stop a background instance started with `serve --daemon`
    Stop,
}

struct Settings {
    bind: SocketAddr,
    egress: Vec<String>,
    allow_failover: bool,
    api_key: Option<String>,
    probe_interval: u64,
    quota_wait_secs: u64,
}

/// Merge CLI flags > env vars (already applied by clap) > config file > defaults.
fn resolve_settings(
    egress: &Option<Vec<String>>,
    api_key: &Option<String>,
    no_failover: bool,
    config: Option<&PathBuf>,
    bind_override: Option<SocketAddr>,
    interval_override: Option<u64>,
    quota_wait_override: Option<u64>,
) -> Result<Settings, String> {
    let (cfg, _path) = Config::load(config.map(|p| p.as_path()))?;
    Ok(Settings {
        bind: bind_override
            .or(cfg.bind)
            .unwrap_or_else(|| DEFAULT_BIND.parse().expect("valid default bind")),
        egress: egress
            .clone()
            .or(cfg.egress)
            .unwrap_or_else(|| vec!["auto".to_string()]),
        allow_failover: !(no_failover || cfg.no_failover.unwrap_or(false)),
        api_key: api_key.clone().or(cfg.api_key).filter(|s| !s.is_empty()),
        probe_interval: interval_override.or(cfg.probe_interval_secs).unwrap_or(DEFAULT_PROBE_INTERVAL),
        quota_wait_secs: quota_wait_override.unwrap_or(DEFAULT_QUOTA_WAIT_SECS),
    })
}

fn build_router(s: &Settings) -> Arc<Router> {
    let pool = Arc::new(EgressPool::new(&s.egress));
    let mut router = Router::new(pool, s.allow_failover);
    router.quota_wait_secs = s.quota_wait_secs;
    Arc::new(router)
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("\n[lanehub] shutdown signal received, stopping gateway...");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Config::load_bundled_env();
    let Cli {
        egress,
        api_key,
        no_failover,
        config,
        command,
    } = Cli::parse();

    // Best-effort upstream metadata refresh before any lane work starts.
    catalog::init_catalog_refresh();

    match command {
        Commands::Stop => {
            init_tracing();
            if let Err(e) = daemon::stop() {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Commands::Probe => {
            init_tracing();
            let s = resolve_settings(&egress, &api_key, no_failover, config.as_ref(), None, None, None)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let router = build_router(&s);
            run_probe(&router).await;
        }
        Commands::Run { model, prompt } => {
            init_tracing();
            let s = resolve_settings(&egress, &api_key, no_failover, config.as_ref(), None, None, None)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let router = build_router(&s);
            run_prompt(router, model, prompt).await;
        }
        Commands::Serve {
            bind,
            daemon: daemon_mode,
            probe_interval,
            quota_wait,
        } => {
            init_tracing();
            let s = resolve_settings(
                &egress,
                &api_key,
                no_failover,
                config.as_ref(),
                bind,
                probe_interval,
                quota_wait,
            )
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let router = build_router(&s);

            if daemon_mode {
                let opts = daemon::ServeOptions {
                    bind: s.bind,
                    egress: s.egress,
                    no_failover: !s.allow_failover,
                    api_key: s.api_key.clone(),
                    probe_interval: s.probe_interval,
                    config: config.clone(),
                };
                match daemon::spawn_detached(&opts) {
                    Ok(pid) => {
                        println!("lanehub daemon started (pid {pid})");
                        println!("  endpoint: http://{}/v1", s.bind);
                        println!("  log     : {}", daemon::log_file().display());
                        println!("  stop    : lanehub stop");
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }

            let exits = router.egress.nodes().len();
            let health =
                HealthTracker::spawn(Arc::clone(&router), Duration::from_secs(s.probe_interval));
            let state = AppState {
                router,
                health,
                api_key: s.api_key.clone(),
                started_at: Instant::now(),
            };
            let app = build_app(state);
            let listener = tokio::net::TcpListener::bind(s.bind).await?;
            println!(
                "LaneHub v{} listening on http://{}/v1 (exits: {exits})",
                env!("CARGO_PKG_VERSION"),
                s.bind
            );
            println!(
                "  prober  : {}",
                if s.probe_interval > 0 {
                    format!("every {}s", s.probe_interval)
                } else {
                    "disabled".to_string()
                }
            );
            println!(
                "  api-key : {}",
                if s.api_key.is_some() { "required" } else { "none" }
            );
            println!(
                "  failover: {}",
                if s.allow_failover { "on" } else { "off" }
            );
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }

    Ok(())
}

async fn run_probe(router: &Arc<Router>) {
    println!(
        "=== LaneHub Live Probe (exits: {}) ===",
        router.egress.nodes().len()
    );
    let reports = router.probe_all().await;
    for r in reports {
        let tag = if r.ok { "OK " } else { "ERR" };
        let lane_str = match r.lane {
            Lane::OpenCodeZen => "opencode-zen",
            Lane::KiloFree => "kilo-free   ",
        };
        println!(
            "  [{tag}] {lane_str} | {:42} | HTTP {:3} ({:4}ms) via {:12} | {}",
            r.model, r.status, r.latency_ms, r.exit_id, r.detail
        );
    }
}

async fn run_prompt(router: Arc<Router>, model: String, prompt: String) {
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true
    });
    match router.execute_chat(body).await {
        Ok(RoutedResponse::Stream {
            served_model,
            exit_id,
            mut stream,
        }) => {
            eprintln!("[lanehub] served by {served_model} via {exit_id}");
            let mut buf = String::new();
            while let Some(chunk_res) = stream.next().await {
                let chunk = match chunk_res {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("stream error: {e}");
                        std::process::exit(1);
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf.drain(..=pos);
                    if let Some(payload) = line.strip_prefix("data:").map(|s| s.trim()) {
                        if payload == "[DONE]" || payload.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(payload) {
                            if let Some(c) = v
                                .get("choices")
                                .and_then(|c| c.get(0))
                                .and_then(|c| c.get("delta"))
                                .and_then(|d| d.get("content"))
                                .and_then(|c| c.as_str())
                            {
                                print!("{c}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                    }
                }
            }
            println!();
        }
        Ok(RoutedResponse::Json { body, .. }) => {
            println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        }
        Err((status, err)) => {
            eprintln!("HTTP {status}: {err}");
            std::process::exit(1);
        }
    }
}
