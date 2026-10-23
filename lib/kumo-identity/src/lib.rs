#![no_std]
//j561

//! The interim identity layer's policy (PLAN/012): `/etc/passwd`, `/etc/group` and
//! `/etc/shadow` parsing, and argon2id verification for the `identityd` service.
//!
//! J561 only brings the vendored `argon2` crate into the build. The parsing and verification
//! land with the identity files and the service, in their own slices.
