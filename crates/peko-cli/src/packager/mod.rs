//! Package authoring and management for the cli.
//!
//! Submodules:
//!
//! - [`manager`]: the firebase-backed [`manager::PackageManager`] that
//!   handles `add` / `update` / `remove` operations against the project's
//!   `Packages/` directory.
//! - [`builder`]: turns a [`peko_core::packages::HostPackage`] into its
//!   `.pkpkg` distribution binary (used by the `pkg` command when
//!   authoring new packages).
//! - [`ziputil`]: small wrappers around the `zip` crate's archive
//!   API, used by `builder` for the per-version zip payloads.

pub mod builder;
pub mod manager;
pub mod ziputil;
