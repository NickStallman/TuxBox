//! Decoders for the Xbox DirectStorage hardware stages. Platform-independent,
//! so they are tested on any host.
//!
//! - [`rans`]: one lane of the MDU rANS decoder (Microsoft's silicon-rans);
//! - [`lanes`]: several lanes sharing one stream, as BCPack uses them;
//! - [`record`]: the BCPack formats and what their block records share;
//! - [`bc1`], [`bcn`] (BC3, BC4, BC5) and [`bc7`]: the block rebuilds.
//!
//! BC6H is not decoded: its block rebuild is not described anywhere TuxBox
//! can use. BC2 has no BCPack data in any title measured.
pub mod bc1;
pub mod bc7;
pub mod bcn;
pub mod lanes;
pub mod models;
pub mod rans;
pub mod record;
mod tables;

#[cfg(test)]
#[path = "tests/encode.rs"]
mod encode;
