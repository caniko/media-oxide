//! Shared media format helpers.
//!
//! [`jxl`] covers JPEG XL: encoding through libjxl's `cjxl`, container
//! inspection, and, with the `decode` feature, decoding and the progressive
//! preview cuts.
//!
//! [`foundry`] covers Foundry VTT assets and is available with the `foundry`
//! feature: conversion planning for tokens, maps, tiles, artwork, and audio,
//! plus the upload checks a server runs before storing them.

#[cfg(feature = "foundry")]
pub mod foundry;
pub mod jxl;
