use clewdr_hub::{
    Args, Command, FIG, IS_DEBUG,
    config::{CLEWDR_CONFIG, CONFIG_PATH, DB_PATH, LOG_DIR},
    error::ClewdrError,
    version_info_colored,
};
use colored::Colorize;
#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;
use std::{io::IsTerminal, net::SocketAddr};
use tracing::Subscriber;
use tracing_subscriber::{
    Layer, Registry,
    fmt::{self, time::ChronoLocal},
    layer::SubscriberExt,
    registry::LookupSpan,
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn setup_subscriber<S>(subscriber: S)
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    #[cfg(feature = "tokio-console")]
    let subscriber = {
        // enable tokio console
        use std::str::FromStr;
        let tokio_console_filter =
            tracing_subscriber::filter::Targets::from_str("tokio=trace,runtime=trace")
                .expect("Failed to parse filter");
        let console_layer = console_subscriber::ConsoleLayer::builder()
            // set the address the server is bound to
            .server_addr(([0, 0, 0, 0], 6669))
            .spawn();
        subscriber.with(console_layer.with_filter(tokio_console_filter))
    };
    tracing::subscriber::set_global_default(subscriber).expect("unable to set global subscriber");
}

fn admin_panel_url(addr: SocketAddr) -> String {
    let host = if addr.ip().is_unspecified() {
        "localhost".to_string()
    } else if addr.ip().is_ipv6() {
        format!("[{}]", addr.ip())
    } else {
        addr.ip().to_string()
    };
    format!("http://{}:{}", host, addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_bare_invocation_gets_friendly_default() {
        let args = Args::default();
        assert!(should_show_interactive_default(&args, 1, true));
    }

    #[test]
    fn non_tty_bare_invocation_keeps_serve_compatibility() {
        let args = Args::default();
        assert!(!should_show_interactive_default(&args, 1, false));
    }

    #[test]
    fn argv_or_explicit_serve_keeps_serve_path() {
        let mut args = Args::default();
        assert!(!should_show_interactive_default(&args, 2, true));

        args.command = Some(Command::Serve);
        assert!(!should_show_interactive_default(&args, 1, true));
    }
}

/// Application entry point.
///
/// Order matters here:
/// 1. Install global crypto provider + ANSI/color setup (cheap, side-effect-free).
/// 2. Dispatch CLI subcommands. They MUST run before logging or
///    [`CLEWDR_CONFIG`] initialization, because touching `CLEWDR_CONFIG`
///    spawns an async writeback to `clewdr.toml` that would race with
///    `clewdr import-config`.
/// 3. In an interactive terminal, bare `clewdr` prints status/help instead
///    of trying to start a second foreground server.
/// 4. Only the serve path proceeds to logging + config + DB + axum.
#[tokio::main]
async fn main() -> Result<(), ClewdrError> {
    // Strict argv validation: surface unknown flags (typos, removed args) as a
    // normal clap error and exit BEFORE anything else runs. The library
    // [`ARGS`] LazyLock uses `try_parse` with a default fallback so that test
    // harnesses and other library consumers tolerate unrelated argv; the
    // binary itself wants the strict behaviour.
    use clap::Parser;
    let args = Args::parse();
    let argc = std::env::args_os().len();

    // Ensure a crypto provider is installed before rustls usage (yup-oauth2 / hyper-rustls).
    #[cfg(target_os = "android")]
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");
    #[cfg(not(target_os = "android"))]
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install aws-lc crypto provider");

    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    #[cfg(windows)]
    {
        _ = enable_ansi_support::enable_ansi_support();
    }

    // detect if stdout is a TTY and disable colors if not
    let stdout_is_tty = std::io::stdout().is_terminal();
    colored::control::set_override(stdout_is_tty);

    // ---- Early CLI dispatch (must precede logging + CLEWDR_CONFIG) ----
    if let Some(cmd) = args.command.clone() {
        if !matches!(cmd, Command::Serve) {
            return verb_finish(clewdr_hub::cli::dispatch(cmd).await);
        }
    }
    #[cfg(feature = "portable")]
    if args.update {
        return verb_finish(clewdr_hub::cli::run_update().await);
    }
    if should_show_interactive_default(&args, argc, stdout_is_tty) {
        return verb_finish(interactive_default().await);
    }

    // ---- From here on it's the serve path; logging + CLEWDR_CONFIG are fair game ----
    serve().await
}

fn should_show_interactive_default(args: &Args, argc: usize, stdout_is_tty: bool) -> bool {
    if !stdout_is_tty || argc != 1 || args.command.is_some() {
        return false;
    }
    #[cfg(feature = "portable")]
    if args.update {
        return false;
    }
    true
}

async fn interactive_default() -> Result<(), ClewdrError> {
    println!("clewdr v{}", env!("CARGO_PKG_VERSION"));
    println!("检测到交互式裸命令，未启动前台服务。");
    println!();

    clewdr_hub::cli::status::run(clewdr_hub::cli::status::Args { json: false }).await?;

    println!();
    println!("常用命令：");
    #[cfg(feature = "tui")]
    println!("  clewdr menu      打开操作菜单");
    println!("  clewdr status    查看运行状态");
    println!("  clewdr diagnose  诊断安装");
    println!("  clewdr serve     前台启动服务");
    println!();
    println!("提示：安装脚本通常已注册后台服务；需要手动前台运行时再执行 `clewdr serve`。");
    Ok(())
}

/// Convert a CLI verb's `Result` into a process-level outcome.
///
/// On success we just return `Ok(())` so `#[tokio::main]` exits 0. On error
/// we print the [`ClewdrError`]'s `Display` form (which carries the carefully
/// crafted user-facing text — DbNotFound, BadRequest, etc.) and exit 1
/// directly, bypassing tokio's default `Debug`-formatted error printout.
fn verb_finish(result: Result<(), ClewdrError>) -> Result<(), ClewdrError> {
    if let Err(e) = result {
        eprintln!("{} {e}", "Error:".red().bold());
        std::process::exit(1);
    }
    Ok(())
}

async fn serve() -> Result<(), ClewdrError> {
    // set up logging time format
    let timer = ChronoLocal::new("%H:%M:%S%.3f".to_string());
    // set up logging
    let filter = if IS_DEBUG {
        tracing_subscriber::filter::LevelFilter::DEBUG
    } else {
        tracing_subscriber::filter::LevelFilter::INFO
    };
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(filter.into())
        .from_env_lossy();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let subscriber = Registry::default().with(
        fmt::Layer::default()
            .with_writer(std::io::stdout)
            .with_timer(timer.to_owned())
            .with_ansi(stdout_is_tty)
            .with_filter(env_filter),
    );
    let _guard = if !CLEWDR_CONFIG.load().no_fs && CLEWDR_CONFIG.load().log_to_file {
        std::fs::create_dir_all(LOG_DIR.as_path()).expect("Failed to create log directory");
        let file_appender = tracing_appender::rolling::daily(LOG_DIR.as_path(), "clewdr.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(filter.into())
            .from_env_lossy();
        let subscriber = subscriber.with(
            fmt::Layer::default()
                .with_writer(file_writer)
                .with_timer(timer)
                .with_ansi(false) // disable ANSI colors for file logging
                .with_filter(filter),
        );
        setup_subscriber(subscriber);
        Some(guard)
    } else {
        setup_subscriber(subscriber);
        None
    };

    println!("{}\n{}", FIG, version_info_colored());

    #[cfg(feature = "portable")]
    {
        use tracing::warn;
        let updater = clewdr_hub::services::update::ClewdrUpdater::new()?;
        if let Err(e) = updater.check_for_updates(false).await {
            warn!("Update check failed: {}", e);
        }
    }

    // print info
    println!("Config dir: {}", CONFIG_PATH.display().to_string().blue());
    println!("{}", *CLEWDR_CONFIG);

    // initialize database
    let no_fs = CLEWDR_CONFIG.load().no_fs;
    let db_path = if no_fs {
        std::path::PathBuf::from(":memory:")
    } else {
        DB_PATH.to_owned()
    };
    println!("Database: {}", db_path.display().to_string().blue());
    let db_pool = clewdr_hub::db::init_pool(&db_path).await?;
    clewdr_hub::db::seed_admin(&db_pool).await?;

    // Start log rotation background task
    tokio::spawn(clewdr_hub::services::log_rotation::start_log_rotation(
        db_pool.clone(),
    ));

    // build axum router
    let addr = CLEWDR_CONFIG.load().address();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let router = clewdr_hub::router::RouterBuilder::new(db_pool)
        .await
        .with_default_setup()
        .build();
    println!(
        "Admin panel: {}",
        admin_panel_url(local_addr).green().underline()
    );
    // serve the application
    Ok(axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl-C handler");
        })
        .await?)
}
