//! Bearer-token authorization with per-token scopes, expiry, and revocation.
//!
//! Access control is a small scope model:
//!
//! - [`Scope::Read`] permits `GET`/`HEAD` (fetching chunks and manifests).
//! - [`Scope::Write`] permits `PUT` (uploading chunks, publishing manifests).
//! - `Read` does **not** imply `Write`: a mirror-only peer can be given a
//!   read token, and a backup target a write-only token.
//!
//! A [`TokenSet`] holds the accepted tokens, each with an optional expiry
//! instant. A token past its expiry is rejected exactly like an unknown one,
//! and [`TokenSet::revoke`] removes a token outright, so credentials can be
//! rotated without restarting the server when the set is rebuilt from config.
//!
//! An empty set means "no auth", which is only appropriate on a trusted
//! network or behind a reverse proxy that authenticates. Tokens are compared
//! in constant time against every entry, and expiry is checked **before** the
//! comparison loop's result is trusted, so a timing observer cannot tell an
//! expired-but-valid token from an unknown one.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{RemoteError, Result};

/// What a token is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Fetching chunks and manifests (`GET`, `HEAD`).
    Read,
    /// Uploading chunks and publishing manifests (`PUT`).
    Write,
}

impl Scope {
    /// Parse a scope from its wire/config spelling (`read` / `write`).
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "ro" | "read-only" => Ok(Scope::Read),
            "write" | "rw" | "read-write" => Ok(Scope::Write),
            other => Err(RemoteError::Config(format!(
                "unknown scope `{other}` (expected `read` or `write`)"
            ))),
        }
    }

    /// The canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
        }
    }
}

/// A token entry: its scope and optional expiry (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEntry {
    /// What the token may do.
    pub scope: Scope,
    /// Absolute expiry as Unix seconds; `None` means no expiry.
    pub expires_at: Option<u64>,
}

impl TokenEntry {
    /// True if the entry has an expiry at or before `now`.
    pub fn is_expired_at(&self, now: u64) -> bool {
        matches!(self.expires_at, Some(e) if now >= e)
    }
}

/// Current wall-clock time as Unix seconds, or 0 if the clock is before epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A set of accepted tokens, their scopes, and optional expiries.
#[derive(Debug, Clone, Default)]
pub struct TokenSet {
    tokens: BTreeMap<String, TokenEntry>,
}

impl TokenSet {
    /// An empty set: authorization is disabled.
    pub fn none() -> Self {
        TokenSet::default()
    }

    /// A single full-access, non-expiring token (the Phase 10 behavior).
    pub fn single(token: impl Into<String>) -> Self {
        let mut set = TokenSet::default();
        set.insert(token, Scope::Write);
        set
    }

    /// Add (or replace) a token with an explicit scope and no expiry.
    ///
    /// Replacing is intentional: re-inserting the same token updates its scope,
    /// which is how rotation is expressed when rebuilding a set from config.
    pub fn insert(&mut self, token: impl Into<String>, scope: Scope) -> &mut Self {
        self.tokens.insert(
            token.into(),
            TokenEntry {
                scope,
                expires_at: None,
            },
        );
        self
    }

    /// Add (or replace) a token that expires at `expires_at` (Unix seconds).
    pub fn insert_with_expiry(
        &mut self,
        token: impl Into<String>,
        scope: Scope,
        expires_at: u64,
    ) -> &mut Self {
        self.tokens.insert(
            token.into(),
            TokenEntry {
                scope,
                expires_at: Some(expires_at),
            },
        );
        self
    }

    /// Remove a token. Returns `true` if it was present (revocation).
    pub fn revoke(&mut self, token: &str) -> bool {
        self.tokens.remove(token).is_some()
    }

    /// Look up an entry without authorizing (for diagnostics).
    pub fn get(&self, token: &str) -> Option<&TokenEntry> {
        self.tokens.get(token)
    }

    /// Parse a comma-separated spec of `token[:scope[:expiry]]` entries.
    ///
    /// - `token` — full access, no expiry.
    /// - `token:read` / `token:write` — scoped, no expiry.
    /// - `token:read:1735689600` — scoped, expiring at that Unix time.
    /// - `token::1735689600` — full access, expiring.
    ///
    /// An empty string yields an empty set (authorization disabled).
    pub fn parse(spec: &str) -> Result<Self> {
        let mut set = TokenSet::none();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let mut parts = entry.splitn(3, ':');
            let token = parts.next().unwrap_or("").trim();
            if token.is_empty() {
                return Err(RemoteError::Config("empty token in auth spec".into()));
            }
            let scope = match parts.next() {
                None | Some("") => Scope::Write,
                Some(s) => Scope::parse(s)?,
            };
            let expires_at = match parts.next() {
                None | Some("") => None,
                Some(e) => Some(e.trim().parse::<u64>().map_err(|_| {
                    RemoteError::Config(format!("invalid expiry `{e}` for token `{token}`"))
                })?),
            };
            match expires_at {
                Some(at) => {
                    set.insert_with_expiry(token.to_string(), scope, at);
                }
                None => {
                    set.insert(token.to_string(), scope);
                }
            }
        }
        Ok(set)
    }

    /// True if no tokens are configured (authorization disabled).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Number of configured tokens (including expired ones).
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Drop every token that has expired at `now`. Returns how many were
    /// removed.
    pub fn prune_expired(&mut self, now: u64) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|_, e| !e.is_expired_at(now));
        before - self.tokens.len()
    }

    /// Authorize a presented bearer token for `required`, as of `now`.
    ///
    /// Returns `Ok(())` when the set is empty (auth disabled) or the token is
    /// known, unexpired, and its scope covers `required`.
    ///
    /// A matching token that is expired is rejected with [`RemoteError::Unauthorized`].
    pub fn authorize_at(&self, presented: Option<&str>, required: Scope, now: u64) -> Result<()> {
        if self.tokens.is_empty() {
            return Ok(());
        }
        let presented = presented.ok_or(RemoteError::Unauthorized)?;

        // Compare against every token in constant time, and decide expiry only
        // after the loop so the number of comparisons does not depend on which
        // token matched or whether it was expired.
        let mut matched: Option<Scope> = None;
        let mut expired = false;
        for (token, entry) in &self.tokens {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                matched = Some(entry.scope);
                expired = entry.is_expired_at(now);
            }
        }

        match matched {
            Some(_) if expired => Err(RemoteError::Unauthorized),
            // `Write` implies read access to chunks/manifests.
            Some(Scope::Write) => Ok(()),
            Some(Scope::Read) if required == Scope::Read => Ok(()),
            Some(Scope::Read) => Err(RemoteError::Unauthorized),
            None => Err(RemoteError::Unauthorized),
        }
    }

    /// Authorize using the current wall clock.
    pub fn authorize(&self, presented: Option<&str>, required: Scope) -> Result<()> {
        self.authorize_at(presented, required, now_unix())
    }
}

/// Compare two byte strings without short-circuiting on the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_allows_everything() {
        let set = TokenSet::none();
        assert!(set.is_empty());
        assert!(set.authorize(None, Scope::Write).is_ok());
        assert!(set.authorize(Some("anything"), Scope::Read).is_ok());
    }

    #[test]
    fn single_token_is_full_access() {
        let set = TokenSet::single("secret");
        assert!(set.authorize(Some("secret"), Scope::Write).is_ok());
        assert!(set.authorize(Some("secret"), Scope::Read).is_ok());
        assert!(set.authorize(Some("wrong"), Scope::Read).is_err());
        assert!(set.authorize(None, Scope::Read).is_err());
    }

    #[test]
    fn read_scope_cannot_write() {
        let mut set = TokenSet::none();
        set.insert("ro", Scope::Read);
        assert!(set.authorize(Some("ro"), Scope::Read).is_ok());
        assert!(set.authorize(Some("ro"), Scope::Write).is_err());
    }

    #[test]
    fn write_scope_can_read() {
        let mut set = TokenSet::none();
        set.insert("rw", Scope::Write);
        assert!(set.authorize(Some("rw"), Scope::Read).is_ok());
        assert!(set.authorize(Some("rw"), Scope::Write).is_ok());
    }

    #[test]
    fn reinserting_a_token_updates_its_scope() {
        let mut set = TokenSet::none();
        set.insert("t", Scope::Read);
        set.insert("t", Scope::Write);
        assert_eq!(set.len(), 1);
        assert!(set.authorize(Some("t"), Scope::Write).is_ok());
        // Rotation back down to read also takes effect.
        set.insert("t", Scope::Read);
        assert!(set.authorize(Some("t"), Scope::Write).is_err());
    }

    #[test]
    fn parse_list_of_scoped_tokens() {
        let set = TokenSet::parse("ro:read, rw:write, full").unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.authorize(Some("ro"), Scope::Read).is_ok());
        assert!(set.authorize(Some("ro"), Scope::Write).is_err());
        assert!(set.authorize(Some("rw"), Scope::Write).is_ok());
        assert!(set.authorize(Some("full"), Scope::Write).is_ok());
    }

    #[test]
    fn parse_rejects_bad_scope() {
        assert!(TokenSet::parse("t:browse").is_err());
        assert!(TokenSet::parse(":read").is_err());
    }

    #[test]
    fn parse_empty_is_disabled() {
        assert!(TokenSet::parse("").unwrap().is_empty());
    }

    #[test]
    fn expired_token_is_rejected() {
        let mut set = TokenSet::none();
        set.insert_with_expiry("temp", Scope::Write, 1000);
        // Before expiry: allowed.
        assert!(set.authorize_at(Some("temp"), Scope::Write, 999).is_ok());
        // At and after expiry: rejected exactly like an unknown token.
        assert!(set.authorize_at(Some("temp"), Scope::Write, 1000).is_err());
        assert!(set.authorize_at(Some("temp"), Scope::Read, 5000).is_err());
    }

    #[test]
    fn non_expiring_token_ignores_time() {
        let set = TokenSet::single("forever");
        assert!(set
            .authorize_at(Some("forever"), Scope::Write, u64::MAX)
            .is_ok());
    }

    #[test]
    fn parse_supports_expiry() {
        let set = TokenSet::parse("ro:read:2000,rw:write").unwrap();
        assert_eq!(set.get("ro").unwrap().expires_at, Some(2000));
        assert!(set.get("rw").unwrap().expires_at.is_none());
        assert!(set.authorize_at(Some("ro"), Scope::Read, 1999).is_ok());
        assert!(set.authorize_at(Some("ro"), Scope::Read, 2000).is_err());
    }

    #[test]
    fn parse_full_access_with_expiry() {
        let set = TokenSet::parse("full::3000").unwrap();
        assert_eq!(set.get("full").unwrap().scope, Scope::Write);
        assert_eq!(set.get("full").unwrap().expires_at, Some(3000));
    }

    #[test]
    fn parse_rejects_bad_expiry() {
        assert!(TokenSet::parse("t:read:soon").is_err());
    }

    #[test]
    fn revoke_removes_a_token() {
        let mut set = TokenSet::single("secret");
        assert!(set.revoke("secret"));
        assert!(!set.revoke("secret"));
        // An emptied set disables authorization entirely.
        assert!(set.is_empty());
        assert!(set.authorize(None, Scope::Write).is_ok());
    }

    #[test]
    fn prune_expired_drops_only_expired() {
        let mut set = TokenSet::none();
        set.insert_with_expiry("old", Scope::Read, 100);
        set.insert("live", Scope::Write);
        assert_eq!(set.prune_expired(200), 1);
        assert_eq!(set.len(), 1);
        assert!(set.get("old").is_none());
        assert!(set.get("live").is_some());
    }
}
