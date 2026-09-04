//! Single-worker job queue (concurrency = 1) — the key memory-safety lever on
//! a 1 GB box (PLAN §3.4 / §4). Jobs are processed one at a time, each under a
//! wall-clock timeout, with pacing between jobs.

use crate::bot::{sender, TgBot};
use crate::config::Config;
use crate::dedup::Dedup;
use crate::extract::{ExtractError, ExtractorChain, Post};
use crate::metrics::Metrics;
use crate::threads_share::ThreadsShareResolver;
use crate::urls::{LinkTarget, Platform};
use std::sync::Arc;
use std::time::Instant;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::mpsc::Receiver;
use tracing::Instrument;

/// What kind of upstream resource a job mirrors. Together with [`Platform`]
/// (the host) this picks the extractor chain and the failure wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// A regular post / reel (Instagram or Threads).
    Post,
    /// An Instagram story item or highlight — login-walled; own chain.
    Story,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub chat_id: ChatId,
    pub reply_to: MessageId,
    pub platform: Platform,
    pub kind: ContentKind,
    /// Canonical post URL — fetched by the chain and shown in the caption.
    pub original_url: String,
    pub shortcode: String,
}

/// Reply for a story link when no login session is configured
/// (`IG_COOKIES_PATH` unset): stories cannot be fetched anonymously, and an
/// unexplained silence would read as a bug.
const NO_STORY_SESSION_NOTICE: &str = "🔒 Instagram Stories can only be fetched with a \
logged-in session, and this bot runs without one.";

/// A detected link waiting in the bounded worker queue. Direct posts are ready
/// for extraction; Threads share aliases still carry their redirect token.
#[derive(Debug, Clone)]
pub struct QueuedJob {
    pub chat_id: ChatId,
    pub reply_to: MessageId,
    pub platform: Platform,
    pub target: LinkTarget,
}

impl QueuedJob {
    pub fn dedup_key(&self) -> String {
        self.target.dedup_key(self.platform)
    }

    fn kind(&self) -> ContentKind {
        match self.target {
            LinkTarget::Story { .. } => ContentKind::Story,
            LinkTarget::Post { .. } | LinkTarget::ThreadsShare { .. } => ContentKind::Post,
        }
    }

    fn reply_context(&self) -> Job {
        Job {
            chat_id: self.chat_id,
            reply_to: self.reply_to,
            platform: self.platform,
            kind: self.kind(),
            original_url: self.target.url().to_string(),
            shortcode: self.target.id().to_string(),
        }
    }
}

/// The extractor chains the worker routes between, selected by the link's host
/// — never by the shortcode (IG/Threads codes can collide) — and, for
/// Instagram, by content kind: stories have their own login-backed chain.
pub struct Chains {
    pub instagram: Arc<ExtractorChain>,
    /// `None` when no cookies are configured — stories are login-walled, so
    /// story jobs then get [`NO_STORY_SESSION_NOTICE`] instead of a chain.
    pub instagram_story: Option<Arc<ExtractorChain>>,
    pub threads: Arc<ExtractorChain>,
    pub threads_share: Arc<ThreadsShareResolver>,
}

impl Chains {
    fn select(&self, job: &Job) -> Option<&ExtractorChain> {
        match (job.platform, job.kind) {
            (Platform::Instagram, ContentKind::Story) => self.instagram_story.as_deref(),
            (Platform::Instagram, ContentKind::Post) => Some(&self.instagram),
            (Platform::Threads, _) => Some(&self.threads),
        }
    }
}

/// What became of a job — drives both the dedup decision and the log line.
enum Outcome {
    /// Media delivered to the chat. Keeps the dedup claim for the TTL.
    Delivered { media: usize },
    /// Post genuinely unavailable (the user was notified). Releases the claim
    /// so a re-post can retry, per the dedup invariant.
    Unavailable,
    /// A share alias resolved to a canonical post already claimed for the TTL.
    Duplicate,
}

/// Dedup claims owned by one queued job. Direct links own their canonical key;
/// share aliases additionally claim the resolved post key. Only delivered jobs
/// keep these claims for the TTL.
struct ClaimSet {
    keys: Vec<String>,
}

impl ClaimSet {
    fn new(initial_key: String) -> Self {
        Self {
            keys: vec![initial_key],
        }
    }

    /// Claim another identity. Returns true when another job already owns it.
    async fn claim(&mut self, dedup: &Dedup, key: String) -> bool {
        if self.keys.iter().any(|owned| owned == &key) {
            return false;
        }
        if dedup.seen_or_claim(&key).await {
            true
        } else {
            self.keys.push(key);
            false
        }
    }

    async fn release_all(self, dedup: &Dedup) {
        for key in self.keys {
            dedup.forget(&key).await;
        }
    }
}

/// Drain the job channel forever, processing one job at a time.
pub async fn run_worker(
    mut rx: Receiver<QueuedJob>,
    bot: TgBot,
    chains: Chains,
    http: reqwest::Client,
    cfg: Arc<Config>,
    dedup: Dedup,
    metrics: Arc<Metrics>,
) {
    tracing::info!("worker started (concurrency = 1)");
    while let Some(job) = rx.recv().await {
        let seq = metrics.record_received();
        // One span per job: every downstream line is tagged automatically, so
        // a post's whole lifecycle is greppable. `id` is the namespaced dedup
        // key (`ig:`/`th:` for posts, `th-share:` for unresolved aliases).
        let span = tracing::info_span!("job", seq, id = %job.dedup_key(), chat = job.chat_id.0);
        process_job(&bot, &chains, &http, &cfg, &dedup, &metrics, &job)
            .instrument(span)
            .await;
        // Pace requests so we don't hammer the upstream (PLAN §4.6).
        tokio::time::sleep(cfg.request_pacing).await;
    }
    tracing::warn!("worker channel closed; exiting");
}

/// Run one job under the wall-clock timeout, record its outcome accurately, and
/// release the dedup claim unless the post was actually delivered.
async fn process_job(
    bot: &TgBot,
    chains: &Chains,
    http: &reqwest::Client,
    cfg: &Config,
    dedup: &Dedup,
    metrics: &Metrics,
    queued: &QueuedJob,
) {
    let started = Instant::now();
    tracing::info!("processing job");
    let mut claims = ClaimSet::new(queued.dedup_key());
    let reply_context = queued.reply_context();
    let result = tokio::time::timeout(cfg.job_timeout, async {
        // Keep the indicator alive through share resolution, extraction, and
        // delivery. It is aborted when this future completes or is timed out.
        let _typing = sender::typing_indicator(bot, queued.chat_id);
        let job = match resolve_job(queued, chains, dedup, &mut claims).await {
            Ok(Some(job)) => job,
            Ok(None) => return Ok(Outcome::Duplicate),
            Err(error) => return report_extract_error(bot, &reply_context, error).await,
        };
        let Some(chain) = chains.select(&job) else {
            // A story link, but no login session is configured: say so rather
            // than spawn an extraction that is certain to hit the login wall.
            sender::reply_failure(bot, &job, NO_STORY_SESSION_NOTICE).await?;
            return Ok(Outcome::Unavailable);
        };
        handle(bot, chain, http, cfg, &job).await
    })
    .await;
    match result {
        // Delivered → keep the claim (TTL-dedups re-posts).
        Ok(Ok(Outcome::Delivered { media })) => {
            metrics.record_succeeded();
            tracing::info!(media, elapsed_ms = elapsed_ms(started), "delivered");
        }
        Ok(Ok(Outcome::Duplicate)) => {
            claims.release_all(dedup).await;
            tracing::info!(
                elapsed_ms = elapsed_ms(started),
                "resolved canonical post already deduped"
            );
        }
        // Unavailable / failed / timed out all release the claim so a re-post
        // can retry — only a delivered post stays deduped (PLAN §4.6).
        Ok(Ok(Outcome::Unavailable)) => {
            metrics.record_failed();
            claims.release_all(dedup).await;
            tracing::info!(
                elapsed_ms = elapsed_ms(started),
                "post unavailable; user notified"
            );
        }
        Ok(Err(e)) => {
            metrics.record_failed();
            claims.release_all(dedup).await;
            tracing::warn!(error = %e, elapsed_ms = elapsed_ms(started), "job failed");
        }
        Err(_) => {
            metrics.record_timed_out();
            claims.release_all(dedup).await;
            tracing::warn!(elapsed_ms = elapsed_ms(started), "job timed out");
            let _ = sender::reply_failure(
                bot,
                &reply_context,
                "⏱️ That one took too long to fetch — try again later.",
            )
            .await;
        }
    }
}

async fn resolve_job(
    queued: &QueuedJob,
    chains: &Chains,
    dedup: &Dedup,
    claims: &mut ClaimSet,
) -> Result<Option<Job>, ExtractError> {
    let (original_url, shortcode) = match &queued.target {
        LinkTarget::Post {
            canonical_url,
            shortcode,
        } => (canonical_url.clone(), shortcode.clone()),
        LinkTarget::Story { canonical_url, id } => (canonical_url.clone(), id.clone()),
        LinkTarget::ThreadsShare { share_url, .. } => {
            if queued.platform != Platform::Threads {
                return Err(ExtractError::Unavailable(
                    "share aliases are only valid for Threads".into(),
                ));
            }
            let resolved = chains.threads_share.resolve(share_url).await?;
            let canonical_key = queued.platform.dedup_key(&resolved.shortcode);
            if claims.claim(dedup, canonical_key).await {
                return Ok(None);
            }
            (resolved.canonical_url, resolved.shortcode)
        }
    };
    Ok(Some(Job {
        chat_id: queued.chat_id,
        reply_to: queued.reply_to,
        platform: queued.platform,
        kind: queued.kind(),
        original_url,
        shortcode,
    }))
}

/// Cap the media of a story job at `cap` items (highlights can hold 100; one
/// album is 10). Returns `(kept, total)` when truncated, and appends a note to
/// the caption so the viewer knows the mirror is partial. `None` cap = no limit.
fn cap_story_media(post: &mut Post, cap: Option<usize>) -> Option<(usize, usize)> {
    let cap = cap?;
    let total = post.media.len();
    if total <= cap {
        return None;
    }
    post.media.truncate(cap);
    let note = format!("Showing the first {cap} of {total} items.");
    post.caption = Some(match post.caption.take() {
        Some(c) if !c.trim().is_empty() => format!("{c}\n\n{note}"),
        _ => note,
    });
    Some((cap, total))
}

/// User-facing wording for an extraction failure, by content kind. Stories
/// expire after 24 h and depend on the bot's login session, so their copy
/// names those causes instead of the post-oriented "private / removed".
fn failure_copy(job: &Job, error: &ExtractError) -> String {
    match (job.kind, error) {
        (ContentKind::Story, ExtractError::NotFound) => {
            "🤷 Couldn't find that story — it may have expired (stories last 24 h) or belong to a private account.".to_string()
        }
        (ContentKind::Story, ExtractError::Blocked) => {
            "🔒 Couldn't fetch that story — Instagram rejected the bot's login session (it may have expired).".to_string()
        }
        (ContentKind::Post, ExtractError::NotFound) => {
            "🤷 Couldn't find that post — it may be private, removed, or image-only behind a login.".to_string()
        }
        _ => format!(
            "⚠️ Couldn't fetch that one — {} may be rate-limiting right now.",
            job.platform.label()
        ),
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
        Ok(mut post) => {
            if job.kind == ContentKind::Story {
                if let Some((kept, total)) = cap_story_media(&mut post, cfg.ig_story_max_items) {
                    tracing::info!(kept, total, "story items capped");
                }
            }
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
        Err(error) => report_extract_error(bot, job, error).await,
    }
}

async fn report_extract_error(
    bot: &TgBot,
    job: &Job,
    error: ExtractError,
) -> anyhow::Result<Outcome> {
    let copy = failure_copy(job, &error);
    sender::reply_failure(bot, job, &copy).await?;
    match error {
        ExtractError::NotFound => Ok(Outcome::Unavailable),
        e => Err(anyhow::anyhow!("all backends failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn queued_share_job_uses_alias_ingress_key() {
        let queued = QueuedJob {
            chat_id: ChatId(1),
            reply_to: MessageId(2),
            platform: Platform::Threads,
            target: LinkTarget::ThreadsShare {
                share_url: "https://www.threads.com/share/ALIAS/".into(),
                token: "ALIAS".into(),
            },
        };
        assert_eq!(queued.dedup_key(), "th-share:ALIAS");
    }

    #[tokio::test]
    async fn share_failure_releases_alias_and_canonical_claims() {
        let dedup = Dedup::new(Duration::from_secs(60));
        let alias = "th-share:ALIAS".to_string();
        let canonical = "th:POST".to_string();
        assert!(
            !dedup.seen_or_claim(&alias).await,
            "handler owns alias claim"
        );

        let mut claims = ClaimSet::new(alias.clone());
        assert!(
            !claims.claim(&dedup, canonical.clone()).await,
            "canonical was new"
        );
        claims.release_all(&dedup).await;

        assert!(
            !dedup.seen_or_claim(&alias).await,
            "alias released for retry"
        );
        assert!(
            !dedup.seen_or_claim(&canonical).await,
            "canonical released for retry"
        );
    }

    #[tokio::test]
    async fn canonical_duplicate_releases_only_the_alias_claim() {
        let dedup = Dedup::new(Duration::from_secs(60));
        let alias = "th-share:ALIAS".to_string();
        let canonical = "th:POST".to_string();
        assert!(!dedup.seen_or_claim(&alias).await);
        assert!(
            !dedup.seen_or_claim(&canonical).await,
            "another job owns canonical"
        );

        let mut claims = ClaimSet::new(alias.clone());
        assert!(
            claims.claim(&dedup, canonical.clone()).await,
            "canonical is duplicate"
        );
        claims.release_all(&dedup).await;

        assert!(
            !dedup.seen_or_claim(&alias).await,
            "temporary alias released"
        );
        assert!(
            dedup.seen_or_claim(&canonical).await,
            "foreign canonical claim retained"
        );
    }

    #[tokio::test]
    async fn successful_share_keeps_alias_and_canonical_claims() {
        let dedup = Dedup::new(Duration::from_secs(60));
        let alias = "th-share:ALIAS".to_string();
        let canonical = "th:POST".to_string();
        assert!(!dedup.seen_or_claim(&alias).await);

        let mut claims = ClaimSet::new(alias.clone());
        assert!(!claims.claim(&dedup, canonical.clone()).await);
        drop(claims); // delivery success deliberately retains owned claims

        assert!(dedup.seen_or_claim(&alias).await);
        assert!(dedup.seen_or_claim(&canonical).await);
    }
}

#[cfg(test)]
mod story_tests {
    use super::*;
    use crate::extract::{Media, Post};

    fn job(platform: Platform, kind: ContentKind) -> Job {
        Job {
            chat_id: ChatId(1),
            reply_to: MessageId(2),
            platform,
            kind,
            original_url: "https://www.instagram.com/stories/u/123/".into(),
            shortcode: "123".into(),
        }
    }

    fn chains(story: Option<Arc<ExtractorChain>>) -> Chains {
        Chains {
            instagram: Arc::new(ExtractorChain::new(vec![])),
            instagram_story: story,
            threads: Arc::new(ExtractorChain::new_accepting_textonly(vec![])),
            threads_share: Arc::new(
                ThreadsShareResolver::new("ua".into(), "ch".into()).expect("resolver"),
            ),
        }
    }

    fn post_with(n: usize, caption: Option<&str>) -> Post {
        Post {
            author: Some("u".into()),
            caption: caption.map(str::to_string),
            media: (0..n)
                .map(|i| Media::image(format!("https://scontent.cdninstagram.com/v/{i}.jpg")))
                .collect(),
            original_url: "x".into(),
        }
    }

    #[test]
    fn story_job_carries_story_kind_and_namespaced_key() {
        let queued = QueuedJob {
            chat_id: ChatId(1),
            reply_to: MessageId(2),
            platform: Platform::Instagram,
            target: LinkTarget::Story {
                canonical_url: "https://www.instagram.com/stories/u/123/".into(),
                id: "123".into(),
            },
        };
        assert_eq!(queued.dedup_key(), "ig-story:123");
        let job = queued.reply_context();
        assert_eq!(job.kind, ContentKind::Story);
        assert_eq!(job.shortcode, "123");
        let post = QueuedJob {
            target: LinkTarget::Post {
                canonical_url: "https://www.instagram.com/p/ABC/".into(),
                shortcode: "ABC".into(),
            },
            ..queued
        };
        assert_eq!(post.reply_context().kind, ContentKind::Post);
    }

    #[test]
    fn stories_route_to_their_own_chain_which_is_absent_without_cookies() {
        let no_cookies = chains(None);
        let post = job(Platform::Instagram, ContentKind::Post);
        let story = job(Platform::Instagram, ContentKind::Story);
        assert!(no_cookies
            .select(&post)
            .is_some_and(|c| std::ptr::eq(c, &*no_cookies.instagram)));
        assert!(
            no_cookies.select(&story).is_none(),
            "no cookies -> no story chain"
        );

        let story_chain = Arc::new(ExtractorChain::new(vec![]));
        let with_cookies = chains(Some(story_chain.clone()));
        assert!(with_cookies
            .select(&story)
            .is_some_and(|c| std::ptr::eq(c, &*story_chain)));
        assert!(with_cookies
            .select(&job(Platform::Threads, ContentKind::Post))
            .is_some_and(|c| std::ptr::eq(c, &*with_cookies.threads)));
    }

    #[test]
    fn story_cap_truncates_and_annotates_caption() {
        let mut post = post_with(25, Some("Rare"));
        assert_eq!(cap_story_media(&mut post, Some(10)), Some((10, 25)));
        assert_eq!(post.media.len(), 10);
        assert_eq!(
            post.caption.as_deref(),
            Some("Rare\n\nShowing the first 10 of 25 items.")
        );
    }

    #[test]
    fn story_cap_note_stands_alone_without_caption() {
        let mut post = post_with(12, None);
        assert_eq!(cap_story_media(&mut post, Some(10)), Some((10, 12)));
        assert_eq!(
            post.caption.as_deref(),
            Some("Showing the first 10 of 12 items.")
        );
    }

    #[test]
    fn story_cap_leaves_small_or_uncapped_posts_alone() {
        let mut small = post_with(3, None);
        assert_eq!(cap_story_media(&mut small, Some(10)), None);
        assert_eq!(small.media.len(), 3);
        assert_eq!(small.caption, None);
        let mut big = post_with(25, Some("c"));
        assert_eq!(
            cap_story_media(&mut big, None),
            None,
            "unset cap = unlimited"
        );
        assert_eq!(big.media.len(), 25);
        assert_eq!(big.caption.as_deref(), Some("c"));
    }

    #[test]
    fn story_failures_get_story_specific_copy() {
        let story = job(Platform::Instagram, ContentKind::Story);
        assert!(failure_copy(&story, &ExtractError::NotFound).contains("expired"));
        assert!(failure_copy(&story, &ExtractError::Blocked).contains("login session"));
        // Posts keep their existing wording.
        let post = job(Platform::Instagram, ContentKind::Post);
        assert!(failure_copy(&post, &ExtractError::NotFound).contains("find that post"));
        assert!(
            failure_copy(&post, &ExtractError::Blocked).contains("Instagram may be rate-limiting")
        );
        let th = job(Platform::Threads, ContentKind::Post);
        assert!(failure_copy(&th, &ExtractError::Transient("x".into()))
            .contains("Threads may be rate-limiting"));
    }
}
