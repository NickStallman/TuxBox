//! Whether a console program can write to an image.
//!
//! A program that indexes the resource heap directly can reach any view in it,
//! so the provider must assume it writes every writable texture there unless
//! the program has no instruction that writes an image at all.

use crate::rdna2::{self, Enc};
use crate::{Dxbc, fssm, xdat};

/// True unless every variant of `shader` is console code that decodes
/// completely and contains no image store or image atomic.
///
/// The answer errs towards true: a desktop shader, a container this reader
/// does not know, an undecoded instruction and an indirect jump all count as
/// a possible image write.
pub fn may_write_images(shader: &[u8]) -> bool {
    writes_no_image(shader).is_none()
}

fn writes_no_image(shader: &[u8]) -> Option<()> {
    let container = Dxbc::parse(shader).ok()?;
    let part = container.chunk(b"DXIL")?;
    let dxil = shader.get(part.offset..part.offset + part.size)?;
    let vertex = u32::from_le_bytes(*dxil.first_chunk()?) >> 16 == VERTEX;
    let variants = xdat::programs(dxil.get(24..)?).ok()?;
    if variants.is_empty() {
        return None;
    }
    for program in &variants {
        stage_writes_no_image(&fssm::decode(program).ok()?.isa, vertex)?;
    }
    Some(())
}

/// The DXIL program kind of a vertex shader.
const VERTEX: u32 = 1;

/// A vertex program calls the hardware fetch program through `s_swappc_b64`.
/// The translation materialises vertex inputs from the input layout instead and
/// never runs that call, so it cannot write an image through it.
fn translated_away(op: &rdna2::Insn, vertex: bool) -> bool {
    vertex && op.mnemonic == "s_swappc_b64"
}

/// `Some` when the instruction stream is fully decoded, stays inside itself
/// and has no image store or image atomic.
pub fn isa_writes_no_image(isa: &[u8]) -> Option<()> {
    stage_writes_no_image(isa, false)
}

fn stage_writes_no_image(isa: &[u8], vertex: bool) -> Option<()> {
    if isa.is_empty() || !isa.len().is_multiple_of(4) {
        return None;
    }
    for op in rdna2::instructions(isa) {
        if !op.is_complete()
            || op.enc == Enc::Unknown
            || op.mnemonic.contains('<')
            // Code reached through a computed address is not in this stream.
            || !translated_away(&op, vertex) && matches!(
                op.mnemonic.as_str(),
                "s_call_b64" | "s_setpc_b64" | "s_swappc_b64" | "s_rfe_b64" | "s_trap"
            )
            || op.mnemonic.starts_with("image_store")
            || op.mnemonic.starts_with("image_atomic")
        {
            return None;
        }
    }
    Some(())
}

/// One descriptor a program stores an image through: `slot` descriptors past
/// the start of the table bound at root parameter `table`.
pub type TableSlot = (u32, u32);

/// The table descriptors `shader` can write an image through, or `None` when
/// it may write through something else: a view picked from the resource heap,
/// a descriptor this reader cannot trace, or anything `may_write_images`
/// already counts as unknown. An empty list is a program that writes no image.
pub fn table_image_writes(shader: &[u8]) -> Option<Vec<TableSlot>> {
    let container = Dxbc::parse(shader).ok()?;
    let part = container.chunk(b"DXIL")?;
    let dxil = shader.get(part.offset..part.offset + part.size)?;
    let stage = u32::from_le_bytes(*dxil.first_chunk()?) >> 16;
    let vertex = stage == VERTEX;
    let variants = xdat::programs(dxil.get(24..)?).ok()?;
    if variants.is_empty() {
        return None;
    }
    let mut slots = Vec::new();
    for program in &variants {
        let code = fssm::decode(program).ok()?;
        if stage_writes_no_image(&code.isa, vertex).is_some() {
            continue;
        }
        // Each descriptor's user-data record names the register its table
        // pointer arrives in and that table's root parameter. Table N arrives
        // in a fixed register for the stage; anything else is not a layout
        // this reader knows.
        let first = match stage {
            5 => 4,
            0 | 1 => 8,
            _ => 0xff,
        };
        let mut tables = Vec::new();
        for [class, _, _, _, location] in crate::compute_table::user_data_records(program)? {
            if class == 3 {
                let (register, table) = (u8::try_from(location & 0xff).ok()?, location >> 8);
                if u32::from(register).checked_sub(first) != Some(table) {
                    return None;
                }
                if !tables.contains(&(register, table)) {
                    tables.push((register, table));
                }
            }
        }
        for slot in stage_table_image_writes(&code.isa, &tables, vertex)? {
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
    }
    slots.sort_unstable();
    Some(slots)
}

/// What a scalar register holds, as far as the trace below can tell.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    /// The value register `n` had when the program started.
    Entry(u8),
    Constant(u32),
    /// Dword `part` of the image descriptor loaded at site `site`.
    Descriptor(u16, u8),
    Unknown,
}

const REGISTERS: usize = 128;
/// The SGPR number that means "no register" in a scalar load's address.
const NO_REGISTER: u32 = 125;

/// The table slots the image stores and atomics of `isa` write through, given
/// the registers that hold table pointers on entry as `(register, table)`.
/// `None` when a store's descriptor does not come, on every path, from one
/// eight-dword load at a constant offset of such a pointer.
pub fn isa_table_image_writes(isa: &[u8], tables: &[(u8, u32)]) -> Option<Vec<TableSlot>> {
    stage_table_image_writes(isa, tables, false)
}

fn stage_table_image_writes(isa: &[u8], tables: &[(u8, u32)], vertex: bool) -> Option<Vec<TableSlot>> {
    decodes(isa, vertex)?;
    let ops = rdna2::disassemble(isa);
    let writes = |op: &rdna2::Insn| op.mnemonic.starts_with("image_store") || op.mnemonic.starts_with("image_atomic");
    if !ops.iter().any(writes) {
        return Some(Vec::new());
    }
    // Where control goes after each instruction, and which registers it writes.
    let mut successors = Vec::with_capacity(ops.len());
    let mut clobbers = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let name = op.mnemonic.as_str();
        if name.starts_with("s_movrel") || name.starts_with("s_subvector_loop") {
            return None;
        }
        let target = if name == "s_branch" || name.starts_with("s_cbranch_") {
            if op.enc != Enc::Sopp {
                return None;
            }
            let address = (op.offset + op.dwords * 4).checked_add_signed(isize::try_from(op.aux.imm * 4).ok()?)?;
            Some(ops.binary_search_by_key(&address, |other| other.offset).ok()?)
        } else {
            None
        };
        let next = (name != "s_branch" && name != "s_endpgm").then_some(index + 1).filter(|next| *next < ops.len());
        successors.push((next, target));
        // s_setreg writes a hardware register; the scalar it names is its source.
        clobbers.push(if name.starts_with("s_setreg") {
            Vec::new()
        } else {
            (0..REGISTERS as u8).filter(|&register| rdna2::clobbers_scalar(op, register)).collect::<Vec<_>>()
        });
    }
    let mut sites: Vec<TableSlot> = Vec::new();
    let mut entry: Vec<Option<Box<[Value; REGISTERS]>>> = vec![None; ops.len()];
    entry[0] = Some(Box::new(std::array::from_fn(|register| Value::Entry(register as u8))));
    let mut pending = vec![0usize];
    let mut written = Vec::new();
    while let Some(index) = pending.pop() {
        let op = &ops[index];
        let mut state = entry[index].clone()?;
        let before = state.clone();
        let read = |register: u8| before.get(usize::from(register)).copied().unwrap_or(Value::Unknown);
        for &register in &clobbers[index] {
            state[usize::from(register)] = Value::Unknown;
        }
        match (op.mnemonic.as_str(), op.dst.first(), op.src.first()) {
            ("s_mov_b32", Some(&rdna2::Opnd::Sgpr(to)), Some(source)) if usize::from(to) < REGISTERS => {
                state[usize::from(to)] = match *source {
                    rdna2::Opnd::Sgpr(from) => read(from),
                    rdna2::Opnd::ConstInt(value) => u32::try_from(value).map_or(Value::Unknown, Value::Constant),
                    rdna2::Opnd::Literal(value) => Value::Constant(value),
                    _ => Value::Unknown,
                };
            }
            ("s_load_dwordx8", Some(&rdna2::Opnd::Sgpr(to)), Some(&rdna2::Opnd::Sgpr(base)))
                if usize::from(to) + 8 <= REGISTERS =>
            {
                // A table pointer is 32 bits wide, so the program clears the
                // high half of the address itself.
                let table = match (read(base), read(base.saturating_add(1))) {
                    (Value::Entry(register), Value::Constant(0)) => {
                        tables.iter().find(|(known, _)| *known == register).map(|&(_, table)| table)
                    }
                    _ => None,
                };
                let offset = u32::try_from(op.aux.imm).ok();
                let plain = op.raw.get(1).is_some_and(|word| word >> 25 == NO_REGISTER);
                if let (Some(table), Some(offset), true) = (table, offset.filter(|offset| offset % 32 == 0), plain) {
                    let slot = (table, offset / 32);
                    let site = sites.iter().position(|known| *known == slot).unwrap_or_else(|| {
                        sites.push(slot);
                        sites.len() - 1
                    });
                    for part in 0..8u8 {
                        state[usize::from(to) + usize::from(part)] = Value::Descriptor(u16::try_from(site).ok()?, part);
                    }
                }
            }
            _ => {}
        }
        if writes(op) {
            // The image descriptor is the operand before the sampler.
            let &rdna2::Opnd::Sgpr(descriptor) = op.src.get(op.src.len().checked_sub(2)?)? else { return None };
            let Value::Descriptor(site, 0) = read(descriptor) else { return None };
            if (1..8u8).any(|part| read(descriptor.saturating_add(part)) != Value::Descriptor(site, part)) {
                return None;
            }
            let slot = sites[usize::from(site)];
            if !written.contains(&slot) {
                written.push(slot);
            }
        }
        let (next, target) = successors[index];
        for successor in [next, target].into_iter().flatten() {
            let merged = match &entry[successor] {
                None => state.clone(),
                Some(known) => {
                    let mut merged = known.clone();
                    for (slot, value) in merged.iter_mut().zip(state.iter()) {
                        if *slot != *value {
                            *slot = Value::Unknown;
                        }
                    }
                    merged
                }
            };
            if entry[successor].as_ref() != Some(&merged) {
                entry[successor] = Some(merged);
                pending.push(successor);
            }
        }
    }
    written.sort_unstable();
    Some(written)
}

/// `Some` when the stream is fully decoded and stays inside itself.
fn decodes(isa: &[u8], vertex: bool) -> Option<()> {
    if isa.is_empty() || !isa.len().is_multiple_of(4) {
        return None;
    }
    for op in rdna2::instructions(isa) {
        if !op.is_complete()
            || op.enc == Enc::Unknown
            || op.mnemonic.contains('<')
            || !translated_away(&op, vertex) && matches!(
                op.mnemonic.as_str(),
                "s_call_b64" | "s_setpc_b64" | "s_swappc_b64" | "s_rfe_b64" | "s_trap"
            )
        {
            return None;
        }
    }
    Some(())
}

#[cfg(test)]
#[path = "tests/image_writes.rs"]
mod tests;

