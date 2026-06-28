//! Single-worker job queue (concurrency = 1) — the key memory-safety lever on
//! a 1 GB box (PLAN §3.4 / §4). Jobs are processed one at a time, each under a
//! wall-clock timeout, with pacing between jobs.

use crate::bot::{sender, TgBot};
use crate::config::Config;
use crate::dedup::Dedup;
use crate::extract::{ExtractError, ExtractorChain};
use crate::metrics::Metrics;
use std::sync::Arc;
use std::time::Instant;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::mpsc::Receiver;
use tracing::Instrument;

#[derive(Debug, Clone)]
pub struct Job {
    pub chat_id: ChatId,
    pub reply_to: MessageId,
    pub original_url: String,
    pub shortcode: String,
}

/// What became of a job — drives both the dedup decision and the log line.
enum Outcome {
    /// Media delivered to the chat. Keeps the dedup claim for the TTL.
    Delivered { media: usize },
    /// Post genuinely unavailable (the user was notified). Releases the claim
    /// so a re-post can retry, per the dedup invariant.
    Unavailable,
}

/// Drain the job channel forever, processing one job at a time.
pub async fn run_worker(
    mut rx: Receiver<Job>,
    bot: TgBot,
    chain: Arc<ExtractorChain>,
    http: reqwest::Client,
    cfg: Arc<Config>,
    dedup: Dedup,
    metrics: Arc<Metrics>,
) {
    tracing::info!("worker started (concurrency = 1)");
    while let Some(job) = rx.recv().await {
        let seq = metrics.record_received();
        // One span per job: every downstream line is tagged with seq/shortcode/
        // chat automatically, so a post's whole lifecycle is greppable.
        let span = tracing::info_span!("job", seq, shortcode = %job.shortcode, chat = job.chat_id.0);
        process_job(&bot, &chain, &http, &cfg, &dedup, &metrics, &job)
            .instrument(span)
            .await;
        // Pace requests so we don't hammer Instagram (PLAN §4.6).
        tokio::time::sleep(cfg.request_pacing).await;
    }
    tracing::warn!("worker channel closed; exiting");
}

/// Run one job under the wall-clock timeout, record its outcome accurately, and
/// release the dedup claim unless the post was actually delivered.
async fn process_job(
    bot: &TgBot,
    chain: &ExtractorChain,
    http: &reqwest::Client,
    cfg: &Config,
    dedup: &Dedup,
    metrics: &Metrics,
    job: &Job,
) {
    let started = Instant::now();
    tracing::info!("processing job");
    match tokio::time::timeout(cfg.job_timeout, handle(bot, chain, http, cfg, job)).await {
        // Delivered → keep the claim (TTL-dedups re-posts).
        Ok(Ok(Outcome::Delivered { media })) => {
            metrics.record_succeeded();
            tracing::info!(media, elapsed_ms = elapsed_ms(started), "delivered");
        }
        // Unavailable / failed / timed out all release the claim so a re-post
        // can retry — only a delivered post stays deduped (PLAN §4.6).
        Ok(Ok(Outcome::Unavailable)) => {
            metrics.record_failed();
            dedup.forget(&job.shortcode).await;
            tracing::info!(elapsed_ms = elapsed_ms(started), "post unavailable; user notified");
        }
        Ok(Err(e)) => {
            metrics.record_failed();
            dedup.forget(&job.shortcode).await;
            tracing::warn!(error = %e, elapsed_ms = elapsed_ms(started), "job failed");
        }
        Err(_) => {
            metrics.record_timed_out();
            dedup.forget(&job.shortcode).await;
            tracing::warn!(elapsed_ms = elapsed_ms(started), "job timed out");
            let _ = sender::reply_failure(
                bot,
                job,
                "⏱️ That one took too long to fetch — try again later.",
            )
            .await;
        }
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

async fn handle(
    bot: &TgBot,
    chain: &ExtractorChain,
    http: &reqwest::Client,
    cfg: &Config,
    job: &Job,
) -> anyhow::Result<Outcome> {
    match chain.extract(&job.original_url, &job.shortcode).await {
        Ok(post) => {
            let media = post.media.len();
            // Extraction succeeded — if delivery fails, still tell the user
            // (don't leave them in silence) and propagate the error so the
            // worker releases the dedup claim for a retry.
            if let Err(e) = sender::deliver(bot, http, cfg, job, post).await {
                let _ = sender::reply_failure(
                    bot,
                    job,
                    "⚠️ Found the post, but couldn't send it here — try again.",
                )
                .await;
                return Err(e);
            }
            Ok(Outcome::Delivered { media })
        }
        Err(ExtractError::NotFound) => {
            sender::reply_failure(
                bot,
                job,
                "🤷 Couldn't find that post — it may be private, removed, or image-only behind a login.",
            )
            .await?;
            Ok(Outcome::Unavailable)
        }
        Err(e) => {
            sender::reply_failure(
                bot,
                job,
                "⚠️ Couldn't fetch that one — Instagram may be rate-limiting right now.",
            )
            .await?;
            Err(anyhow::anyhow!("all backends failed: {e}"))
        }
    }
}
