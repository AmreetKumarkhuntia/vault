//! `vault-mock` — the embedded HTTP server that impersonates the target's
//! external dependencies.
//!
//! One listener serves N dependencies via path prefixes (`/payments/...`).
//! Stubs match in declaration order, first full match wins. EVERY request is
//! recorded unconditionally — the recording log is the single source of truth
//! for `verify.calls` and unmatched-call detection. Unmatched requests answer
//! with status 599: unmistakable in target logs.

pub mod matcher;
pub mod record;
pub mod server;
pub mod session;
pub mod verify;

pub use record::{RecordedExchange, RecordedRequest};
pub use server::{
    claim_mode, mounts_overlap, Candidate, Claim, ClaimMode, MockServer, Orphan, PrefixClaim,
};
pub use session::{ResponseRenderer, Session, SessionGuard, SessionReport};
pub use verify::verify_calls;

/// Status served when no stub matches and the dependency's policy is `fail`.
pub const UNMATCHED_STATUS: u16 = 599;
