//! Which console instructions the emitted programs hold, for the shader harness.
use std::collections::BTreeSet;
use std::io::Write;

use super::Insn;

/// With `TUXBOX_XSHADER_COVERAGE` naming a file, append one line for each
/// instruction of every program emitted. The translation does not change.
pub(super) fn record(insns: &[Insn]) {
    let Some(path) = std::env::var_os("TUXBOX_XSHADER_COVERAGE") else { return };
    let names: BTreeSet<&str> = insns.iter().map(|op| op.mnemonic.as_str()).collect();
    let text: String = names.into_iter().map(|name| format!("{name}\n")).collect();
    // One write keeps lines whole when several translations append at once.
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
    }
}
