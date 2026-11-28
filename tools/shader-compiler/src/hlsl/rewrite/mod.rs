//! Rewrites of the instruction list made before translation.
use super::*;

mod quad;
mod reductions;
mod lanes;
mod election;
mod key_counts;
pub(in crate::hlsl) use quad::*;
pub(in crate::hlsl) use reductions::*;
pub(in crate::hlsl) use lanes::*;
pub(in crate::hlsl) use election::*;
pub(in crate::hlsl) use key_counts::*;
