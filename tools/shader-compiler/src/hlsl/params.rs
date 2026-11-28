//! Which signature elements each hardware parameter (interpolated attribute) carries.
use std::collections::{BTreeMap, BTreeSet};

use super::{Enc, Insn};
use crate::{meta::SigElement, ShaderExtract};

/// One four-channel parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Param {
    /// One signature register, one component per channel.
    Full(u32),
    /// Interpolated half-float registers, two components per channel, each register starting
    /// at its own channel.
    Half(Vec<(u32, usize)>),
}

impl Param {
    pub fn registers(&self) -> Vec<u32> {
        match self {
            Param::Full(register) => vec![*register],
            Param::Half(rows) => rows.iter().map(|r| r.0).collect(),
        }
    }

    /// Channel of a register's component, and whether it is the upper half of that channel.
    pub fn channel(&self, register: u32, component: usize) -> Option<(usize, Option<bool>)> {
        match self {
            Param::Full(r) => (*r == register).then_some((component, None)),
            Param::Half(rows) => rows.iter().find(|r| r.0 == register)
                .map(|r| (r.1 + component / 2, Some(component % 2 != 0))),
        }
    }

    /// The register and component in one half of a channel.
    pub fn component(&self, channel: usize, high: bool) -> Option<(u32, usize)> {
        let Param::Half(rows) = self else { return None };
        let (register, first) = rows.iter().rev().find(|r| r.1 <= channel)?;
        Some((*register, (channel - first) * 2 + usize::from(high)))
    }
}

/// Assign the registers `keep` selects to parameters in signature order. A half-float register
/// that is interpolated shares the latest half-float parameter while its channels fit.
fn assign(elements: &[SigElement], flat: &[u32], keep: impl Fn(&SigElement) -> bool) -> Vec<Param> {
    let mut params: Vec<Param> = Vec::new();
    let mut seen = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for e in elements.iter().filter(|e| keep(e)) {
        if seen.contains(&e.register) {
            continue;
        }
        seen.push(e.register);
        let row = || elements.iter().filter(|o| o.register == e.register && keep(o));
        if flat.contains(&e.register) || row().any(|o| o.comp_type != 6) {
            params.push(Param::Full(e.register));
            continue;
        }
        let mask = row().fold(0u8, |m, o| m | o.mask);
        let channels = (8 - mask.leading_zeros() as usize).div_ceil(2);
        match open {
            Some((at, used)) if used + channels <= 4 => {
                if let Param::Half(rows) = &mut params[at] {
                    rows.push((e.register, used));
                }
                open = (used + channels < 4).then_some((at, used + channels));
            }
            _ => {
                open = (channels < 4).then_some((params.len(), channels));
                params.push(Param::Half(vec![(e.register, 0)]));
            }
        }
    }
    params
}

/// Parameters a vertex or mesh program exports for `elements`. A clip distance takes one too.
pub(super) fn outputs(ex: &ShaderExtract, elements: &[SigElement]) -> Vec<Param> {
    assign(elements, &ex.flat_outputs, |e| matches!(e.sysval, 0 | 2))
}

/// Parameters a pixel program reads. Programs differ in whether an input they never read keeps
/// its place; the interpolant count in the program's hardware state and the components its
/// instructions read tell them apart.
pub(super) fn inputs(ex: &ShaderExtract, insns: &[Insn]) -> Vec<Param> {
    let layout = declared_inputs(ex, insns);
    let reads = reads(insns);
    let sig = &ex.input_sig;
    if !reads.iter().any(|&r| rejects(&layout, sig, r)) {
        return layout;
    }
    // No layout built from the declarations explains what the program reads.
    // Take the one its reads force, where they force one.
    let count = parameter_count(ex);
    let all = assign(sig, &ex.flat_inputs, |e| e.sysval == 0);
    count.and_then(|count| forced(&all, sig, &ex.flat_inputs, count, &interpolations(insns)))
        .filter(|forced| !reads.iter().any(|&r| rejects(forced, sig, r)))
        .unwrap_or(layout)
}

/// The number of parameters the program's hardware state says it reads.
fn parameter_count(ex: &ShaderExtract) -> Option<usize> {
    ex.fssm.get(0x238..).and_then(|b| b.first_chunk()).map(|b| u32::from_le_bytes(*b) as usize)
}

/// The layout the declared signature suggests.
fn declared_inputs(ex: &ShaderExtract, insns: &[Insn]) -> Vec<Param> {
    let sig = &ex.input_sig;
    // A register counts as read if any element in it is, including a system value.
    let read = |register: u32| sig.iter().any(|o| o.register == register && o.rw_mask != 0);
    let used = assign(sig, &ex.flat_inputs, |e| e.sysval == 0 && read(e.register));
    let all = assign(sig, &ex.flat_inputs, |e| e.sysval == 0);
    let count = parameter_count(ex);
    let paired: Vec<Param> = all.iter().filter(|p| p.registers().into_iter().any(read)).cloned().collect();
    let reads = reads(insns);
    let mut fits: Vec<_> = [used.clone(), paired, all].into_iter().filter(|p| Some(p.len()) == count).collect();
    // The first of equals.
    fits.sort_by_key(|p| reads.iter().filter(|&&r| !carries(p, sig, r)).count());
    if let Some(layout) = fits.into_iter().next() {
        return layout;
    }
    // Without PS09 a clip distance can still take an exported parameter, even
    // if the pixel stage never interpolates it. Only use the producer layout
    // when the hardware count and every accessed component confirm it.
    if ex.ps09_pixel_inputs.is_none() && sig.iter().any(|e| e.sysval == 2)
        && reads.iter().any(|&r| rejects(&used, sig, r))
    {
        let exported = assign(sig, &ex.flat_inputs, |e| matches!(e.sysval, 0 | 2));
        if Some(exported.len()) == count && reads.iter().all(|&r| declared(&exported, sig, r)) {
            // Dropping unused rows before packing can retain the same parameter
            // count while moving the halves. The observed components must decide.
            let compact = assign(sig, &ex.flat_inputs, |e| e.sysval == 2 || e.sysval == 0 && read(e.register));
            if compact.len() != exported.len() || !reads.iter().all(|&r| declared(&compact, sig, r))
                || reads.iter().all(|&r| component(&compact, r) == component(&exported, r))
            {
                return exported;
            }
        }
    }
    used
}

type Read = (usize, usize, Option<bool>);

fn component(params: &[Param], (attr, channel, half): Read) -> Option<(u32, usize)> {
    match (params.get(attr)?, half) {
        (Param::Full(reg), _) => Some((*reg, channel)),
        (packed @ Param::Half(_), Some(high)) => packed.component(channel, high),
        _ => None,
    }
}

/// Match actual emitter rejections. VINTRP supplies the default channel of
/// an existing full parameter, so that case must keep the existing layout.
fn rejects(params: &[Param], sig: &[SigElement], r: Read) -> bool {
    let Some((reg, ch)) = component(params, r) else { return true };
    r.2.is_some() && !sig.iter().any(|e| e.sysval == 0 && e.register == reg && e.mask >> ch & 1 != 0
        && !matches!(e.comp_type, 1 | 2))
}

fn declared(params: &[Param], sig: &[SigElement], r: Read) -> bool {
    let Some((reg, ch)) = component(params, r) else { return false };
    let half = r.2.is_some();
    matches!(params[r.0], Param::Half(_)) == half && sig.iter().any(|e|
        e.sysval == 0 && e.register == reg && e.mask & e.rw_mask & (1 << ch) != 0)
}

/// Parameter, channel and half (`None` for the whole channel) of each interpolation.
fn reads(insns: &[Insn]) -> BTreeSet<(usize, usize, Option<bool>)> {
    insns.iter().filter_map(|ins| match ins.enc {
        Enc::Vinterp => Some((ins.aux.imm as usize, ins.aux.imm2 as usize, None)),
        Enc::Vop3 if ins.mnemonic.starts_with("v_interp_p") && ins.mnemonic.ends_with("_f16") => {
            let (attr, channel, high) = super::Emitter::f16_interp_descriptor(*ins.raw.get(1)?);
            Some((attr, channel, Some(high)))
        }
        _ => None,
    }).collect()
}

/// One interpolation of a parameter: channel, half, and whether it takes a
/// vertex's own value (`v_interp_mov_f32`) instead of interpolating.
type Interpolation = (usize, Option<bool>, bool);

fn interpolations(insns: &[Insn]) -> BTreeMap<usize, BTreeSet<Interpolation>> {
    let mut by_parameter: BTreeMap<usize, BTreeSet<Interpolation>> = BTreeMap::new();
    for ins in insns {
        let own = ins.mnemonic == "v_interp_mov_f32";
        for (parameter, channel, half) in reads(std::slice::from_ref(ins)) {
            by_parameter.entry(parameter).or_default().insert((channel, half, own));
        }
    }
    by_parameter
}

/// The `count` parameters, taken in order from `all`, that the program's reads
/// force. A console compiler can drop a parameter the signature still calls
/// read, so the declarations alone don't say which are left. A choice must give
/// each interpolated parameter a place that has every component it reads: of the
/// same width, declared read, and not a constant register unless the program
/// takes a vertex's own value. `None` unless some choice fits and every choice
/// that fits reads each component from the same register.
fn forced(
    all: &[Param], sig: &[SigElement], flat: &[u32], count: usize,
    reads: &BTreeMap<usize, BTreeSet<Interpolation>>,
) -> Option<Vec<Param>> {
    if count == 0 || count > all.len() || reads.keys().any(|&parameter| parameter >= count) {
        return None;
    }
    let source = |param: &Param, channel: usize, half: Option<bool>| {
        if matches!(param, Param::Half(_)) != half.is_some() {
            return None;
        }
        component(std::slice::from_ref(param), (0, channel, half))
    };
    let fits = |param: &Param, parameter: usize| {
        reads.get(&parameter).into_iter().flatten().all(|&(channel, half, own)| {
            source(param, channel, half).is_some_and(|(register, component)| {
                let rows = || sig.iter().filter(|e| e.sysval == 0 && e.register == register);
                component < 4 && rows().next().is_some() && (own || !flat.contains(&register))
                    && (rows().any(|e| e.mask & e.rw_mask & (1 << component) != 0)
                        // VINTRP supplies a default for a channel the register lacks.
                        || half.is_none() && !rows().any(|e| e.mask & (1 << component) != 0))
            })
        })
    };
    // Places each parameter can take with the ones before it placed, then of
    // those the places that leave room for the ones after it.
    let mut places: Vec<Vec<usize>> = Vec::with_capacity(count);
    for parameter in 0..count {
        let first = match places.last() {
            Some(before) => before.first()? + 1,
            None => 0,
        };
        places.push((first..all.len()).filter(|&at| fits(&all[at], parameter)).collect());
    }
    for parameter in (0..count - 1).rev() {
        let last = *places[parameter + 1].last()?;
        places[parameter].retain(|&at| at < last);
    }
    for (&parameter, wanted) in reads {
        for &(channel, half, _) in wanted {
            let found: BTreeSet<_> = places[parameter].iter().map(|&at| source(&all[at], channel, half)).collect();
            if found.len() != 1 {
                return None;
            }
        }
    }
    let mut next = 0;
    places.iter().map(|choices| {
        let at = *choices.iter().find(|&&at| at >= next)?;
        next = at + 1;
        Some(all[at].clone())
    }).collect()
}

/// Whether `params` carries a component the program declares it reads there.
fn carries(params: &[Param], sig: &[SigElement], (attr, channel, half): (usize, usize, Option<bool>)) -> bool {
    let Some((register, component)) = component(params, (attr, channel, half)) else { return false };
    sig.iter().any(|e| e.register == register && e.rw_mask >> component & 1 != 0)
}

/// Registers whose elements the pipeline validation part marks as constant over a primitive:
/// inputs, then outputs.
pub(crate) fn flat_registers(psv: &[u8]) -> Option<(Vec<u32>, Vec<u32>)> {
    fn word(b: &[u8], at: usize) -> Option<usize> {
        Some(u32::from_le_bytes(*b.get(at..)?.first_chunk()?) as usize)
    }
    let runtime = psv.get(4..)?.get(..word(psv, 0)?)?;
    let (inputs, outputs) = (*runtime.get(28)? as usize, *runtime.get(29)? as usize);
    let mut at = 4 + runtime.len();
    let resources = word(psv, at)?;
    at += 4;
    if resources != 0 {
        at += 4 + word(psv, at)?.checked_mul(resources)?;
    }
    for scale in [1, 4] {
        at += 4 + word(psv, at)?.checked_mul(scale)?;
    }
    let size = word(psv, at)?;
    at += 4;
    let mut flat = (Vec::new(), Vec::new());
    for n in 0..inputs + outputs {
        let e = psv.get(at + n.checked_mul(size)?..)?.get(..size)?;
        let (rows, start, kind, mode) = (*e.get(8)?, *e.get(9)?, *e.get(11)?, *e.get(13)?);
        if kind == 0 && mode == 1 {
            let list = if n < inputs { &mut flat.0 } else { &mut flat.1 };
            list.extend((0..u32::from(rows)).map(|r| u32::from(start) + r));
        }
    }
    Some(flat)
}

#[cfg(test)]
#[path = "../tests/hlsl/params.rs"]
mod tests;
