//! A provider credential that expires and is renewed when a request needs it.
//!
//! A gateway or channel builds its provider once and keeps it for days, so a
//! token resolved at build time is sent long after it expired. This holds the
//! current token in memory, hands it out while it has life left, and calls the
//! renewal function when it does not.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A token with this much life left, or less, is renewed before it is used.
pub(crate) const RENEWAL_MARGIN: Duration = Duration::from_mins(2);

/// How long a refreshed token is assumed to last when the refresh response
/// names no expiry.
pub(crate) const ASSUMED_LIFETIME: Duration = Duration::from_mins(30);

/// An access token and the moment it stops working.
#[derive(Clone)]
pub(crate) struct RenewedToken {
    pub(crate) token: String,
    /// `None` when the source gives no expiry; such a token is never renewed.
    pub(crate) expires_at: Option<SystemTime>,
}

impl RenewedToken {
    pub(crate) fn has_life_beyond_margin(&self) -> bool {
        self.expires_at
            .is_none_or(|expires_at| expires_at > SystemTime::now() + RENEWAL_MARGIN)
    }
}

type RenewFn = dyn Fn() -> anyhow::Result<RenewedToken> + Send + Sync;

/// The current token plus the function that replaces it.
///
/// The renewal function may block (file reads, a blocking HTTP client), so it
/// runs on the blocking pool and never on the async runtime. The token lives
/// behind an async mutex that stays locked during a renewal: concurrent
/// requests wait for it and then use its result, so one expiry causes one
/// renewal.
pub(crate) struct RenewingCredential {
    held: tokio::sync::Mutex<Option<RenewedToken>>,
    renew: Arc<RenewFn>,
}

impl RenewingCredential {
    /// `held` is the token known at build time, if any. Building never calls
    /// `renew`.
    pub(crate) fn new(
        held: Option<RenewedToken>,
        renew: impl Fn() -> anyhow::Result<RenewedToken> + Send + Sync + 'static,
    ) -> Self {
        Self {
            held: tokio::sync::Mutex::new(held),
            renew: Arc::new(renew),
        }
    }

    /// A token to send now. A failed renewal is returned as the error and
    /// leaves the held token as it was, so the next request tries again.
    pub(crate) async fn current(&self) -> anyhow::Result<String> {
        let mut held = self.held.lock().await;
        if let Some(token) = held.as_ref().filter(|token| token.has_life_beyond_margin()) {
            return Ok(token.token.clone());
        }

        let renew = Arc::clone(&self.renew);
        let renewed = tokio::task::spawn_blocking(move || renew())
            .await
            .map_err(|error| anyhow::anyhow!("credential renewal did not finish: {error}"))??;
        let token = renewed.token.clone();
        *held = Some(renewed);
        Ok(token)
    }
}
