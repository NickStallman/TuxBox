//! Translate a cooked shader, optionally linking its outputs to a pixel shader.
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use tuxbox_xshader::{extract, ShaderExtract, Stage};

// Used only for deterministic refusals by the HLSL emitter.
const UNSUPPORTED_HLSL: u8 = 65;
use tuxbox_xshader::hlsl::MAX_HLSL_BYTES;

/// SHA-256 (FIPS 180-4) for printing the variant digest the host records.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64) * 8;
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, d) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(d.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

enum Error {
    Other(String),
    Unsupported(String),
}

impl From<String> for Error {
    fn from(message: String) -> Self { Self::Other(message) }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self { Self::Other(message.into()) }
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(feature = "legacy-hlsl")]
fn translate(
    shader: &ShaderExtract,
    consumer: Option<&Path>,
    pixel_order: Option<&Path>,
    stencil_target: Option<u32>,
    wave32_host: bool,
) -> Result<(String, &'static str), Error> {
    use tuxbox_xshader::{hlsl, meta};

    let translated = if let Some(path) = consumer {
        let bytes = read(path)?;
        if !matches!(shader.stage, Stage::Vertex | Stage::Geometry | Stage::Mesh) {
            return Err("expected a vertex, geometry or mesh producer and a pixel consumer".into());
        }
        let actual = pixel_order.map(read).transpose()?;
        // A portable original consumer has no native ISA to translate. Read
        // its interpolation directly; retain native consumer handling otherwise.
        let portable = tuxbox_xshader::psv::portable_pixel_interface(&bytes);
        let is_portable = portable.is_some();
        let mut producer = shader.clone();
        let interp = if let Some(interface) = portable {
            if actual.as_deref().is_some_and(|code| code != bytes.as_slice()) {
                return Err(Error::Unsupported("portable pixel consumer differs from its bound bytecode".into()));
            }
            // Portable consumers need the same component-type repair as
            // native consumers. Only established 32-bit types are supported.
            // Mesh programs also feed per-primitive attributes to the pixel stage.
            for output in producer.output_sig.iter_mut().chain(producer.primitive_sig.iter_mut()).filter(|o| o.sysval == 0) {
                let mut matches = interface.inputs.iter().filter(|i| i.sysval == 0
                    && i.index == output.index && i.name.eq_ignore_ascii_case(&output.name));
                if let Some(input) = matches.next() {
                    // A packed half output has no integer form.
                    if matches.next().is_some() || !matches!(input.comp_type, 1..=3)
                        || output.comp_type == 6 && input.comp_type != 3 {
                        return Err(Error::Unsupported("ambiguous or unsupported portable pixel component type".into()));
                    }
                    output.comp_type = input.comp_type;
                }
            }
            hlsl::InputInterp(interface.interpolation)
        } else {
            let pixel = extract(&bytes)?;
            if pixel.stage != Stage::Pixel {
                return Err("expected a pixel consumer".into());
            }
            // Console interpolants are raw dwords, so a title may write an element
            // as float and read it as uint (or the reverse). Desktop drivers reject
            // the pair (vkd3d: "signature element ... not compatible"). Give each
            // producer output the consumer's component type; exports keep the bits.
            for output in producer.output_sig.iter_mut().chain(producer.primitive_sig.iter_mut()).filter(|o| o.sysval == 0) {
                let input = pixel.input_sig.iter().find(|i| {
                    i.sysval == 0 && i.index == output.index && i.name.eq_ignore_ascii_case(&output.name)
                });
                // 1 = uint, 2 = sint; 0, 3 (float) and 6 (float16) all emit as float.
                let integer = |t: u32| matches!(t, 1 | 2);
                match input.map(|i| i.comp_type) {
                    Some(t) if integer(t) => output.comp_type = t,
                    Some(_) if integer(output.comp_type) => output.comp_type = 3,
                    _ => {}
                }
            }
            hlsl::translate(&pixel).map_err(Error::Unsupported)?.ps_input_interp
        };
        let order = actual.as_deref().map(meta::pixel_input_order)
            .unwrap_or_else(|| if is_portable { meta::pixel_input_order(&bytes) } else { Vec::new() });
        hlsl::translate_ordered(&producer, Some(&interp), &order)
    } else if let Some(target) = stencil_target {
        hlsl::translate_stencil_target(shader, target)
    } else if wave32_host {
        hlsl::translate_for_wave32_host(shader)
    } else {
        hlsl::translate(shader)
    }.map_err(Error::Unsupported)?;
    let model = if translated.required_wave_size.is_some()
        || translated.typed_int64_atomics
        || matches!(translated.profile, "ps_6_6" | "cs_6_6" | "ms_6_6")
    {
        "6_6"
    } else if shader.stage == Stage::Mesh || translated.profile == "cs_6_5" {
        "6_5"
    } else if translated.packed_dot_product {
        "6_4"
    } else if translated.needs_barycentrics {
        "6_1"
    } else {
        "6_0"
    };
    Ok((translated.hlsl, model))
}

#[cfg(not(feature = "legacy-hlsl"))]
fn translate(
    shader: &ShaderExtract,
    consumer: Option<&Path>,
    _pixel_order: Option<&Path>,
    stencil_target: Option<u32>,
    _wave32_host: bool,
) -> Result<(String, &'static str), Error> {
    if consumer.is_some() {
        return Err(Error::Unsupported("independent HLSL emitter does not support linked shaders".into()));
    }
    if stencil_target.is_some() {
        return Err(Error::Unsupported("independent HLSL emitter does not support stencil exports".into()));
    }
    let hlsl = tuxbox_xshader::independent_hlsl(shader).ok_or_else(|| {
        format!(
            "unsupported {:?} program for the independent HLSL emitter",
            shader.stage
        )
    }).map_err(Error::Unsupported)?;
    Ok((hlsl, if tuxbox_xshader::requires_inline_ray_query(shader) { "6_5" } else { "6_0" }))
}

fn run() -> Result<(), Error> {
    let mut args = env::args_os().skip(1);
    let source = args.next().ok_or("expected a cooked shader path")?;
    if source == "--list-emitters" {
        if args.next().is_some() {
            return Err("--list-emitters takes no arguments".into());
        }
        println!("independent");
        #[cfg(feature = "legacy-hlsl")]
        println!("legacy");
        return Ok(());
    }
    let mut output = None;
    let mut profile_output = None;
    let mut consumer = None;
    let mut pixel_order = None;
    let mut stencil_target = None;
    let mut wave32_host = false;
    let mut stride_table = false;
    while let Some(argument) = args.next() {
        if argument == "--stride-table" {
            if std::mem::replace(&mut stride_table, true) {
                return Err("duplicate --stride-table".into());
            }
            continue;
        }
        if argument == "--wave32-host" {
            if std::mem::replace(&mut wave32_host, true) {
                return Err("duplicate --wave32-host".into());
            }
            continue;
        }
        if argument == "--stencil-target" {
            let index: u32 = args
                .next()
                .and_then(|v| v.to_str()?.parse().ok())
                .ok_or("--stencil-target requires a render target index")?;
            if stencil_target.replace(index).is_some() {
                return Err("duplicate --stencil-target".into());
            }
            continue;
        }
        let slot = match argument.to_str() {
            Some("--hlsl-out") => &mut output,
            Some("--profile-out") => &mut profile_output,
            Some("--link-pixel") => &mut consumer,
            Some("--pixel-order") => &mut pixel_order,
            _ => return Err(format!("unknown option {argument:?}").into()),
        };
        if slot
            .replace(PathBuf::from(args.next().ok_or("missing path")?))
            .is_some()
        {
            return Err(format!("duplicate option {argument:?}").into());
        }
    }
    let output = output.ok_or("--hlsl-out is required")?;
    if consumer.is_some() && stencil_target.is_some() {
        return Err("a stencil export requires an unlinked pixel shader".into());
    }
    if wave32_host && (consumer.is_some() || stencil_target.is_some()) {
        return Err("--wave32-host applies to an unlinked compute shader".into());
    }
    if pixel_order.is_some() && consumer.is_none() {
        return Err("--pixel-order requires --link-pixel".into());
    }
    let source_bytes = read(source.as_ref())?;
    let (source_bytes, stride_digest) = if stride_table {
        if consumer.is_some() || stencil_target.is_some() {
            return Err("--stride-table applies to an unlinked standalone shader".into());
        }
        let candidate = tuxbox_dxbc::rootsig::stride_table_variant(&source_bytes)
            .ok_or("could not build the stride-table variant of this shader")?;
        let digest = sha256_hex(&candidate);
        (candidate, Some(digest))
    } else {
        (source_bytes, None)
    };
    if source_bytes.starts_with(&2_u32.to_le_bytes()) {
        if consumer.is_some() || stencil_target.is_some() {
            return Err("raster linking options cannot be applied to a ray library".into());
        }
        let (name, library) = tuxbox_xshader::ray::library(&source_bytes)?;
        fs::write(&output, library).map_err(|e| format!("{}: {e}", output.display()))?;
        if let Some(path) = profile_output {
            fs::write(&path, "lib_6_3").map_err(|e| format!("{}: {e}", path.display()))?;
        }
        println!("entry={name}");
        return Ok(());
    }
    let mut shader = extract(&source_bytes)?;
    // A root ending with the runtime's group base is a split dispatch's variant.
    let group_base = tuxbox_xshader::group_base::take(&mut shader);
    let (hlsl, model) = translate(
        &shader,
        consumer.as_deref(),
        pixel_order.as_deref(),
        stencil_target,
        wave32_host,
    )?;
    let hlsl = if group_base {
        tuxbox_xshader::group_base::offset(&hlsl).map_err(Error::Unsupported)?
    } else {
        hlsl
    };
    // DXC needs about 1.1 GB of memory per MB of generated HLSL at -O0: two
    // 10 MB programs took 11 GB each and starved the running title. Refuse such
    // a translation rather than hand it to the compiler.
    if hlsl.len() > MAX_HLSL_BYTES {
        return Err(Error::Unsupported(format!(
            "translated HLSL of {} MiB exceeds the {} MiB compiler budget",
            hlsl.len() >> 20,
            MAX_HLSL_BYTES >> 20
        )));
    }
    if let Some(path) = profile_output {
        let stage = match shader.stage {
            Stage::Pixel => "ps",
            Stage::Vertex => "vs",
            Stage::Geometry => "gs",
            Stage::Compute => "cs",
            Stage::Mesh => "ms",
            _ => return Err("unsupported DXC stage".into()),
        };
        fs::write(&path, format!("{stage}_{model}"))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let mut instructions = 0;
    let mut consumed = 0;
    let mut ended = false;
    for ins in tuxbox_xshader::rdna2::instructions(&shader.isa) {
        instructions += 1;
        consumed += ins.dwords * 4;
        ended |= ins.mnemonic == "s_endpgm";
    }
    println!("entry={:?}", shader.entry);
    println!(
        "decode: {instructions} instrs, consumed {consumed}/{} bytes, terminated_on_s_endpgm={ended}",
        shader.isa.len()
    );
    if let Some(digest) = stride_digest {
        println!("stride-digest={digest}");
    }
    fs::write(&output, hlsl).map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(Error::Other(error)) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
        Err(Error::Unsupported(error)) => {
            eprintln!("{error}");
            std::process::ExitCode::from(UNSUPPORTED_HLSL)
        }
    }
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
