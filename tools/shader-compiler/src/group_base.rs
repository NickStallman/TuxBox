//! Translate a compute program for a dispatch the runtime splits.
//!
//! The runtime records a console dispatch with more than 65,535 groups in a
//! dimension as several host dispatches, and appends root constants that hold
//! the console group ID each one starts at (`tuxbox_dxbc::group_base`). A
//! translation against such a root adds them to every group ID the program
//! reads, so each group computes with its console ID.
use crate::{ShaderExtract, Stage};

/// Take the group base from a compute program's root, so the translation
/// sees only the title's parameters. Returns whether the root had it.
pub fn take(shader: &mut ShaderExtract) -> bool {
    shader.stage == Stage::Compute
        && shader.root_sig.as_mut().is_some_and(tuxbox_dxbc::group_base::take)
}

const BASE: &str = "tb_group_base";
const LAUNCH: &str = "tb_launch_";
const ENTRY: &str = "void main(";

/// Add the group base to the group IDs of the translated program `hlsl`.
///
/// Every `SV_GroupID` and `SV_DispatchThreadID` the program reads must be a
/// `uint3` parameter of its one `main`. Each is renamed, and `main` begins by
/// defining the original name as the console value. A dispatch thread ID is
/// the group ID times the group size plus the thread's place in the group, so
/// it moves by the base times the `[numthreads]` of `main`. Anything else is
/// refused, so a split dispatch never runs a program whose group IDs it
/// could not correct.
pub fn offset(hlsl: &str) -> Result<String, String> {
    if hlsl.contains(BASE) || hlsl.contains(LAUNCH) || hlsl.contains("TBGroupBase") {
        return Err("group base: the program already uses its names".into());
    }
    let mut entries = hlsl.match_indices(ENTRY);
    let (Some((entry, _)), None) = (entries.next(), entries.next()) else {
        return Err("group base: the program needs exactly one main".into());
    };
    let open = entry + ENTRY.len();
    let close = open + hlsl[open..].find(')').ok_or("group base: unterminated main parameters")?;
    let params = &hlsl[open..close];
    let body = close + 1 + hlsl[close + 1..].find('{').ok_or("group base: main has no body")?;
    if params.contains('(') || !hlsl[close + 1..body].trim().is_empty() {
        return Err("group base: main's parameters are not plain".into());
    }
    let mut renamed = Vec::new();
    let mut prologue = String::new();
    let mut corrected = 0;
    for param in params.split(',') {
        let Some((declaration, semantic)) = param.split_once(':') else {
            renamed.push(param.to_owned());
            continue;
        };
        let semantic = semantic.trim();
        let thread = semantic.eq_ignore_ascii_case("SV_DispatchThreadID");
        if !thread && !semantic.eq_ignore_ascii_case("SV_GroupID") {
            renamed.push(param.to_owned());
            continue;
        }
        let words: Vec<_> = declaration.split_whitespace().collect();
        let [kind @ "uint3", name] = words[..] else {
            return Err(format!("group base: {semantic} parameter `{}` is not a uint3", declaration.trim()));
        };
        let launch = format!("{LAUNCH}{name}");
        renamed.push(format!(" {kind} {launch} : {semantic}"));
        let scale = if thread {
            let [x, y, z] = group_size(&hlsl[..entry])?;
            format!(" * uint3({x}u, {y}u, {z}u)")
        } else {
            String::new()
        };
        prologue.push_str(&format!("\n    uint3 {name} = {launch} + {BASE}{scale};"));
        corrected += 1;
    }
    let uses = semantic_uses(hlsl, "SV_GroupID") + semantic_uses(hlsl, "SV_DispatchThreadID");
    if uses != corrected {
        return Err("group base: the program reads a group ID outside main's parameters".into());
    }
    let mut out = String::with_capacity(hlsl.len() + 256);
    out.push_str("cbuffer TBGroupBase : register(b0, space32766) { uint3 tb_group_base; };\n");
    out.push_str(&hlsl[..open]);
    out.push_str(renamed.join(",").trim_start());
    out.push_str(&hlsl[close..=body]);
    out.push_str(&prologue);
    out.push_str(&hlsl[body + 1..]);
    Ok(out)
}

/// The `[numthreads]` written last before `main`.
fn group_size(before_entry: &str) -> Result<[u32; 3], String> {
    const ATTRIBUTE: &str = "[numthreads(";
    let start = before_entry.rfind(ATTRIBUTE).ok_or("group base: main has no [numthreads]")? + ATTRIBUTE.len();
    let end = start + before_entry[start..].find(')').ok_or("group base: unterminated [numthreads]")?;
    let sizes: Vec<_> = before_entry[start..end].split(',').map(|n| n.trim().parse::<u32>()).collect();
    match sizes[..] {
        [Ok(x), Ok(y), Ok(z)] => Ok([x, y, z]),
        _ => Err("group base: [numthreads] is not three literals".into()),
    }
}

/// How often `semantic` appears as a whole word, in any letter case.
fn semantic_uses(hlsl: &str, semantic: &str) -> usize {
    let text = hlsl.to_ascii_lowercase();
    let word = semantic.to_ascii_lowercase();
    let ident = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    text.match_indices(&word)
        .filter(|&(at, _)| !ident(text[..at].chars().next_back()) && !ident(text[at + word.len()..].chars().next()))
        .count()
}

#[cfg(test)]
#[path = "tests/group_base.rs"]
mod tests;
