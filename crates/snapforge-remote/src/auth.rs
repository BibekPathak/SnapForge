//! Bearer-token authorization with per-token scopes.
//!
//! The Phase 10 server accepted a single shared token that granted full
//! access. Phase 13 replaces that with a small scope model:
//!
//! - [`Scope::Read`] permits `GET`/`HEAD` (fetching chunks and manifests).
//! - [`Scope::Write`] permits `PUT` (uploading chunks, publishing manifests).
//! - `Read` does **not** imply `Write`: a mirror-only peer can be given a
//!   read token, and a backup target a write-only token.
//!
//! A [`TokenSet`] holds the accepted tokens. An empty set means "no auth",
//! which is only appropriate on a trusted network or behind a reverse proxy
//! that authenticates. Tokens are compared in constant time.

use std::collections::BTreeMap;

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

/// A set of accepted tokens and their scopes.
#[derive(Debug, Clone, Default)]
pub struct TokenSet {
    tokens: BTreeMap<String, Scope>,
}

impl TokenSet {
    /// An empty set: authorization is disabled.
    pub fn none() -> Self {
        TokenSet::default()
    }

    /// A single full-access token (the Phase 10 behavior).
    pub fn single(token: impl Into<String>) -> Self {
        let mut set = TokenSet::default();
        set.tokens.insert(token.into(), Scope::Write);
        set
    }

    /// Add a token with an explicit scope. A later entry for the same token
    /// with a broader scope wins, so `write` implies `read` when configured.
    pub fn insert(&mut self, token: impl Into<String>, scope: Scope) -> &mut Self {
        self.tokens
            .entry(token.into())
            .and_modify(|existing| {
                if scope == Scope::Write {
                    *existing = Scope::Write;
                }
            })
            .or_insert(scope);
        self
    }

    /// Parse a `token:scope` list (for example `abc:read,def:write`).
    ///
    /// A bare `token` (no colon) is full access. An empty string yields an
    /// empty set (authorization disabled).
    pub fn parse(spec: &str) -> Result<Self> {
        let mut set = TokenSet::none();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once(':') {
                Some((token, scope)) => {
                    let token = token.trim();
                    if token.is_empty() {
                        return Err(RemoteError::Config("empty token in auth spec".into()));
                    }
                    set.insert(token, Scope::parse(scope)?);
                }
                None => {
                    set.insert(entry, Scope::Write);
                }
            }
        }
        Ok(set)
    }

    /// True if no tokens are configured (authorization disabled).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Number of configured tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Authorize a presented bearer token for `required`.
    ///
    /// Returns `Ok(())` when the set is empty (auth disabled) or the token is
    /// known and its scope covers `required`.
    pub fn authorize(&self, presented: Option<&str>, required: Scope) -> Result<()> {
        if self.tokens.is_empty() {
            return Ok(());
        }
        let presented = presented.ok_or_else(|| RemoteError::Unauthorized)?;
        // Compare against every token in constant time to avoid leaking which
        // token matched (or how long a shared prefix is).
        let mut matched: Option<Scope> = None;
        for (token, scope) in &self.tokens {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                matched = Some(*scope);
            }
        }
        match matched {
            // `Write` implies read access to chunks/manifests.
            Some(Scope::Write) => Ok(()),
            Some(Scope::Read) if required == Scope::Read => Ok(()),
            Some(Scope::Read) => Err(RemoteError::Unauthorized),
            None => Err(RemoteError::Unauthorized),
        }
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
    fn upgrade_to_write_wins() {
        let mut set = TokenSet::none();
        set.insert("t", Scope::Read);
        set.insert("t", Scope::Write);
        assert_eq!(set.len(), 1);
        assert!(set.authorize(Some("t"), Scope::Write).is_ok());
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
}
