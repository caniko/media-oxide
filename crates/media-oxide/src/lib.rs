//! Shared media format helpers.
//!
//! [`jxl`] covers JPEG XL: encoding through libjxl's `cjxl`, container
//! inspection, and, with the `decode` feature, decoding and the progressive
//! preview cuts.

pub mod jxl;
