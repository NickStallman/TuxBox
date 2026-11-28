//! Values exchanged between lanes through group memory.
use super::*;

impl<'a> Emitter<'a> {
    // READLANE ignores EXEC. Publish each lane's current VGPR even when
    // that lane skipped the guest branch, then guard only the load (for
    // partial-group bounds) and scalar write while keeping barriers uniform.
    pub(in crate::hlsl) fn emit_indexed_read(&mut self, idx: usize, ins: &Insn, active: &[usize]) -> Result<(), String> {
        if self.dispatched {
            return Err("indexed lane read in a block-dispatched program".into());
        }
        let mask = if ins.aux.imm == 31 { 31u32 } else { 63u32 };
        self.line(&format!("tb_swizzle[tb_lane] = {};", r(&ins.src[0])?));
        self.line("GroupMemoryBarrierWithGroupSync();");
        let load = format!("tb_swizzle_read((tb_lane & ~{mask}u) | ({} & {mask}u))", r(&ins.src[1])?);
        if active.is_empty() {
            self.line(&format!("uint readlane{idx} = {load};"));
        } else {
            let guard: Vec<_> = active.iter().map(|&label| not_skipping(label)).collect();
            self.line(&format!("uint readlane{idx} = ({}) ? {load} : 0u;", guard.join(" && ")));
        }
        self.line("GroupMemoryBarrierWithGroupSync();");
        Ok(())
    }

    pub(in crate::hlsl) fn emit_compute_swizzle(&mut self, idx: usize, ins: &Insn, active: &[usize]) -> Result<(), String> {
        if ins.mnemonic == GROUP_GATHER {
            // Every lane publishes its value; each reads the lane its index names.
            let guard: Vec<_> = active.iter().map(|&label| not_skipping(label)).collect();
            let value = r(&ins.src[0])?;
            let value = if guard.is_empty() { value } else { format!("({}) ? {value} : 0u", guard.join(" && ")) };
            self.line(&format!("tb_swizzle[tb_lane] = {value};"));
            self.line("GroupMemoryBarrierWithGroupSync();");
            self.line(&format!("uint swizzle{idx} = tb_swizzle_read((tb_lane & ~63u) | ({} & 63u));", r(&ins.src[1])?));
            self.line("GroupMemoryBarrierWithGroupSync();");
            return Ok(());
        }
        if ins.mnemonic == GROUP_READ {
            let mask = if ins.aux.imm == 31 { 31u32 } else { 63u32 };
            if self.lockstep.is_some() && self.required_wave_size == Some(mask + 1) {
                // The whole wave is here (see `lockstep`), so the lane is read directly.
                self.line(&format!("uint swizzle{idx} = WaveReadLaneAt({}, {} & {mask}u);", r(&ins.src[0])?, r(&ins.src[1])?));
                return Ok(());
            }
            if self.dispatched || !self.compute_swizzle {
                return Err("group lane read requires structured compute swizzle lowering".into());
            }
            let width = mask + 1;
            let count = compute_group(self.ex)
                .into_iter()
                .try_fold(1u32, u32::checked_mul)
                .unwrap_or(0);
            if count == 0 || count > 1024 || count % width != 0 {
                return Err(format!("group lane read requires complete blocks of {width} lanes"));
            }
            // READLANE ignores EXEC. Publish each lane's actual VGPR even when
            // that lane skipped the guest branch, then guard only the load on
            // active skip flags and mask the selector to 0..width-1. Inside an
            // accepted loop the lane was kept in the loop but already finished:
            // mask the publish too, so the remaining lanes read a zero.
            let value = r(&ins.src[0])?;
            let value = if self.in_accepted_swizzle_loop() && !active.is_empty() {
                let guard: Vec<_> = active.iter().map(|&label| not_skipping(label)).collect();
                format!("({}) ? {value} : 0u", guard.join(" && "))
            } else {
                value
            };
            self.line(&format!("tb_swizzle[tb_lane] = {value};"));
            self.line("GroupMemoryBarrierWithGroupSync();");
            let load = format!("tb_swizzle_read((tb_lane & ~{mask}u) | ({} & {mask}u))", r(&ins.src[1])?);
            if active.is_empty() {
                self.line(&format!("uint swizzle{idx} = {load};"));
            } else {
                let guard: Vec<_> = active.iter().map(|&label| not_skipping(label)).collect();
                self.line(&format!("uint swizzle{idx} = ({}) ? {load} : 0u;", guard.join(" && ")));
            }
            self.line("GroupMemoryBarrierWithGroupSync();");
            return Ok(());
        }
        let mode = ins.aux.imm as u32;
        // RDNA2 ISA DS_SWIZZLE_B32: offset[15] selects QDMode, which permutes
        // within each quad by offset[7:0]. Other high offset bits and GDS stay refused.
        let quad = mode & 0x8000 != 0;
        if ins.raw[0] & 0x3_0000 != 0 || (quad && mode & 0xff00 != 0x8000) {
            return Err("unsupported compute DS swizzle encoding".into());
        }
        // Skipped and EXEC-masked lanes contribute zero. Rendezvous before
        // reapplying the guest branch guard to the destination write.
        let mut predicate = String::from("(exec_lo & 1u) != 0u");
        for &label in active { write!(predicate, " && {}", not_skipping(label)).unwrap(); }
        let value = r(&ins.src[0])?;
        self.line(&format!("tb_swizzle[tb_lane] = ({predicate}) ? {value} : 0u;"));
        self.line("GroupMemoryBarrierWithGroupSync();");
        if quad {
            self.line(&format!("uint swizzle{idx} = tb_swizzle[(tb_lane & ~3u) | (({:#x}u >> (2u * (tb_lane & 3u))) & 3u)];",
                mode & 255));
        } else {
            self.line(&format!("uint swizzle{idx} = tb_swizzle[(tb_lane & ~31u) | (((tb_lane & {}u) | {}u) ^ {}u)];",
                mode & 31, (mode >> 5) & 31, (mode >> 10) & 31));
        }
        self.line("GroupMemoryBarrierWithGroupSync();");
        Ok(())
    }
}
