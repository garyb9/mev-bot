//! Recorder sources.
//!
//! Each source translates a venue transport into [`crate::envelope::Envelope`]s
//! and forwards them to a [`crate::segment::SegmentWriter`].

pub mod cex;
pub mod deribit;
pub mod hl_rest;
