//! Writer for Apple's CAR (Compiled Asset Catalog) binary format.
//!
//! Used by the iOS bundler to embed app icons as a `.car` asset catalog
//! inside the bundle. The format is undocumented by Apple; this
//! implementation is reverse-engineered from `actool` output and supports
//! only the subset needed for app-icon embedding (CSI rendition entries
//! plus BOM tree metadata).
//!
//! Submodules:
//!
//! - [`carinfo`]: type definitions for the CAR format's data shapes
//!   ([`carinfo::CarBinary`], [`carinfo::CarHeader`], [`carinfo::CSIData`],
//!   [`carinfo::BomTree`], etc.).
//! - [`carwriter`]: the serializer that turns a [`carinfo::CarBinary`]
//!   into its on-disk byte representation.

pub mod carinfo;
pub mod carwriter;
