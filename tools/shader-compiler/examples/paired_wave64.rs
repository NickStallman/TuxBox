//! Translate a synthetic wave64 compute program, supplied as native words, into
//! the two-lanes-per-thread form. Arguments: thread group size, then the words.
use tuxbox_xshader::{ShaderExtract, Stage, hlsl};

mod fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/support/wave_guard_fixture.rs"));
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    // `heap` first: the program indexes the resource heap, whose base is in s10, and the sampler heap in s14.
    let heap = args.next_if(|arg| arg == "heap").is_some();
    // The group is a thread count, or `XxY` for a group with rows.
    let group = args.next().expect("thread count");
    let (x, y) = group.split_once('x').unwrap_or((group.as_str(), "1"));
    let (x, y): (u32, u32) = (x.parse().expect("integer"), y.parse().expect("integer"));
    let threads = x * y;
    let words: Vec<u32> = args.map(|s| u32::from_str_radix(&s, 16).expect("hex native word")).collect();
    assert!((threads % 64 == 0 || threads == 32) && !words.is_empty() && words.last() == Some(&0xbf810000));
    let (mut shader, _, _): (ShaderExtract, _, _) = fixture::shader(64);
    shader.isa = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    shader.isa_data = shader.isa.clone();
    shader.isa_preamble = [0, x, y, 1];
    shader.psv_threads = Some([x, y, 1]);
    if heap {
        // One user-data record of class 5 (the resource heap) in s10, and a root that indexes the heap directly.
        let mut fssm = vec![0u8; 0x100];
        fssm[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
        fssm[0xe8..0xf0].copy_from_slice(&0xf0u64.to_le_bytes());
        fssm[0xf8..0xfc].copy_from_slice(&2u32.to_le_bytes());
        // Class 6 is the sampler heap, in s14.
        fssm.extend([5u32, 0, 1, 0, 10, 6, 0, 1, 0, 14].into_iter().flat_map(u32::to_le_bytes));
        let size = (fssm.len() - 16) as u32;
        fssm[8..12].copy_from_slice(&size.to_le_bytes());
        shader.fssm = fssm;
        shader.shader_model = (6, 6);
        shader.required_features = Some(1 << 25);
        shader.has_root_sig = true;
        shader.root_sig = Some(tuxbox_dxbc::rootsig::RootSig {
            version: 1, flags: 0x400, params: Vec::new(), num_static_samplers: 0, static_samplers: Vec::new(),
        });
    }
    assert!(matches!(shader.stage, Stage::Compute));
    match hlsl::translate_paired_wave64(&shader) {
        Ok(result) => print!("{}", result.hlsl),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(65);
        }
    }
}
