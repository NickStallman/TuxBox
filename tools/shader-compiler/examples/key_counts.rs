//! Translate the synthetic key-count program for a GPU check.
//! usage: key_counts <32|64> <threads> <before|inside> <all|masked> [paired]
//!
//! `paired` asks for the translation a host without 64-lane waves runs.
use tuxbox_xshader::meta::{RootParam, RootParamKind, RootRange, RootSig};
use tuxbox_xshader::psv::PsvResource;
use tuxbox_xshader::{hlsl, ShaderExtract, Stage};

mod fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/support/key_count_fixture.rs"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [width, threads, valid, lanes, rest @ ..] = &args[..] else { panic!("width, threads, mask and lanes") };
    let wide = match width.as_str() { "32" => false, "64" => true, _ => panic!("width 32 or 64") };
    let threads: u32 = threads.parse().expect("thread count");
    let valid = match valid.as_str() {
        "before" => fixture::Valid::Before,
        "inside" => fixture::Valid::Inside,
        _ => panic!("before or inside"),
    };
    let masked = match lanes.as_str() { "all" => false, "masked" => true, _ => panic!("all or masked") };
    let shader = fixture::shader(threads, &fixture::words(wide, valid, masked, &[]));
    let translated = match rest {
        [] => hlsl::translate(&shader),
        [form] if form == "paired" && wide => hlsl::translate_for_wave32_host(&shader),
        _ => panic!("only a 64-lane program has a paired form"),
    };
    match translated {
        Ok(result) => print!("{}", result.hlsl),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(65);
        }
    }
}
