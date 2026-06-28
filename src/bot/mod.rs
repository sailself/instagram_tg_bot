//! Telegram-facing layer: the update handler and the reply/sender logic.

pub mod handler;
pub mod sender;

/// The bot type used throughout the app: a rate-limit-throttled Bot (PLAN §5).
pub type TgBot = teloxide::adaptors::throttle::Throttle<teloxide::Bot>;
