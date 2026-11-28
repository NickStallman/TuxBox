//! The hardware decode stages of a request, in the documented order: zlib
//! (RFC 1950), then BCPack. Swizzle is not implemented.
use std::borrow::Cow;
use tuxbox_xcrdapi_codec::record::{self, Format};

/// `ZlibDecompress`, bit 0 of the request options.
pub(crate) const ZLIB: u64 = 1;
/// `BcpackMode`, bits 1-4: the BC format number, 0 for none.
pub(crate) const BCPACK: u64 = 0xf << 1;

/// True when this provider decodes the BCPack mode in `options`: none, BC1,
/// BC3, BC4, BC5 or BC7. BC2 and BC6H are refused.
pub(crate) fn supported(options: u64) -> bool {
    let mode = bcpack_mode(options);
    mode == 0 || Format::from_mode(mode).is_some()
}

fn bcpack_mode(options: u64) -> u64 {
    (options & BCPACK) >> 1
}

/// Runs the stages `options` selects over `input`. `capacity` is the
/// destination size; `intermediate` is the zlib output size when BCPack
/// follows zlib. An error names the stage and why it failed.
pub(crate) fn run(
    input: &[u8],
    options: u64,
    capacity: usize,
    intermediate: usize,
) -> Result<Cow<'_, [u8]>, String> {
    let mode = bcpack_mode(options);
    let packed = if options & ZLIB != 0 {
        let limit = if mode == 0 { capacity } else { intermediate };
        Cow::Owned(
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(input, limit).map_err(|e| {
                format!(
                    "zlib {:?} after {} of at most {limit} bytes",
                    e.status,
                    e.output.len()
                )
            })?,
        )
    } else {
        Cow::Borrowed(input)
    };
    if mode == 0 {
        return Ok(packed);
    }
    let format = Format::from_mode(mode).ok_or_else(|| format!("BCPack mode {mode}"))?;
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(capacity)
        .map_err(|_| "out of memory".to_owned())?;
    blocks.resize(capacity, 0);
    record::decode(format, &packed, &mut blocks)
        .map_err(|e| format!("{format:?} {e:?} from {} bytes", packed.len()))?;
    Ok(Cow::Owned(blocks))
}
