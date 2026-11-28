//! Texture sizes read out of a descriptor.
//!
//! A console program can take a texture's size from the descriptor it loaded
//! instead of asking for it. The image descriptor keeps the width less one in
//! bits 30-31 of word 1 and 0-11 of word 2, the height less one from bit 14 of
//! word 2, and the view's first mip level in bits 12-15 of word 3
//! (`SQ_IMG_RSRC_WORD1..3` in Mesa's `gfx10-rsrc.json`: WIDTH_LO, WIDTH_HI,
//! HEIGHT, BASE_LEVEL). Compiled code computes `(field + 1) >> level`. It
//! takes each field as 16 bits, two more than Mesa names; a size the host
//! can report (at most 16384) leaves those two zero either way.
//!
//! The host descriptor has no words, and a shader can only ask for the size
//! of the view's first mip. So the load fills these fields with that size and
//! level 0, which gives the same `(field + 1) >> level` wherever the console's
//! shift leaves at least one texel. That is exact only for this computation,
//! so the fields are followed from the descriptor to the shift, and a program
//! that does anything else with them is refused.
//!
//! A buffer descriptor keeps its record count in word 2 (`SQ_BUF_RSRC_WORD2`),
//! which is what `GetDimensions` returned to the program's source: bytes for
//! a raw buffer, elements otherwise. The count is the whole word, so the load
//! fills it and every later use is right. A structured buffer is counted only
//! when the translation declares its element with the stride the program's
//! records give, since a host may divide the view's size by that element.
use super::*;
use super::operand_fields::{constant_before, memory, run_start, unused_field};
use crate::rdna2::clobbers_scalar;

/// A value on the way from a descriptor to its texture's size. `u32` is the
/// descriptor (`table << 24 | offset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SizePart {
    /// Bits 30-31 of word 1, shifted down.
    WidthLow(u32),
    /// Word 2 shifted up by two.
    WidthHigh(u32),
    /// The two together, with the height above bit 13.
    WidthAndMore(u32),
    WidthLessOne(u32),
    Width(u32),
    HeightLessOne(u32),
    Height(u32),
    Level(u32),
    /// Some size value on one path and not on another.
    Lost,
}

/// Dataflow key of the size fact of an SGPR, beside the descriptor facts.
pub(super) const KEY: u16 = 3000;

fn constant(insns: &[Insn], at: usize, operand: Option<&Opnd>) -> Option<u32> {
    match operand {
        Some(Opnd::ConstInt(value)) => Some(*value as u32),
        Some(Opnd::Literal(value)) => Some(*value),
        Some(Opnd::Sgpr(register)) => constant_before(insns, at, *register),
        _ => None,
    }
}

/// The mask that leaves the width less one: 14 bits as Mesa names the field,
/// or the 16 the console's compiler takes (the height is extracted as 16 too).
fn width_mask(mask: Option<u32>) -> bool {
    matches!(mask, Some(0x3fff | 0xffff))
}

impl<'a> Emitter<'a> {
    /// Follow size values through one instruction. Runs before the instruction's
    /// own descriptor transfer, which never touches these keys.
    pub(super) fn step_texture_size(&mut self, insns: &[Insn], at: usize, sym: &mut HashMap<u16, Sym>) -> Result<(), String> {
        let ins = &insns[at];
        if ins.mnemonic == "v_nop" {
            return Ok(());
        }
        let part = |sym: &HashMap<u16, Sym>, operand: Option<&Opnd>| match operand {
            Some(Opnd::Sgpr(register)) => match sym.get(&(KEY + u16::from(*register))) {
                Some(Sym::Size(part)) => Some(*part),
                _ => None,
            },
            _ => None,
        };
        let word = |sym: &HashMap<u16, Sym>, operand: Option<&Opnd>| match operand {
            Some(Opnd::Sgpr(register)) => match sym.get(&u16::from(*register)) {
                Some(Sym::Desc { table, off, k }) => Some(((u32::from(*table) << 24) | off, *k)),
                _ => None,
            },
            _ => None,
        };
        let (a, b) = (ins.src.first(), ins.src.get(1));
        let scalar = ins.dst.len() == 1 && matches!(ins.dst.first(), Some(Opnd::Sgpr(_)));
        let result = match ins.mnemonic.as_str() {
            _ if !scalar => None,
            "s_mov_b32" => part(sym, a),
            "s_lshr_b32" => match (word(sym, a), part(sym, a), part(sym, b)) {
                (Some((id, 1)), _, _) if constant(insns, at, b) == Some(30) => Some(SizePart::WidthLow(id)),
                // The size at the view's first mip: an ordinary number from here on.
                (_, Some(SizePart::Width(id) | SizePart::Height(id)), Some(SizePart::Level(level))) if id == level => {
                    self.clear_size(ins, sym);
                    return Ok(());
                }
                _ => None,
            },
            "s_lshl_b32" => match word(sym, a) {
                Some((id, 2)) if constant(insns, at, b) == Some(2) => Some(SizePart::WidthHigh(id)),
                _ => None,
            },
            "s_bfe_u32" => match (word(sym, a), constant(insns, at, b)) {
                (Some((id, 2)), Some(0x10000e)) => Some(SizePart::HeightLessOne(id)),
                (Some((id, 3)), Some(0x4000c)) => Some(SizePart::Level(id)),
                _ => None,
            },
            "s_or_b32" => match (part(sym, a), part(sym, b)) {
                (Some(SizePart::WidthLow(low)), Some(SizePart::WidthHigh(high)))
                | (Some(SizePart::WidthHigh(high)), Some(SizePart::WidthLow(low))) if low == high => Some(SizePart::WidthAndMore(low)),
                _ => None,
            },
            "s_and_b32" => match (part(sym, a), part(sym, b)) {
                (Some(SizePart::WidthAndMore(id)), None) if width_mask(constant(insns, at, b)) => Some(SizePart::WidthLessOne(id)),
                (None, Some(SizePart::WidthAndMore(id))) if width_mask(constant(insns, at, a)) => Some(SizePart::WidthLessOne(id)),
                _ => None,
            },
            "s_add_i32" | "s_add_u32" => {
                let (value, one) = match (part(sym, a), part(sym, b)) {
                    (Some(value), None) => (Some(value), constant(insns, at, b)),
                    (None, Some(value)) => (Some(value), constant(insns, at, a)),
                    _ => (None, None),
                };
                match (value, one) {
                    (Some(SizePart::WidthLessOne(id)), Some(1)) => Some(SizePart::Width(id)),
                    (Some(SizePart::HeightLessOne(id)), Some(1)) => Some(SizePart::Height(id)),
                    _ => None,
                }
            }
            _ => None,
        };
        if result.is_none() {
            // Any other read of a size value is not the computation described above.
            for (index, source) in ins.src.iter().enumerate() {
                let Opnd::Sgpr(register) = *source else { continue };
                if unused_field(ins, index, register) {
                    continue;
                }
                if part(sym, Some(source)).is_some() {
                    return Err(format!("texture size field from a descriptor used outside its size computation at {:#x}", ins.offset));
                }
            }
        }
        self.clear_size(ins, sym);
        if let (Some(part), Some(Opnd::Sgpr(register))) = (result, ins.dst.first()) {
            if let SizePart::WidthLow(id) | SizePart::WidthHigh(id) | SizePart::HeightLessOne(id) | SizePart::Level(id) = part {
                if ins.mnemonic != "s_mov_b32" {
                    self.size_reads.insert(id);
                }
            }
            sym.insert(KEY + u16::from(*register), Sym::Size(part));
        }
        Ok(())
    }

    /// Forget the size fact of every scalar register the instruction writes.
    fn clear_size(&self, ins: &Insn, sym: &mut HashMap<u16, Sym>) {
        sym.retain(|&key, _| key < KEY || !clobbers_scalar(ins, (key - KEY) as u8));
    }

    /// Fill the size fields of a descriptor load from the texture it names.
    pub(super) fn emit_texture_size(&mut self, idx: usize) -> Result<(), String> {
        let Some(&(first, id)) = self.dsel_loads.get(&idx) else { return Ok(()) };
        if !self.size_reads.contains(&id) {
            return Ok(());
        }
        let (table, offset) = ((id >> 24) as u8, id & 0x00ff_ffff);
        if self.insns[idx].op != 3 {
            return Err("texture size read from a descriptor that is not eight words".into());
        }
        let (kind, slot) = self.table_binding(table, offset, 32)?;
        // Dimension codes as the image instructions record them.
        let (texture, outputs) = match kind {
            Some(0) => (format!("tex{slot}"), match self.tex.get(&slot) {
                Some(1 | 3) => "0u, w, h, n",
                Some(5) => "0u, w, h, e, n",
                Some(2) => "0u, w, h, d, n",
                _ => return Err("texture size read from a descriptor no supported image instruction uses".into()),
            }),
            Some(1) => (format!("uav{slot}"), match self.uav.get(&slot) {
                Some(1) => "w, h",
                Some(5) => "w, h, e",
                Some(2) => "w, h, d",
                _ => return Err("texture size read from a descriptor no supported image instruction uses".into()),
            }),
            _ => return Err("texture size read from a descriptor that is not a texture".into()),
        };
        let first = u8::try_from(first).ok().filter(|&n| n < 102).ok_or("descriptor words outside the scalar registers")?;
        let t = self.tmp;
        self.tmp += 1;
        let names: Vec<String> = outputs.split(", ").map(|name| if name == "0u" { name.to_string() } else { format!("{name}{t}") }).collect();
        let declared: Vec<&str> = names.iter().map(String::as_str).filter(|name| *name != "0u").collect();
        self.line(&format!("uint {}; {texture}.GetDimensions({});", declared.join(", "), names.join(", ")));
        // A null view reports no texels. The console's empty descriptor holds zero
        // in these fields, which reads as one texel each way.
        let [width, height] = ["w", "h"].map(|name| format!("(max({name}{t}, 1u) - 1u)"));
        self.ws(&Opnd::Sgpr(first + 1), &format!("({width} & 3u) << 30"))?;
        self.ws(&Opnd::Sgpr(first + 2), &format!("({width} >> 2) | ({height} << 14)"))?;
        self.ws(&Opnd::Sgpr(first + 3), "0u")
    }
}

impl<'a> Emitter<'a> {
    /// Record a read of word 2 of a four-word descriptor as a number.
    pub(super) fn note_buffer_count(&mut self, insns: &[Insn], at: usize, sym: &HashMap<u16, Sym>) {
        let ins = &insns[at];
        // These take a descriptor as a descriptor, and a move carries the fact along,
        // as does a copy `descriptor_broadcast` proved a waterfall reads back.
        if memory(ins) || matches!(ins.mnemonic.as_str(), "v_nop" | "s_mov_b32" | "s_mov_b64" | "s_getpc_b64")
            || self.descriptor_copies.contains(&at) {
            return;
        }
        for (index, source) in ins.src.iter().enumerate() {
            let Opnd::Sgpr(register) = *source else { continue };
            if unused_field(ins, index, register) {
                continue;
            }
            let Some(Sym::Desc { table, off, k: 2 }) = sym.get(&u16::from(register)).copied() else { continue };
            let id = (u32::from(table) << 24) | off;
            let loads: Vec<usize> = self.dsel_loads.iter().filter(|&(_, &(_, loaded))| loaded == id).map(|(&load, _)| load).collect();
            if loads.is_empty() || loads.iter().any(|&load| insns[load].op != 2) {
                continue;
            }
            // Certain when the load is in the same straight run and nothing
            // wrote the register since: the descriptor facts are optimistic
            // where paths join.
            // A waterfall's read of the copied word is as sure as the word it copied,
            // except in the loop's own test for the lanes that hold that descriptor.
            let read_back = !ins.mnemonic.starts_with("v_cmp_eq_u")
                && (run_start(insns, at)..at).rev()
                    .find(|&k| clobbers_scalar(&insns[k], register))
                    .is_some_and(|k| self.descriptor_reads.contains(&k));
            let certain = read_back || loads.iter().any(|&load| load < at && load >= run_start(insns, at)
                && self.dsel_loads[&load].0 + 2 == u16::from(register)
                && !(load + 1..at).any(|between| clobbers_scalar(&insns[between], register)));
            *self.count_reads.entry(id).or_insert(false) |= certain;
        }
    }

    /// Fill the record count of a buffer descriptor load.
    pub(super) fn emit_buffer_count(&mut self, idx: usize) -> Result<(), String> {
        let Some(&(first, id)) = self.dsel_loads.get(&idx) else { return Ok(()) };
        let Some(&certain) = self.count_reads.get(&id) else { return Ok(()) };
        let (table, offset) = ((id >> 24) as u8, id & 0x00ff_ffff);
        let query = self.table_binding(table, offset, 32).ok().and_then(|(kind, slot)| {
            let is_uav = match kind? { 0 => false, 1 => true, _ => return None };
            // The view type PSV names decides what the console counted.
            let space = self.binding_spaces.get(&(u32::from(is_uav), slot)).copied().unwrap_or(0);
            let (native, space) = self.native_binding(is_uav, slot, space);
            let base = if is_uav { 6 } else { 3 };
            let mut types = self.ex.resources.iter()
                .filter(|p| p.space == space && p.lower <= native && native <= p.upper && (base..base + 3).contains(&p.res_type))
                .map(|p| p.res_type - base);
            let ty = types.next()?;
            if types.next().is_some() {
                return None;
            }
            let key = (is_uav, slot);
            // A buffer the program asks only for its count is declared for the query.
            if ![self.tbuf.contains(&key), self.raw.contains(&key), self.sbuf.contains_key(&key)].contains(&true) {
                match ty {
                    0 => { self.tbuf.insert(key); }
                    1 => { self.raw.insert(key); }
                    _ => {
                        let words = self.table_structured_stride(slot, space, is_uav).or_else(|| {
                            if space != 0 || (native, space) != (slot, space) { return None; }
                            fssm_bindings::stride_words(&self.ex.fssm, &self.ex.resources, self.ex.stage, slot, is_uav)
                        })?;
                        self.sbuf.insert(key, words);
                        self.reflected_strides.insert(key);
                    }
                }
            }
            let declared = [self.tbuf.contains(&key), self.raw.contains(&key), self.sbuf.contains_key(&key)];
            let name = |srv: &str, uav: &str| format!("{}{slot}", if is_uav { uav } else { srv });
            match (ty, declared) {
                (0, [true, false, false]) => Some((name("stb", "utb"), false)),
                (1, [false, true, false]) => Some((name("raw_sbuf", "raw_ubuf"), false)),
                // A host can count records by the element the shader declares, so
                // that element must have the stride the program's records give.
                (2, [false, false, true]) if self.reflected_strides.contains(&key) => Some((name("sbuf", "ubuf"), true)),
                _ => None,
            }
        });
        let Some((buffer, stride)) = query else {
            // A read the dataflow is sure of has no number to use.
            return if certain { Err("buffer record count read from a descriptor with no one host buffer".into()) } else { Ok(()) };
        };
        let first = u8::try_from(first).ok().filter(|&n| n < 102).ok_or("descriptor words outside the scalar registers")?;
        let t = self.tmp;
        self.tmp += 1;
        if stride {
            self.line(&format!("uint n{t}, st{t}; {buffer}.GetDimensions(n{t}, st{t});"));
        } else {
            self.line(&format!("uint n{t}; {buffer}.GetDimensions(n{t});"));
        }
        self.ws(&Opnd::Sgpr(first + 2), &format!("n{t}"))
    }
}

#[cfg(test)]
#[path = "../tests/hlsl/descriptor_size.rs"]
mod tests;
