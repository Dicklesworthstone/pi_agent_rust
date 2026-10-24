#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::unreadable_literal)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::upper_case_acronyms)]
#![allow(clippy::uninlined_format_args)]
#![no_std]

use ::core::ptr;

/// Common error message for converting between C `size_t` and Rust `usize`;
pub const SIZE_T_ERROR: &str =
    "conversion between C type 'size_t' and Rust type 'usize' overflowed.";

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(not(feature = "bindgen"))]
include!(concat!("bindings/", bindings_env!("TARGET"), ".rs"));

#[cfg(target_pointer_width = "64")]
include!("inlines/ptr_64.rs");

#[cfg(target_pointer_width = "32")]
include!("inlines/ptr_32_nan_boxing.rs");

include!("inlines/common.rs");
