//! Reload-safe browser reauthorization primitives (WI 0160, Layer 1).
//!
//! A browser enrolls an ECDSA P-256 public key with a link. After the owner
//! approves the link, the key may prove possession over a one-use challenge
//! to recover the **same** grant (ID, bearer and original expiry). This
//! module owns the mechanics; policy is `command::daemon_api::browser`.
//!
//! * [`ReauthPublicKey`] parses exactly `0x04 || X[32] || Y[32]` in canonical
//!   unpadded base64url and rejects points that are not on P-256 (validated
//!   with ring's ECDH peer-key check; no curve arithmetic lives here).
//! * [`ReauthPublicKey::verify`] verifies a raw 64-byte `r || s` signature
//!   (WebCrypto's format, not DER) with ring's `ECDSA_P256_SHA256_FIXED`,
//!   hashing the message exactly once. High-S and low-S are both accepted;
//!   single-use challenges, not normalization, defeat malleated replay.
//! * [`transcript`] is the exact signed byte string of C3.
//! * [`ChallengeTable`] holds bounded, one-use challenges.
//! * [`ReauthBindings`] holds key → grant bindings, the memory-only bearer
//!   slot for key-bound grants and bounded public tombstones. Nothing here
//!   touches the disk; a daemon restart forgets everything.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use ring::{agreement, rand::SystemRandom, signature};
use sha2::{Digest, Sha256};

use crate::data::browser_auth::{
    BrowserReauthBinding, BrowserReauthKeyView, BrowserSecret, GrantRevocationReason,
    REAUTH_CHALLENGES_PER_CREDENTIAL, REAUTH_CHALLENGES_PER_LISTENER, REAUTH_CHALLENGE_TTL,
    REAUTH_COMPLETE_PATH, REAUTH_TOMBSTONE_RETENTION, REAUTH_TRANSCRIPT_DOMAIN, REAUTH_VERSION,
};
use crate::data::ids::{BrowserGrantId, BrowserLinkId};
use crate::engine::auth::device_flow::generate_secret;
use crate::engine::error::EngineError;

/// Random identifier sizes, in bytes.
pub const CREDENTIAL_ID_BYTES: usize = 32;
pub const NONCE_BYTES: usize = 32;
pub const EPOCH_BYTES: usize = 16;
pub const PUBLIC_KEY_BYTES: usize = 65;
pub const SIGNATURE_BYTES: usize = 64;

/// The P-256 generator in uncompressed form. Used only as the fixed dummy key
/// so an unknown credential costs one verification like a wrong key; its
/// result is always discarded.
const DUMMY_PUBLIC_KEY: [u8; PUBLIC_KEY_BYTES] = [
    0x04, 0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40,
    0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2,
    0x96, 0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e,
    0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51,
    0xf5,
];

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode canonical unpadded base64url of exactly `len` bytes. Padding,
/// standard-alphabet characters, whitespace, non-canonical trailing bits and
/// wrong lengths are all refused.
pub fn decode_exact(text: &str, len: usize) -> Option<Vec<u8>> {
    if text.len() != len.div_ceil(3) * 4 - (3 - len % 3) % 3 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()?;
    (bytes.len() == len && b64(&bytes) == text).then_some(bytes)
}

/// Is `text` a canonical encoding of a `len`-byte random identifier?
pub fn is_id(text: &str, len: usize) -> bool {
    decode_exact(text, len).is_some()
}

/// A fresh daemon or listener epoch: 16 CSPRNG bytes, base64url (22 chars).
/// Never persisted.
pub fn new_epoch() -> Result<String, EngineError> {
    generate_secret(EPOCH_BYTES)
}

/// An enrolled public key: a validated, uncompressed P-256 point.
#[derive(Clone, PartialEq, Eq)]
pub struct ReauthPublicKey([u8; PUBLIC_KEY_BYTES]);

impl std::fmt::Debug for ReauthPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReauthPublicKey")
            .field(&self.sha256())
            .finish()
    }
}

impl ReauthPublicKey {
    /// Parse the enrollment `public_key` field. `None` for any encoding,
    /// length, prefix or curve failure.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = decode_exact(text, PUBLIC_KEY_BYTES)?;
        if bytes[0] != 0x04 {
            return None;
        }
        // ring validates a peer public key fully (on curve, not the point at
        // infinity, coordinates in range) before ECDH. A one-off ephemeral
        // agreement is that check; its output is discarded.
        let rng = SystemRandom::new();
        let ephemeral =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).ok()?;
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &bytes);
        agreement::agree_ephemeral(ephemeral, &peer, |_| ()).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }

    /// The fixed dummy key used when a credential has no binding.
    pub fn dummy() -> Self {
        Self(DUMMY_PUBLIC_KEY)
    }

    /// Unpadded base64url SHA-256 of the 65 raw bytes (owner lookup).
    pub fn sha256(&self) -> String {
        b64(&Sha256::digest(self.0))
    }

    /// Verify a raw `r || s` signature over `message` (hashed once with
    /// SHA-256 by ring).
    pub fn verify(&self, message: &[u8], signature: &ReauthSignature) -> bool {
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &self.0)
            .verify(message, &signature.0)
            .is_ok()
    }
}

/// A raw 64-byte `r || s` signature. Proof material: never printed.
#[derive(Clone)]
pub struct ReauthSignature([u8; SIGNATURE_BYTES]);

impl std::fmt::Debug for ReauthSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReauthSignature(<redacted>)")
    }
}

impl ReauthSignature {
    /// 86 characters of canonical unpadded base64url decoding to 64 bytes.
    pub fn parse(text: &str) -> Option<Self> {
        Some(Self(decode_exact(text, SIGNATURE_BYTES)?.try_into().ok()?))
    }
}

/// The fields a challenge binds, as the exact strings of its response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeFields {
    pub credential_id: String,
    pub daemon_epoch: String,
    pub listener_epoch: String,
    pub origin: String,
    pub listener: String,
    pub challenge_id: String,
    pub nonce: String,
    pub client_nonce: String,
    pub expires_at: String,
}

/// The signed message: `awman.browser.reauth.v1 || 0x00` then eleven
/// `u32be(len) || utf8` strings in C3's order. Not JSON; no normalization.
pub fn transcript(fields: &ChallengeFields) -> Vec<u8> {
    let parts: [&str; 11] = [
        "POST",
        REAUTH_COMPLETE_PATH,
        &fields.credential_id,
        &fields.daemon_epoch,
        &fields.listener_epoch,
        &fields.origin,
        &fields.listener,
        &fields.challenge_id,
        &fields.nonce,
        &fields.client_nonce,
        &fields.expires_at,
    ];
    let mut out = Vec::with_capacity(
        REAUTH_TRANSCRIPT_DOMAIN.len() + 1 + parts.iter().map(|p| 4 + p.len()).sum::<usize>(),
    );
    out.extend_from_slice(REAUTH_TRANSCRIPT_DOMAIN);
    out.push(0);
    for part in parts {
        let len = u32::try_from(part.len()).expect("transcript strings are short");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

/// The transcript's timestamp format: RFC 3339 UTC, three fractional digits,
/// `Z`.
pub fn format_expiry(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// One outstanding challenge.
#[derive(Debug, Clone)]
pub struct Challenge {
    pub fields: ChallengeFields,
    expires_at: DateTime<Utc>,
    deadline: Instant,
    issued: Instant,
}

impl Challenge {
    /// Live until the earlier of its wall-clock and monotonic deadlines.
    pub fn live(&self, now: DateTime<Utc>, instant: Instant) -> bool {
        now < self.expires_at && instant < self.deadline
    }
}

/// Why a challenge could not be issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChallengeIssueError {
    #[error("too many outstanding challenges on this listener")]
    ListenerFull,
    #[error("{0}")]
    Engine(String),
}

/// What a caller asks a challenge to bind (all from the gate or the
/// validated request).
#[derive(Debug, Clone)]
pub struct ChallengeRequest<'a> {
    pub credential_id: &'a str,
    pub daemon_epoch: &'a str,
    pub listener_epoch: &'a str,
    pub origin: &'a str,
    pub listener: &'a str,
    pub client_nonce: &'a str,
}

/// Bounded one-use challenges, keyed by challenge ID.
#[derive(Debug, Default)]
pub struct ChallengeTable {
    challenges: Mutex<HashMap<String, Challenge>>,
}

impl ChallengeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a challenge. At [`REAUTH_CHALLENGES_PER_CREDENTIAL`] for the
    /// presented credential ID (known or not, counted identically) its
    /// oldest challenge is dropped; at [`REAUTH_CHALLENGES_PER_LISTENER`]
    /// the request is refused.
    pub fn issue(
        &self,
        request: ChallengeRequest<'_>,
        now: DateTime<Utc>,
        instant: Instant,
    ) -> Result<ChallengeFields, ChallengeIssueError> {
        let engine = |e: EngineError| ChallengeIssueError::Engine(e.to_string());
        let mut table = self.lock();
        table.retain(|_, c| c.live(now, instant));
        let mut mine: Vec<(String, Instant)> = table
            .iter()
            .filter(|(_, c)| {
                c.fields.credential_id == request.credential_id
                    && c.fields.listener == request.listener
            })
            .map(|(id, c)| (id.clone(), c.issued))
            .collect();
        if mine.len() >= REAUTH_CHALLENGES_PER_CREDENTIAL {
            mine.sort_by_key(|(_, issued)| *issued);
            for (id, _) in mine
                .iter()
                .take(mine.len() + 1 - REAUTH_CHALLENGES_PER_CREDENTIAL)
            {
                table.remove(id);
            }
        }
        let on_listener = table
            .values()
            .filter(|c| c.fields.listener == request.listener)
            .count();
        if on_listener >= REAUTH_CHALLENGES_PER_LISTENER {
            return Err(ChallengeIssueError::ListenerFull);
        }
        let ttl = chrono::Duration::from_std(REAUTH_CHALLENGE_TTL).expect("30s fits");
        // Truncate to the transcript's millisecond precision so the stored
        // instant and the signed string agree.
        let expires_at = DateTime::from_timestamp_millis((now + ttl).timestamp_millis())
            .expect("valid timestamp");
        let fields = ChallengeFields {
            credential_id: request.credential_id.to_owned(),
            daemon_epoch: request.daemon_epoch.to_owned(),
            listener_epoch: request.listener_epoch.to_owned(),
            origin: request.origin.to_owned(),
            listener: request.listener.to_owned(),
            challenge_id: generate_secret(NONCE_BYTES).map_err(engine)?,
            nonce: generate_secret(NONCE_BYTES).map_err(engine)?,
            client_nonce: request.client_nonce.to_owned(),
            expires_at: format_expiry(expires_at),
        };
        table.insert(
            fields.challenge_id.clone(),
            Challenge {
                fields: fields.clone(),
                expires_at,
                deadline: instant + REAUTH_CHALLENGE_TTL,
                issued: instant,
            },
        );
        Ok(fields)
    }

    /// Remove `challenge_id` and return it if it was still live. Gone after
    /// this call whatever the result: a challenge is consumed exactly once.
    pub fn take(
        &self,
        challenge_id: &str,
        now: DateTime<Utc>,
        instant: Instant,
    ) -> Option<Challenge> {
        self.lock()
            .remove(challenge_id)
            .filter(|c| c.live(now, instant))
    }

    /// Drop every challenge for `listener` (listener generation changed).
    pub fn purge_listener(&self, listener: &str) {
        self.lock().retain(|_, c| c.fields.listener != listener);
    }

    /// Drop every challenge for `credential_id` (its grant was revoked).
    pub fn purge_credential(&self, credential_id: &str) {
        self.lock()
            .retain(|_, c| c.fields.credential_id != credential_id);
    }

    /// Drop every challenge (daemon stop).
    pub fn purge_all(&self) {
        self.lock().clear();
    }

    /// Outstanding challenges (tests and diagnostics).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.challenges.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Where a binding is in its life.
#[derive(Debug, Clone)]
pub enum BindingState {
    /// Enrolled with a link the owner has not approved.
    Pending { link: BrowserLinkId },
    /// Approved: the grant and its originally minted bearer, retained in
    /// memory only so a proof can return it again.
    Approved {
        grant: BrowserGrantId,
        bearer: BrowserSecret,
        expires_at: DateTime<Utc>,
        /// `Some(link)` until the first poll or proof collects it.
        uncollected: Option<BrowserLinkId>,
    },
    /// Ended: public material and the first termination reason only.
    Ended {
        reason: GrantRevocationReason,
        expires_at: DateTime<Utc>,
    },
}

/// One enrolled key and what it is bound to.
#[derive(Debug, Clone)]
pub struct Binding {
    pub credential_id: String,
    pub key: ReauthPublicKey,
    pub origin: String,
    pub listener: String,
    pub listener_epoch: String,
    pub link_expires_at: DateTime<Utc>,
    pub state: BindingState,
}

impl Binding {
    pub fn key_view(&self) -> BrowserReauthKeyView {
        BrowserReauthKeyView {
            version: REAUTH_VERSION,
            credential_id: self.credential_id.clone(),
            key_sha256: self.key.sha256(),
        }
    }

    pub fn public_binding(&self, daemon_epoch: &str) -> BrowserReauthBinding {
        BrowserReauthBinding {
            version: REAUTH_VERSION,
            credential_id: self.credential_id.clone(),
            daemon_epoch: daemon_epoch.to_owned(),
            listener_epoch: self.listener_epoch.clone(),
        }
    }

    pub fn grant(&self) -> Option<&BrowserGrantId> {
        match &self.state {
            BindingState::Approved { grant, .. } => Some(grant),
            _ => None,
        }
    }

    /// Should this binding still occupy a slot at `now`?
    fn retained(&self, now: DateTime<Utc>) -> bool {
        let retention = chrono::Duration::from_std(REAUTH_TOMBSTONE_RETENTION).expect("24h fits");
        match &self.state {
            BindingState::Pending { .. } => now < self.link_expires_at,
            BindingState::Approved { expires_at, .. } | BindingState::Ended { expires_at, .. } => {
                now < *expires_at + retention
            }
        }
    }
}

/// Key bindings by credential ID. Not internally locked: the policy layer
/// holds it under the lock that serializes grant release with revocation.
#[derive(Debug, Default)]
pub struct ReauthBindings {
    bindings: HashMap<String, Binding>,
}

impl ReauthBindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop pending enrollments past their link deadline and tombstones past
    /// their retention.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.bindings.retain(|_, b| b.retained(now));
    }

    pub fn contains(&self, credential_id: &str) -> bool {
        self.bindings.contains_key(credential_id)
    }

    pub fn count_for_listener(&self, listener: &str) -> usize {
        self.bindings
            .values()
            .filter(|b| b.listener == listener)
            .count()
    }

    pub fn insert(&mut self, binding: Binding) {
        self.bindings.insert(binding.credential_id.clone(), binding);
    }

    pub fn get(&self, credential_id: &str) -> Option<&Binding> {
        self.bindings.get(credential_id)
    }

    pub fn get_mut(&mut self, credential_id: &str) -> Option<&mut Binding> {
        self.bindings.get_mut(credential_id)
    }

    pub fn remove(&mut self, credential_id: &str) -> Option<Binding> {
        self.bindings.remove(credential_id)
    }

    pub fn by_link(&self, link: &BrowserLinkId) -> Option<&Binding> {
        self.bindings.values().find(|b| match &b.state {
            BindingState::Pending { link: l } => l == link,
            BindingState::Approved { uncollected, .. } => uncollected.as_ref() == Some(link),
            BindingState::Ended { .. } => false,
        })
    }

    pub fn by_grant(&self, grant: &BrowserGrantId) -> Option<&Binding> {
        self.bindings.values().find(|b| b.grant() == Some(grant))
    }

    /// Remove the pending enrollment of `link` (denied).
    pub fn remove_pending_link(&mut self, link: &BrowserLinkId) {
        self.bindings
            .retain(|_, b| !matches!(&b.state, BindingState::Pending { link: l } if l == link));
    }

    /// Attach an approved grant to the enrollment of `link`.
    pub fn approve(
        &mut self,
        link: &BrowserLinkId,
        grant: BrowserGrantId,
        bearer: BrowserSecret,
        expires_at: DateTime<Utc>,
    ) -> bool {
        let Some(binding) = self
            .bindings
            .values_mut()
            .find(|b| matches!(&b.state, BindingState::Pending { link: l } if l == link))
        else {
            return false;
        };
        binding.state = BindingState::Approved {
            grant,
            bearer,
            expires_at,
            uncollected: Some(link.clone()),
        };
        true
    }

    /// The grant was collected (by poll or proof).
    pub fn mark_collected(&mut self, grant: &BrowserGrantId) {
        for b in self.bindings.values_mut() {
            if let BindingState::Approved {
                grant: g,
                uncollected,
                ..
            } = &mut b.state
            {
                if g == grant {
                    *uncollected = None;
                }
            }
        }
    }

    /// The grant ended. Owner deletion, origin removal and expiry keep a
    /// public tombstone (first reason wins); listener-wide endings and
    /// daemon stop keep nothing, because the changed epoch answers first.
    /// The retained bearer is dropped either way. Returns the credential IDs
    /// affected.
    pub fn end_grant(
        &mut self,
        grant: &BrowserGrantId,
        reason: GrantRevocationReason,
    ) -> Vec<String> {
        let ids: Vec<String> = self
            .bindings
            .values()
            .filter(|b| b.grant() == Some(grant))
            .map(|b| b.credential_id.clone())
            .collect();
        for id in &ids {
            let Some(binding) = self.bindings.get_mut(id) else {
                continue;
            };
            let BindingState::Approved { expires_at, .. } = binding.state else {
                continue;
            };
            match reason {
                GrantRevocationReason::Deleted
                | GrantRevocationReason::OriginRemoved
                | GrantRevocationReason::Expired => {
                    binding.state = BindingState::Ended { reason, expires_at };
                }
                GrantRevocationReason::ListenerClosed | GrantRevocationReason::DaemonStopping => {
                    self.bindings.remove(id);
                }
            }
        }
        ids
    }

    /// Forget a binding whose uncollected approval timed out: that is a
    /// generic proof failure, not a twelve-hour expiry.
    pub fn forget_grant(&mut self, grant: &BrowserGrantId) {
        self.bindings.retain(|_, b| b.grant() != Some(grant));
    }

    /// Remove pending enrollments whose (origin, listener) match `pred`.
    /// Approved bindings end through [`Self::end_grant`] instead.
    pub fn remove_unapproved_where(&mut self, pred: impl Fn(&str, &str) -> bool) {
        self.bindings.retain(|_, b| {
            matches!(
                b.state,
                BindingState::Approved { .. } | BindingState::Ended { .. }
            ) || !pred(&b.origin, &b.listener)
        });
    }

    /// Remove every binding on `listener` (its generation changed).
    pub fn remove_listener(&mut self, listener: &str) {
        self.bindings.retain(|_, b| b.listener != listener);
    }

    pub fn clear(&mut self) {
        self.bindings.clear();
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    fn keypair() -> EcdsaKeyPair {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    fn fields() -> ChallengeFields {
        ChallengeFields {
            credential_id: "c".repeat(43),
            daemon_epoch: "d".repeat(22),
            listener_epoch: "l".repeat(22),
            origin: "http://127.0.0.1:3000".into(),
            listener: "lôopback".into(),
            challenge_id: "i".repeat(43),
            nonce: "n".repeat(43),
            client_nonce: "k".repeat(43),
            expires_at: "2026-11-12T12:00:30.000Z".into(),
        }
    }

    /// Subtract a 32-byte big-endian `s` from the P-256 group order: the
    /// alternate (high/low-S) signature for the same `r`.
    fn negate_s(sig: &[u8]) -> Vec<u8> {
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let mut out = sig[..32].to_vec();
        let mut s = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = N[i] as i16 - sig[32 + i] as i16 - borrow;
            borrow = i16::from(d < 0);
            s[i] = (d + if d < 0 { 256 } else { 0 }) as u8;
        }
        out.extend_from_slice(&s);
        out
    }

    #[test]
    fn transcript_is_length_prefixed_utf8_in_the_contract_order() {
        let t = transcript(&fields());
        assert!(t.starts_with(b"awman.browser.reauth.v1\0\x00\x00\x00\x04POST"));
        let route = b"\x00\x00\x00\x1b/v1/browser-reauth/complete";
        assert_eq!(&t[24 + 8..24 + 8 + route.len()], route);
        // Non-ASCII listener: its UTF-8 byte length (9), not its char count.
        let needle = [&9u32.to_be_bytes()[..], "lôopback".as_bytes()].concat();
        assert!(t.windows(needle.len()).any(|w| w == needle));
        assert!(t.ends_with(b"\x00\x00\x00\x182026-11-12T12:00:30.000Z"));
        let total: usize = 24
            + [4, 27, 43, 22, 22, 21, 9, 43, 43, 43, 24]
                .iter()
                .map(|n| 4 + n)
                .sum::<usize>();
        assert_eq!(t.len(), total);
    }

    #[test]
    fn raw_p256_signatures_verify_once_hashed_and_both_s_forms_are_accepted() {
        let pair = keypair();
        let key = ReauthPublicKey::parse(&b64(pair.public_key().as_ref())).unwrap();
        let message = transcript(&fields());
        let rng = SystemRandom::new();
        let sig = pair.sign(&rng, &message).unwrap();
        assert_eq!(sig.as_ref().len(), 64, "raw r||s, not DER");
        let parsed = ReauthSignature::parse(&b64(sig.as_ref())).unwrap();
        assert!(key.verify(&message, &parsed));
        let alternate = ReauthSignature::parse(&b64(&negate_s(sig.as_ref()))).unwrap();
        assert!(
            key.verify(&message, &alternate),
            "high-S and low-S both verify"
        );
        // Double hashing, a tampered byte and a different key all fail.
        let digest = Sha256::digest(&message);
        assert!(!key.verify(&digest, &parsed));
        let mut tampered = message.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(!key.verify(&tampered, &parsed));
        let other = ReauthPublicKey::parse(&b64(keypair().public_key().as_ref())).unwrap();
        assert!(!other.verify(&message, &parsed));
        assert!(!ReauthPublicKey::dummy().verify(&message, &parsed));
        assert_eq!(format!("{parsed:?}"), "ReauthSignature(<redacted>)");
    }

    #[test]
    fn public_keys_must_be_canonical_uncompressed_points_on_p256() {
        let pair = keypair();
        let raw = pair.public_key().as_ref().to_vec();
        assert!(ReauthPublicKey::parse(&b64(&raw)).is_some());
        // Padding, standard alphabet, whitespace, compressed, off-curve,
        // wrong prefix, truncated and private-key-sized material.
        let padded = format!("{}=", b64(&raw));
        assert!(ReauthPublicKey::parse(&padded).is_none());
        let standard = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&raw);
        if standard != b64(&raw) {
            assert!(ReauthPublicKey::parse(&standard).is_none());
        }
        assert!(ReauthPublicKey::parse(&format!(" {}", b64(&raw))).is_none());
        let mut compressed = vec![0x02 | (raw[64] & 1)];
        compressed.extend_from_slice(&raw[1..33]);
        assert!(ReauthPublicKey::parse(&b64(&compressed)).is_none());
        let mut off_curve = raw.clone();
        off_curve[64] ^= 1;
        assert!(ReauthPublicKey::parse(&b64(&off_curve)).is_none());
        let mut prefix = raw.clone();
        prefix[0] = 0x05;
        assert!(ReauthPublicKey::parse(&b64(&prefix)).is_none());
        assert!(ReauthPublicKey::parse(&b64(&raw[..64])).is_none());
        assert!(ReauthPublicKey::parse(&b64(&[0x42; 32])).is_none());
        assert!(ReauthPublicKey::parse(&b64(&[0u8; 65])).is_none());
        assert_eq!(ReauthPublicKey::dummy().sha256().len(), 43);
    }

    #[test]
    fn exact_decoding_refuses_wrong_lengths_and_noncanonical_text() {
        assert_eq!(decode_exact(&b64(&[7u8; 32]), 32), Some(vec![7u8; 32]));
        assert!(decode_exact(&b64(&[7u8; 31]), 32).is_none());
        assert!(decode_exact(&format!("{}=", b64(&[7u8; 32])), 32).is_none());
        // 43 chars where the last one carries non-zero unused bits.
        let mut text = b64(&[0u8; 32]);
        text.pop();
        text.push('B');
        assert!(decode_exact(&text, 32).is_none());
        assert_eq!(b64(&[0u8; 64]).len(), 86);
        assert!(is_id(&new_epoch().unwrap(), EPOCH_BYTES));
        assert_eq!(new_epoch().unwrap().len(), 22);
    }

    fn request<'a>(credential: &'a str, listener: &'a str) -> ChallengeRequest<'a> {
        ChallengeRequest {
            credential_id: credential,
            daemon_epoch: "d",
            listener_epoch: "l",
            origin: "http://127.0.0.1:3000",
            listener,
            client_nonce: "k",
        }
    }

    #[test]
    fn challenges_are_one_use_and_end_at_the_earlier_clock() {
        let table = ChallengeTable::new();
        let now = Utc::now();
        let instant = Instant::now();
        let c = table.issue(request("a", "l"), now, instant).unwrap();
        assert_eq!(c.expires_at.len(), 24);
        assert!(c.expires_at.ends_with('Z'));
        assert!(table.take(&c.challenge_id, now, instant).is_some());
        assert!(
            table.take(&c.challenge_id, now, instant).is_none(),
            "one use"
        );
        // Wall time advanced past expiry while monotonic time stood still
        // (suspend), and the reverse (wall clock rolled back).
        let late = chrono::Duration::seconds(31);
        let c = table.issue(request("a", "l"), now, instant).unwrap();
        assert!(table.take(&c.challenge_id, now + late, instant).is_none());
        let c = table.issue(request("a", "l"), now, instant).unwrap();
        assert!(table
            .take(&c.challenge_id, now - late, instant + REAUTH_CHALLENGE_TTL)
            .is_none());
    }

    #[test]
    fn a_ninth_challenge_drops_the_oldest_for_known_and_unknown_ids_alike() {
        let table = ChallengeTable::new();
        let now = Utc::now();
        let start = Instant::now();
        for credential in ["known", "unknown"] {
            let issued: Vec<_> = (0..9u64)
                .map(|i| {
                    table
                        .issue(
                            request(credential, "l"),
                            now,
                            start + std::time::Duration::from_millis(i),
                        )
                        .unwrap()
                })
                .collect();
            let at = start + std::time::Duration::from_millis(10);
            assert!(table.take(&issued[0].challenge_id, now, at).is_none());
            for c in &issued[1..] {
                assert!(table.take(&c.challenge_id, now, at).is_some());
            }
        }
    }

    #[test]
    fn a_full_listener_refuses_new_challenges_without_evicting_others() {
        let table = ChallengeTable::new();
        let now = Utc::now();
        let instant = Instant::now();
        for i in 0..REAUTH_CHALLENGES_PER_LISTENER {
            table
                .issue(request(&format!("c{i}"), "l"), now, instant)
                .unwrap();
        }
        assert_eq!(
            table.issue(request("x", "l"), now, instant),
            Err(ChallengeIssueError::ListenerFull)
        );
        assert!(table.issue(request("x", "other"), now, instant).is_ok());
        table.purge_listener("l");
        assert_eq!(table.len(), 1);
    }
}
/// A fixed raw-signature vector shared with the web client's tests
/// (`/awman/context/workflow/reauth-test-vector.json` in the WI 0160
/// workflow). The web lane must verify its WebCrypto transcript bytes match
/// `TRANSCRIPT_HEX` and add a live WebCrypto-signed round trip.
#[cfg(test)]
mod fixed_vector {
    use super::*;

    const PUBLIC_KEY: &str =
        "BE-_pawjfdRueGK4URlmiPBsnIdvWUOwI1GC5jaMB5ts7WAKNul0gXeRaeUm_5y30mfYPFFKj-CHgeaqkKwSAY4";
    const SIGNATURE: &str =
        "_N4tPjRQTMAIX4RBxnOGA6gg2T0-teT479XJGYwHRN4MRIfILPzAdo4I_PxmXp-pUu0TSb4oPyqQ5d_9_eyo0A";
    const TRANSCRIPT_HEX: &str = "61776d616e2e62726f777365722e7265617574682e76310000000004504f53540000001b2f76312f62726f777365722d7265617574682f636f6d706c6574650000002b5a7a7273504a7962557744767255786258346f513933734e4b354261707259675372566c303674717279550000001647733963317968572d5341365036484c5155734e6577000000164330744f5a58454275414c4258745a4173676d44436700000015687474703a2f2f3132372e302e302e313a33303030000000086c6f6f706261636b0000002b71416651617942747761627238756f6756414b445a6a4c48686862414e62654c634955486646635555376f0000002b576849645266764239796962655f56443846427058365275664b4352306b376c3576396c78704d626175450000002b6c5432324165456b465848626a6c3961795a344e556468563943745f3548577937717653314e684468763800000018323032362d31312d31325431323a30303a33302e3030305a";

    fn fields() -> ChallengeFields {
        ChallengeFields {
            credential_id: "ZzrsPJybUwDvrUxbX4oQ93sNK5BaprYgSrVl06tqryU".into(),
            daemon_epoch: "Gs9c1yhW-SA6P6HLQUsNew".into(),
            listener_epoch: "C0tOZXEBuALBXtZAsgmDCg".into(),
            origin: "http://127.0.0.1:3000".into(),
            listener: "loopback".into(),
            challenge_id: "qAfQayBtwabr8uogVAKDZjLHhhbANbeLcIUHfFcUU7o".into(),
            nonce: "WhIdRfvB9yibe_VD8FBpX6RufKCR0k7l5v9lxpMbauE".into(),
            client_nonce: "lT22AeEkFXHbjl9ayZ4NUdhV9Ct_5HWy7qvS1NhDhv8".into(),
            expires_at: "2026-11-12T12:00:30.000Z".into(),
        }
    }

    #[test]
    fn the_shared_raw_signature_vector_verifies_against_its_exact_transcript() {
        let message = transcript(&fields());
        let hex: String = message.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, TRANSCRIPT_HEX);
        let key = ReauthPublicKey::parse(PUBLIC_KEY).unwrap();
        let signature = ReauthSignature::parse(SIGNATURE).unwrap();
        assert!(key.verify(&message, &signature));
        let mut other = fields();
        other.listener = "loopback2".into();
        assert!(!key.verify(&transcript(&other), &signature));
    }
}
