//! igbot — mirrors Instagram posts/reels into a Telegram group (PLAN.md).

mod bot;
mod config;
mod dedup;
mod error;
mod extract;
mod media;
mod metrics;
mod queue;
mod threads_share;
mod urls;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    // Hold the file-appender flush guard for the whole run (drop = flush).
    let _log_guard = init_tracing();

    let cfg = Arc::new(config::Config::from_env()?);
    tracing::info!("config loaded — {}", cfg.summary());
    if cfg.allowed_chats.is_empty() {
        tracing::warn!("ALLOWED_CHAT_IDS is empty — bot will act in ANY chat it is added to");
    }
    tokio::fs::create_dir_all(&cfg.temp_dir).await.ok();

    // Shared HTTP client (rustls); per-request UA is overridden where needed.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(cfg.user_agent.clone())
        .build()?;

    // Telegram API client with a generous request timeout: big media-group
    // sends (server-side URL fetches / multipart uploads) far outlast
    // teloxide's 17 s default, and timing out a request Telegram then
    // completes anyway yields a phantom "couldn't send" failure.
    let tg_http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(cfg.tg_send_timeout)
        .build()?;
    // Throttled bot (innermost wrap) for built-in rate limiting (PLAN §5).
    let tgbot =
        teloxide::Bot::with_client(cfg.bot_token.clone(), tg_http).throttle(Limits::default());
    preflight(&tgbot).await?;

    let dedup = dedup::Dedup::new(cfg.cache_ttl);
    let metrics = Arc::new(metrics::Metrics::new());

    // Periodic liveness/metrics heartbeat (HEARTBEAT_SECS; 0 disables).
    if let Some(interval) = cfg.heartbeat {
        metrics::spawn_heartbeat(metrics.clone(), interval);
    }

    // Bounded queue → single worker (concurrency = 1). The worker routes each
    // job to the Instagram or Threads chain by platform.
    let (tx, rx) = tokio::sync::mpsc::channel::<queue::QueuedJob>(cfg.queue_capacity);
    {
        let bot = tgbot.clone();
        let chains = queue::Chains {
            instagram: Arc::new(extract::build_ig_chain(&cfg, http.clone())),
            threads: Arc::new(extract::build_threads_chain(&cfg, http.clone())),
            threads_share: Arc::new(threads_share::ThreadsShareResolver::new(
                cfg.threads_user_agent.clone(),
                cfg.threads_sec_ch_ua.clone(),
            )?),
        };
        let cfg = cfg.clone();
        let http = http.clone();
        let dedup = dedup.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            queue::run_worker(rx, bot, chains, http, cfg, dedup, metrics).await;
        });
    }

    tracing::info!("igbot starting (long polling)");
    let handler = Update::filter_message().endpoint(bot::handler::on_message);
    Dispatcher::builder(tgbot, handler)
        .dependencies(dptree::deps![cfg, tx, dedup])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// Verify token + connectivity before `dispatch()`, which *panics* if its
/// startup prepare-context call fails (and we build with panic=abort, so it
/// can't be caught). Retries transient network errors; fatal on API rejection.
async fn preflight(bot: &bot::TgBot) -> Result<()> {
    const MAX: u32 = 5;
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=MAX {
        match bot.get_me().await {
            Ok(me) => {
                tracing::info!(bot = ?me.user.username, "authenticated with Telegram");
                return Ok(());
            }
            Err(teloxide::RequestError::Api(e)) => {
                return Err(anyhow::anyhow!(
                    "Telegram rejected the request ({e}); check TELEGRAM_BOT_TOKEN"
                ));
            }
            Err(e) => {
                if attempt == MAX {
                    return Err(anyhow::anyhow!(
                        "cannot reach Telegram after {MAX} attempts: {e}"
                    ));
                }
                tracing::warn!(attempt, error = %e, "preflight: Telegram unreachable, retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    unreachable!()
}

/// Initialise logging: always to stdout (→ systemd journal in production), and
/// optionally to a daily-rotating file when `LOG_DIR` is set. Reads its env
/// directly (before `Config`) so logging is live before config can fail.
/// Returns the file-appender flush guard, which the caller must keep alive.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "igbot=info,warn".into());
    // Plain text by default: ANSI styling shows up as `[3m`/`[2m` noise in
    // journald, log files, and consoles without VT processing (terminal
    // detection can't see those). Colors are opt-in via LOG_ANSI=1.
    let ansi = std::env::var("LOG_ANSI")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    let stdout_layer = fmt::layer().compact().with_target(false).with_ansi(ansi);

    // Optional rotating file layer. journald already persists stdout on the VM,
    // so this is opt-in (LOG_DIR). Build failure → warn + stdout-only; never
    // panic (release builds use panic=abort).
    let (file_layer, guard) = match std::env::var("LOG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(dir) => {
            let max_files = std::env::var("LOG_MAX_FILES")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(7);
            match tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("igbot")
                .filename_suffix("log")
                .max_log_files(max_files)
                .build(&dir)
            {
                Ok(appender) => {
                    // Lossy non-blocking writer: drop lines under burst rather
                    // than stall the async runtime on disk I/O.
                    let (writer, guard) = tracing_appender::non_blocking(appender);
                    let layer = fmt::layer()
                        .compact()
                        .with_ansi(false)
                        .with_target(false)
                        .with_writer(writer);
                    (Some(layer), Some(guard))
                }
                Err(e) => {
                    eprintln!("igbot: file logging disabled — cannot open LOG_DIR '{dir}': {e}");
                    (None, None)
                }
            }
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    guard
}
