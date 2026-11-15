//! Short-name crate for the buildwithnexus CLI.
//!
//! `cargo install bwn --locked` installs the `bwn` binary. The
//! [buildwithnexus](https://crates.io/crates/buildwithnexus) crate installs
//! that same binary alongside `buildwithnexus`; install one of the two, not
//! both.
//!
//! Product documentation: <https://buildwithnexus.dev>

// This library exists so `cargo rustdoc --lib` has a target. docs.rs runs
// that and otherwise fails the crate with "no library targets found".
