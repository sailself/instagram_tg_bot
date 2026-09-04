//! Group-message handler: enforce the chat allowlist, scan text + entities for
//! Instagram links, dedup, and enqueue jobs for the worker (PLAN §3.1 / §5).

use crate::bot::TgBot;
use crate::config::Config;
use crate::dedup::Dedup;
use crate::queue::QueuedJob;
use crate::urls::{self, DetectedLink, LinkTarget, Platform};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{Me, MessageEntityKind, ReplyParameters, UserId};
use tokio::sync::mpsc::{error::TrySendError, Sender};

pub async fn on_message(
    bot: TgBot,
    msg: Message,
    me: Me,
    cfg: Arc<Config>,
    tx: Sender<QueuedJob>,
    dedup: Dedup,
) -> ResponseResult<()> {
    if !cfg.chat_allowed(msg.chat.id.0) {
        return Ok(());
    }
    // The bot's own replies end with a `🔗 <post url>` footer. Forwarded into a
    // chat (from anywhere), that footer would otherwise make the bot mirror
    // its own mirror. `Me` is injected by teloxide's dispatcher at startup.
    if is_from_self(&msg, me.id) {
        tracing::debug!(
            chat = msg.chat.id.0,
            "ignoring the bot's own (forwarded) message"
        );
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
        if !link_enabled(&cfg, &link) {
            tracing::debug!(
                platform = ?link.platform,
                id = %link.target.id(),
                "link type disabled by config, skipping"
            );
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

/// True when this bot authored the message: it is a forward of one of the bot's
/// own messages (Telegram records the original sender in `forward_origin`), or
/// — defensively — the bot itself is the sender. Only *this* bot is filtered;
/// other bots' and users' forwards are processed normally. A forward whose
/// origin Telegram hides (privacy setting, channel post) is not attributable
/// and is processed.
fn is_from_self(msg: &Message, me: UserId) -> bool {
    msg.forward_from_user().is_some_and(|u| u.id == me)
        || msg.from.as_ref().is_some_and(|u| u.id == me)
}

/// Config kill-switches: Threads (`THREADS_ENABLED`) and Instagram Stories
/// (`IG_STORIES_ENABLED`). Instagram posts are always on.
fn link_enabled(cfg: &Config, link: &DetectedLink) -> bool {
    if link.platform == Platform::Threads && !cfg.threads_enabled {
        return false;
    }
    if matches!(link.target, LinkTarget::Story { .. }) && !cfg.ig_stories_enabled {
        return false;
    }
    true
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

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::UserId;

    const BOT_ID: u64 = 7_000_000_001;

    /// A group message as Telegram delivers it, with an optional `forward_origin`
    /// block spliced in. Deserializing the real `Message` type keeps the test
    /// honest about the shape the guard must read.
    fn message(from_id: u64, forward_origin: Option<&str>) -> Message {
        let origin = forward_origin
            .map(|o| format!(r#""forward_origin": {o},"#))
            .unwrap_or_default();
        let json = format!(
            r#"{{
                "message_id": 10,
                "date": 1725000000,
                "chat": {{"id": -1001234567890, "title": "g", "type": "supergroup"}},
                "from": {{"id": {from_id}, "is_bot": false, "first_name": "Ann"}},
                {origin}
                "text": "https://www.instagram.com/p/ABC123/"
            }}"#
        );
        serde_json::from_str(&json).expect("valid message json")
    }

    fn user_origin(id: u64, is_bot: bool) -> String {
        format!(
            r#"{{"type": "user", "date": 1724990000,
                 "sender_user": {{"id": {id}, "is_bot": {is_bot}, "first_name": "x"}}}}"#
        )
    }

    #[test]
    fn plain_member_message_is_not_from_self() {
        assert!(!is_from_self(&message(42, None), UserId(BOT_ID)));
    }

    #[test]
    fn forward_of_the_bots_own_reply_is_from_self() {
        let msg = message(42, Some(&user_origin(BOT_ID, true)));
        assert!(is_from_self(&msg, UserId(BOT_ID)));
    }

    #[test]
    fn forward_from_another_user_or_bot_is_not_from_self() {
        let human = message(42, Some(&user_origin(99, false)));
        assert!(!is_from_self(&human, UserId(BOT_ID)));
        // Another bot's message must still be processed — only *this* bot is
        // filtered, as requested.
        let other_bot = message(42, Some(&user_origin(7_000_000_002, true)));
        assert!(!is_from_self(&other_bot, UserId(BOT_ID)));
    }

    #[test]
    fn forward_from_hidden_user_or_channel_is_not_from_self() {
        let hidden = message(
            42,
            Some(r#"{"type": "hidden_user", "date": 1724990000, "sender_user_name": "Someone"}"#),
        );
        assert!(!is_from_self(&hidden, UserId(BOT_ID)));
        let channel = message(
            42,
            Some(
                r#"{"type": "channel", "date": 1724990000, "message_id": 5,
                    "chat": {"id": -1009876543210, "title": "c", "type": "channel"}}"#,
            ),
        );
        assert!(!is_from_self(&channel, UserId(BOT_ID)));
    }

    #[test]
    fn message_sent_by_the_bot_itself_is_from_self() {
        // Defensive: the Bot API doesn't echo a bot's own messages, but if one
        // ever arrives (e.g. via a linked channel), it must not be re-parsed.
        assert!(is_from_self(&message(BOT_ID, None), UserId(BOT_ID)));
    }
}

#[cfg(test)]
mod story_tests {
    use super::*;

    #[test]
    fn story_kill_switch_gates_story_links_only() {
        let mut cfg = Config::test_default();
        let story = urls::find_links("https://www.instagram.com/stories/u/1/").remove(0);
        let post = urls::find_links("https://www.instagram.com/p/AAA/").remove(0);
        let threads = urls::find_links("https://www.threads.com/@u/post/BBB").remove(0);
        assert!(link_enabled(&cfg, &story));
        assert!(link_enabled(&cfg, &post));
        assert!(link_enabled(&cfg, &threads));
        cfg.ig_stories_enabled = false;
        assert!(
            !link_enabled(&cfg, &story),
            "stories off -> story links ignored"
        );
        assert!(link_enabled(&cfg, &post), "posts unaffected");
        cfg.threads_enabled = false;
        assert!(!link_enabled(&cfg, &threads));
    }
}
