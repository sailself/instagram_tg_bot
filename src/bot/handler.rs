//! Group-message handler: enforce the chat allowlist, scan text + entities for
//! Instagram links, dedup, and enqueue jobs for the worker (PLAN §3.1 / §5).

use crate::bot::TgBot;
use crate::config::Config;
use crate::dedup::Dedup;
use crate::queue::QueuedJob;
use crate::urls::{self, Platform};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageEntityKind, ReplyParameters};
use tokio::sync::mpsc::{error::TrySendError, Sender};

pub async fn on_message(
    bot: TgBot,
    msg: Message,
    cfg: Arc<Config>,
    tx: Sender<QueuedJob>,
    dedup: Dedup,
) -> ResponseResult<()> {
    if !cfg.chat_allowed(msg.chat.id.0) {
        return Ok(());
    }

    let haystack = collect_text(&msg);
    let links = urls::find_links(&haystack);
    if links.is_empty() {
        return Ok(());
    }

    // Who posted the link (username, else numeric id) — the audit record.
    let poster = msg
        .from
        .as_ref()
        .map(|u| u.username.clone().unwrap_or_else(|| u.id.0.to_string()));

    for link in links {
        // Threads support is a config-gated kill-switch; Instagram is always on.
        if link.platform == Platform::Threads && !cfg.threads_enabled {
            tracing::debug!(id = %link.target.id(), "threads disabled, skipping link");
            continue;
        }
        let dedup_key = link.dedup_key();
        if dedup.seen_or_claim(&dedup_key).await {
            tracing::debug!(key = %dedup_key, "duplicate within TTL, skipping");
            continue;
        }
        tracing::info!(
            platform = ?link.platform,
            id = %link.target.id(),
            chat = msg.chat.id.0,
            user = poster.as_deref().unwrap_or("?"),
            "link detected"
        );
        let job = QueuedJob {
            chat_id: msg.chat.id,
            reply_to: msg.id,
            platform: link.platform,
            target: link.target,
        };
        match tx.try_send(job) {
            Ok(()) => {}
            // Couldn't enqueue → release the claim so the suggested retry works.
            Err(TrySendError::Full(job)) => {
                dedup.forget(&job.dedup_key()).await;
                let _ = bot
                    .send_message(
                        msg.chat.id,
                        "🐢 Busy right now — try that link again in a moment.",
                    )
                    .reply_parameters(ReplyParameters::new(msg.id))
                    .await;
            }
            Err(TrySendError::Closed(job)) => {
                dedup.forget(&job.dedup_key()).await;
                tracing::error!("job channel closed; worker is gone");
            }
        }
    }
    Ok(())
}

/// Gather scannable text: message text, caption, and any `text_link` entity
/// targets (plain `url` entities are already in the literal text).
fn collect_text(msg: &Message) -> String {
    let mut out = String::new();
    if let Some(t) = msg.text() {
        out.push_str(t);
        out.push(' ');
    }
    if let Some(c) = msg.caption() {
        out.push_str(c);
        out.push(' ');
    }
    for ents in [msg.entities(), msg.caption_entities()]
        .into_iter()
        .flatten()
    {
        for e in ents {
            if let MessageEntityKind::TextLink { url } = &e.kind {
                out.push_str(url.as_str());
                out.push(' ');
            }
        }
    }
    out
}
