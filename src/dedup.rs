//! In-memory TTL dedup so the same shortcode isn't re-fetched within a window
//! (PLAN §4.6). Backed by moka; cheap to clone (shared internally).
//!
//! A claim is taken when a job is enqueued and **released on failure** (or if
//! the job is never enqueued), so a transient failure doesn't block retries for
//! the whole TTL — only a *successfully delivered* post stays deduped.

use moka::future::Cache;
use std::time::Duration;

#[derive(Clone)]
pub struct Dedup {
    cache: Cache<String, ()>,
}

impl Dedup {
    pub fn new(ttl: Duration) -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(10_000)
                .build(),
        }
    }

    /// Atomically claim a shortcode. Returns `true` if it was **already**
    /// claimed (caller should skip); `false` if this call claimed it. The
    /// claim is atomic across concurrent callers (no TOCTOU double-process).
    pub async fn seen_or_claim(&self, shortcode: &str) -> bool {
        let entry = self
            .cache
            .entry(shortcode.to_string())
            .or_insert_with(std::future::ready(()))
            .await;
        !entry.is_fresh()
    }

    /// Release a claim so the shortcode can be retried (on failure, or when a
    /// job couldn't be enqueued).
    pub async fn forget(&self, shortcode: &str) {
        self.cache.invalidate(shortcode).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claim_is_idempotent_and_releasable() {
        let d = Dedup::new(Duration::from_secs(60));
        assert!(!d.seen_or_claim("a").await, "first claim should win");
        assert!(d.seen_or_claim("a").await, "second claim should see it");
        d.forget("a").await;
        d.cache.run_pending_tasks().await; // flush the invalidation deterministically
        assert!(!d.seen_or_claim("a").await, "after forget it is claimable again");
    }
}
