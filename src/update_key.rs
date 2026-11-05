//! Public half of the release signing key, compiled into the app.
//!
//! The release process sets this to the key printed by `sign-release keygen`.
//! While it is `None`, `update::verify_with_compiled_key` refuses every update
//! with `UpdateError::NoKey`. Only the PUBLIC key belongs here.

pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = None;
