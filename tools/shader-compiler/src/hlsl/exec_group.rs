// Consecutive per-lane VGPR writes observing the same EXEC mask bit can share
// one predicated block, including dead-outside block-local temporaries declared
// inside the chain. Guard mutations, incompatible masks, live-after temps,
// wave/quad collectives, derivatives, out-arg intrinsics, barriers and memory
// accesses always close the region.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    ExecLo1u,
    ExecLoLaneBit,
}

impl Guard {
    fn expr(self) -> &'static str {
        match self {
            Self::ExecLo1u => "exec_lo & 1u",
            Self::ExecLoLaneBit => "exec_lo & tb_lane_bit",
        }
    }
}

enum LineKind<'a> {
    Guarded {
        indent: &'a str,
        guard: Guard,
        stmt: &'a str,
        expr: &'a str,
    },
    TempDecl {
        indent: &'a str,
        decls: Vec<(&'a str, &'a str)>,
    },
    Comment,
    Other,
}

struct Line<'a> {
    raw: &'a str,
    start: usize,
    end: usize,
    kind: LineKind<'a>,
}

struct BodyScan<'a> {
    line_scope: Vec<usize>,
    scope_close: Vec<usize>,
    line_shadows: Vec<bool>,
    line_has_block_or_quote: Vec<bool>,
    temp_uses: HashMap<&'a str, Vec<usize>>,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_vgpr_name(s: &str) -> bool {
    s.strip_prefix('v')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()))
}

fn is_sdwa_temp(s: &str) -> bool {
    s.strip_prefix("tb_s")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()))
}

fn is_indexed_reg(s: &str, prefix: char) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()))
}

fn is_block_local_temp(token: &str) -> bool {
    if is_sdwa_temp(token) {
        return true;
    }
    if let Some(tail) = token.strip_prefix("diff") {
        return !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit());
    }
    let Some(tail) = token.strip_prefix('t') else {
        return false;
    };
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let suffix = &tail[digits..];
    matches!(
        suffix,
        "" | "a"
            | "b"
            | "c"
            | "o"
            | "w"
            | "l"
            | "h"
            | "lo"
            | "hi"
            | "sum"
            | "co"
            | "shift"
            | "byte"
            | "f64"
            | "value"
            | "quad"
    ) || suffix
        .strip_prefix('_')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

fn is_scalar_value_ident(token: &str) -> bool {
    if is_vgpr_name(token) || is_indexed_reg(token, 's') || is_block_local_temp(token) {
        return true;
    }
    if let Some(tail) = token.strip_prefix('q') {
        let base = tail
            .strip_suffix("active")
            .or_else(|| tail.strip_suffix("pair"))
            .unwrap_or(tail);
        if !base.is_empty() && base.bytes().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    if let Some(tail) = token.strip_prefix("tb_pk") {
        let base = tail
            .strip_suffix('a')
            .or_else(|| tail.strip_suffix('b'))
            .unwrap_or("");
        if !base.is_empty() && base.bytes().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    for prefix in [
        "select_mask",
        "saved_lane",
        "saved_exec",
        "tb_votes",
        "swizzle",
        "readlane",
    ] {
        if let Some(tail) = token.strip_prefix(prefix) {
            if !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    if let Some(tail) = token.strip_prefix("tb_bit") {
        let base = tail
            .strip_suffix("result")
            .or_else(|| tail.strip_suffix(['l', 'h', 'i']))
            .unwrap_or(tail);
        if !base.is_empty() && base.bytes().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    matches!(
        token,
        "vcc_lo"
            | "vcc_hi"
            | "vcc"
            | "exec_lo"
            | "exec_hi"
            | "scc"
            | "m0"
            | "nul"
            | "tb_fp_mode"
            | "tb_lane"
            | "tb_lane_bit"
            | "true"
            | "false"
    )
}

fn is_decl_type(token: &str) -> bool {
    matches!(
        token,
        "bool"
            | "int"
            | "uint"
            | "float"
            | "half"
            | "min16float"
            | "min16int"
            | "min16uint"
            | "uint2"
            | "uint3"
            | "uint4"
            | "int2"
            | "int3"
            | "int4"
            | "float2"
            | "float3"
            | "float4"
    )
}

fn is_any_decl_head(token: &str) -> bool {
    is_decl_type(token) || matches!(token, "double" | "float16_t")
}

fn is_pure_intrinsic(token: &str) -> bool {
    matches!(
        token,
        "asuint"
            | "asfloat"
            | "asint"
            | "asfloat16"
            | "f16tof32"
            | "f32tof16"
            | "abs"
            | "sign"
            | "min"
            | "max"
            | "clamp"
            | "saturate"
            | "floor"
            | "ceil"
            | "trunc"
            | "round"
            | "frac"
            | "rcp"
            | "mad"
            | "fma"
            | "lerp"
            | "smoothstep"
            | "step"
            | "sqrt"
            | "rsqrt"
            | "exp"
            | "exp2"
            | "log"
            | "log2"
            | "log10"
            | "pow"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "radians"
            | "degrees"
            | "dot"
            | "cross"
            | "length"
            | "distance"
            | "normalize"
            | "reflect"
            | "refract"
            | "all"
            | "any"
            | "isfinite"
            | "isinf"
            | "isnan"
            | "ldexp"
            | "countbits"
            | "reversebits"
            | "firstbitlow"
            | "firstbithigh"
            | "dot4add_i8packed"
            | "dot4add_u8packed"
            | "select"
    )
}

// Verified side-effect-free helpers in `hlsl.rs` and its HLSL includes: no
// wave/quad/lane communication, no out/inout args, no resource/LDS/UAV reads.
fn is_pure_tb_helper(token: &str) -> bool {
    matches!(
        token,
        "tb_bit"
            | "tb_perm_byte"
            | "tb_perm"
            | "tb_cmp_class"
            | "tb_bfe_i32"
            | "tb_mulhi_u32"
            | "tb_dot4_i8"
            | "tb_mulhi_i32"
            | "tb_cubema"
            | "tb_cubesc"
            | "tb_cubetc"
            | "tb_cubeid"
            | "tb_cube_dir"
            | "tb_mull"
            | "tb_log2"
            | "tb_omod"
            | "tb_f16_cvt"
            | "tb_mad24_sat"
            | "tb_f16_rne"
            | "tb_repack_half"
            | "tb_trig_f16"
            | "tb_f16_rtz"
            | "tb_isnan"
            | "tb_min"
            | "tb_max"
            | "tb_med3"
            | "tb_nan_bits"
            | "tb_cvt_u32_f32"
            | "tb_cvt_i32_f32"
            | "tb_round_u8"
            | "tb_integer_to_f32"
            | "tb_fract_f32"
            | "tb_scale_f32"
            | "tb_ldexp_f32"
            | "tb_quad_extrema"
            | "tb_shr64"
            | "tb_field64"
            | "tb_mode_half"
            | "tb_finite_unary_f16"
            | "tb_unary_f16"
            | "tb_half_output"
            | "tb_fma_half"
            | "tb_fmac_half"
            | "tb_norm_f16"
            | "tb_float_relation"
            | "tb_class_f64"
            | "tb_f64_round"
            | "tb_f64_to_i32"
            | "tb_i32_to_f64"
    )
}

fn has_embedded_assignment(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut k = 0;
    while k < bytes.len() {
        if matches!(bytes.get(k..k + 3), Some(b"<<=" | b">>=")) {
            return true;
        }
        if matches!(bytes.get(k..k + 2), Some(b"==" | b"!=" | b"<=" | b">=")) {
            k += 2;
            continue;
        }
        if bytes[k] == b'=' {
            return true;
        }
        k += 1;
    }
    false
}

fn has_valid_dots(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut p = 0;
    while p < bytes.len() {
        if bytes[p] != b'.' {
            p += 1;
            continue;
        }
        let mut digit_start = p;
        while digit_start > 0 && bytes[digit_start - 1].is_ascii_digit() {
            digit_start -= 1;
        }
        let standalone_num_prefix =
            digit_start < p && (digit_start == 0 || !is_ident_byte(bytes[digit_start - 1]));

        let next = bytes.get(p + 1).copied();
        if next.is_some_and(|c| c.is_ascii_digit()) {
            // `.5f` or `1.5`: reject `v0.5`.
            if digit_start < p && !standalone_num_prefix {
                return false;
            }
            if digit_start == p && p > 0 && is_ident_byte(bytes[p - 1]) {
                return false;
            }
            p += 1;
            continue;
        }
        if standalone_num_prefix {
            let mut end = p + 1;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            let suffix = &expr[p + 1..end];
            let valid_float_tail = suffix.is_empty()
                || matches!(suffix, "f" | "F" | "h" | "H" | "l" | "L")
                || suffix.starts_with(['e', 'E']);
            if !valid_float_tail {
                return false;
            }
            let after = bytes[end..]
                .iter()
                .copied()
                .find(|c| !c.is_ascii_whitespace());
            if after == Some(b'(') {
                return false;
            }
            p = end;
            continue;
        }
        // Component swizzle (`.x`, `.xyzw`) on an expression/register; never method calls.
        if p == 0 || !is_ident_byte(bytes[p - 1]) {
            return false;
        }
        let mut end = p + 1;
        while end < bytes.len() && is_ident_byte(bytes[end]) {
            end += 1;
        }
        let swizzle = &bytes[p + 1..end];
        if !(1..=4).contains(&swizzle.len())
            || !swizzle.iter().all(|c| matches!(c, b'x' | b'y' | b'z' | b'w'))
        {
            return false;
        }
        let after = bytes[end..]
            .iter()
            .copied()
            .find(|c| !c.is_ascii_whitespace());
        if after == Some(b'(') {
            return false;
        }
        p = end;
    }
    true
}

fn is_pure_expr(expr: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty()
        || expr.contains(['[', ']', ';', '{', '}', '\\', '#', '"', '\''])
        || expr.contains("++")
        || expr.contains("--")
        || expr.contains("//")
        || expr.contains("/*")
        || has_embedded_assignment(expr)
        || !has_valid_dots(expr)
    {
        return false;
    }
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        if start > 0 && bytes[start - 1] == b'.' {
            continue;
        }
        let token = &expr[start..i];
        if bytes[start].is_ascii_digit() {
            continue;
        }
        let after = bytes[i..]
            .iter()
            .copied()
            .find(|c| !c.is_ascii_whitespace());
        let called = after == Some(b'(');
        let prev = bytes[..start]
            .iter()
            .rev()
            .copied()
            .find(|c| !c.is_ascii_whitespace());
        let cast = prev == Some(b'(') && after == Some(b')') && is_decl_type(token);
        let callable = is_decl_type(token) || is_pure_intrinsic(token) || is_pure_tb_helper(token);
        if called {
            if !callable {
                return false;
            }
        } else if !cast && !is_scalar_value_ident(token) {
            return false;
        }
    }
    true
}

fn parse_guarded(line: &str) -> Option<LineKind<'_>> {
    let trimmed = line.trim_start_matches(' ');
    let indent = &line[..line.len() - trimmed.len()];
    let (guard, stmt) = if let Some(rest) = trimmed.strip_prefix("if (exec_lo & 1u) ") {
        (Guard::ExecLo1u, rest)
    } else {
        let rest = trimmed.strip_prefix("if (exec_lo & tb_lane_bit) ")?;
        (Guard::ExecLoLaneBit, rest)
    };
    if stmt.starts_with('{') {
        return None;
    }
    let (dest, expr) = stmt.strip_suffix(';')?.split_once(" = ")?;
    if (!is_vgpr_name(dest) && !is_sdwa_temp(dest)) || !is_pure_expr(expr) {
        return None;
    }
    Some(LineKind::Guarded {
        indent,
        guard,
        stmt,
        expr,
    })
}

fn split_top_level_commas(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, b) in text.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => {
                parts.push(&text[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&text[start..]);
    Some(parts)
}

fn find_top_level_assign(item: &str) -> Option<(&str, &str)> {
    let bytes = item.as_bytes();
    let mut depth = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'(' => depth += 1,
            b')' => depth = depth.checked_sub(1)?,
            b'=' if depth == 0 => {
                if bytes.get(idx + 1) == Some(&b'=') {
                    return None;
                }
                let prev = idx.checked_sub(1).map(|p| bytes[p]);
                if matches!(prev, Some(b'!' | b'<' | b'>')) {
                    return None;
                }
                return Some((item[..idx].trim(), item[idx + 1..].trim()));
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn parse_temp_decl(line: &str) -> Option<LineKind<'_>> {
    let trimmed = line.trim_start_matches(' ');
    let indent = &line[..line.len() - trimmed.len()];
    if trimmed.contains("//")
        || trimmed.contains("/*")
        || trimmed.contains("*/")
        || trimmed.contains(['[', ']', '{', '}', '\\', '#', '"', '\''])
        || trimmed.contains("++")
        || trimmed.contains("--")
    {
        return None;
    }
    let body = trimmed.strip_suffix(';')?;
    let mut decls = Vec::new();
    for raw_sub in body.split(';') {
        let sub = raw_sub.trim();
        if sub.is_empty() {
            return None;
        }
        let type_end = sub
            .bytes()
            .position(|b| !is_ident_byte(b))
            .filter(|&p| p > 0)?;
        let ty = &sub[..type_end];
        if !is_decl_type(ty) {
            return None;
        }
        let rest = &sub[type_end..];
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        for item in split_top_level_commas(rest.trim())? {
            let (name, rhs) = find_top_level_assign(item.trim())?;
            if !is_block_local_temp(name) || !is_pure_expr(rhs) {
                return None;
            }
            decls.push((name, rhs));
        }
    }
    (!decls.is_empty()).then_some(LineKind::TempDecl { indent, decls })
}

fn classify_line(raw: &str) -> LineKind<'_> {
    if raw.trim_start().starts_with("//") {
        return LineKind::Comment;
    }
    parse_guarded(raw)
        .or_else(|| parse_temp_decl(raw))
        .unwrap_or(LineKind::Other)
}

fn for_each_temp_in_expr<'a>(expr: &'a str, mut f: impl FnMut(&'a str)) {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident_byte(bytes[i]) {
            i += 1;
        }
        if start > 0 && bytes[start - 1] == b'.' {
            continue;
        }
        let token = &expr[start..i];
        if is_block_local_temp(token) {
            f(token);
        }
    }
}

fn scan_body<'a>(body: &'a str, lines: &[Line<'a>]) -> BodyScan<'a> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ScanMode {
        Normal,
        LineComment,
        BlockComment,
        Quote(u8),
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum DeclState {
        Start,
        ExpectName,
        SkipInit,
        IgnoreStmt,
    }

    let bytes = body.as_bytes();
    let n = lines.len();
    let mut line_scope = vec![usize::MAX; n];
    let mut line_shadows = vec![false; n];
    let mut line_has_block_or_quote = vec![false; n];
    let mut scope_stack = vec![0usize];
    let mut scope_close = vec![body.len()];
    let mut scope_decls: Vec<HashSet<&'a str>> = vec![HashSet::new()];
    let mut temp_uses: HashMap<&'a str, Vec<usize>> = HashMap::new();

    let mut mode = ScanMode::Normal;
    let mut decl_state = DeclState::Start;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut line_idx = 0usize;
    let mut p = 0usize;

    let advance_line = |pos: usize, line_idx: &mut usize, line_scope: &mut [usize], top: usize| {
        while *line_idx < n && pos >= lines[*line_idx].end {
            *line_idx += 1;
        }
        if *line_idx < n && line_scope[*line_idx] == usize::MAX {
            line_scope[*line_idx] = top;
        }
    };

    if n > 0 {
        line_scope[0] = 0;
    }

    while p < bytes.len() {
        advance_line(p, &mut line_idx, &mut line_scope, *scope_stack.last().unwrap());
        match mode {
            ScanMode::LineComment => {
                if bytes[p] == b'\n' {
                    mode = ScanMode::Normal;
                }
                p += 1;
            }
            ScanMode::BlockComment => {
                if line_idx < n {
                    line_has_block_or_quote[line_idx] = true;
                }
                if bytes.get(p..p + 2) == Some(b"*/") {
                    mode = ScanMode::Normal;
                    p += 2;
                } else {
                    p += 1;
                }
            }
            ScanMode::Quote(q) => {
                if line_idx < n {
                    line_has_block_or_quote[line_idx] = true;
                }
                if bytes[p] == b'\\' && p + 1 < bytes.len() {
                    p += 2;
                } else {
                    if bytes[p] == q {
                        mode = ScanMode::Normal;
                    }
                    p += 1;
                }
            }
            ScanMode::Normal => {
                if bytes.get(p..p + 2) == Some(b"//") {
                    mode = ScanMode::LineComment;
                    p += 2;
                    continue;
                }
                if bytes.get(p..p + 2) == Some(b"/*") {
                    if line_idx < n {
                        line_has_block_or_quote[line_idx] = true;
                    }
                    mode = ScanMode::BlockComment;
                    p += 2;
                    continue;
                }
                if bytes[p] == b'"' || bytes[p] == b'\'' {
                    if line_idx < n {
                        line_has_block_or_quote[line_idx] = true;
                    }
                    mode = ScanMode::Quote(bytes[p]);
                    p += 1;
                    continue;
                }
                if bytes[p] == b'#' {
                    mode = ScanMode::LineComment;
                    decl_state = DeclState::Start;
                    p += 1;
                    continue;
                }
                if is_ident_byte(bytes[p]) {
                    let start = p;
                    while p < bytes.len() && is_ident_byte(bytes[p]) {
                        p += 1;
                    }
                    let token = &body[start..p];
                    let preceded_by_dot = start > 0 && bytes[start - 1] == b'.';
                    if !preceded_by_dot {
                        if is_block_local_temp(token) {
                            temp_uses.entry(token).or_default().push(start);
                        }
                        if paren_depth == 0 && bracket_depth == 0 {
                            match decl_state {
                                DeclState::Start => {
                                    if matches!(token, "const" | "static" | "groupshared" | "precise") {
                                        // Wait for the element type token.
                                    } else if is_any_decl_head(token) {
                                        decl_state = DeclState::ExpectName;
                                    } else {
                                        decl_state = DeclState::IgnoreStmt;
                                    }
                                }
                                DeclState::ExpectName => {
                                    if !bytes[start].is_ascii_digit() {
                                        let shadows = scope_decls
                                            .iter()
                                            .any(|frame| frame.contains(token));
                                        if shadows && line_idx < n {
                                            line_shadows[line_idx] = true;
                                        }
                                        scope_decls.last_mut().unwrap().insert(token);
                                        decl_state = DeclState::SkipInit;
                                    }
                                }
                                DeclState::SkipInit | DeclState::IgnoreStmt => {}
                            }
                        }
                    }
                    continue;
                }
                match bytes[p] {
                    b'(' => paren_depth += 1,
                    b')' => paren_depth = paren_depth.saturating_sub(1),
                    b'[' => bracket_depth += 1,
                    b']' => bracket_depth = bracket_depth.saturating_sub(1),
                    b',' if paren_depth == 0 && bracket_depth == 0 => {
                        if decl_state == DeclState::SkipInit {
                            decl_state = DeclState::ExpectName;
                        }
                    }
                    b';' if paren_depth == 0 && bracket_depth == 0 => {
                        decl_state = DeclState::Start;
                    }
                    b'{' => {
                        let id = scope_close.len();
                        scope_close.push(body.len());
                        scope_stack.push(id);
                        scope_decls.push(HashSet::new());
                        decl_state = DeclState::Start;
                        paren_depth = 0;
                        bracket_depth = 0;
                    }
                    b'}' => {
                        if scope_stack.len() > 1 {
                            let id = scope_stack.pop().unwrap();
                            scope_close[id] = p;
                            scope_decls.pop();
                        }
                        decl_state = DeclState::Start;
                        paren_depth = 0;
                        bracket_depth = 0;
                    }
                    _ => {}
                }
                p += 1;
            }
        }
    }
    for slot in &mut line_scope {
        if *slot == usize::MAX {
            *slot = 0;
        }
    }
    BodyScan {
        line_scope,
        scope_close,
        line_shadows,
        line_has_block_or_quote,
        temp_uses,
    }
}

fn max_use_before(uses: &HashMap<&str, Vec<usize>>, name: &str, scope_end: usize) -> Option<usize> {
    let list = uses.get(name)?;
    let count = list.partition_point(|&pos| pos < scope_end);
    count.checked_sub(1).map(|idx| list[idx])
}

fn prev_guarded(lines: &[Line<'_>], start: usize, before: usize) -> Option<usize> {
    (start..before)
        .rev()
        .find(|&idx| matches!(lines[idx].kind, LineKind::Guarded { .. }))
}

/// Write each guarded register value as a `select`, which needs no branch.
///
/// Mesh programs run their lanes through one function, where DXC spends about
/// half its time on the blocks of these guards. Only the pure register writes that
/// `parse_guarded` accepts change; everything else keeps its branch.
pub(super) fn select_writes(body: &str) -> String {
    let mut output = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        let raw = line.trim_end_matches(['\n', '\r']);
        let written = match parse_guarded(raw) {
            Some(LineKind::Guarded { indent, guard, stmt, expr }) => stmt.split_once(" = ").map(|(dest, _)| {
                format!("{indent}{dest} = select(({}) != 0u, {expr}, {dest});", guard.expr())
            }),
            _ => None,
        };
        match written {
            Some(written) => {
                output.push_str(&written);
                output.push_str(&line[raw.len()..]);
            }
            None => output.push_str(line),
        }
    }
    output
}

pub(super) fn group(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut offset = 0usize;
    for chunk in body.split_inclusive('\n') {
        let raw = chunk
            .strip_suffix('\n')
            .unwrap_or(chunk)
            .strip_suffix('\r')
            .unwrap_or_else(|| chunk.strip_suffix('\n').unwrap_or(chunk));
        let start = offset;
        offset += chunk.len();
        lines.push(Line {
            raw,
            start,
            end: offset,
            kind: classify_line(raw),
        });
    }
    let scan = scan_body(body, &lines);
    for (line, &non_normal) in lines.iter_mut().zip(&scan.line_has_block_or_quote) {
        if non_normal {
            line.kind = LineKind::Other;
        }
    }
    let n = lines.len();
    let mut output = String::with_capacity(body.len());
    let mut i = 0usize;

    while i < n {
        match lines[i].kind {
            LineKind::Comment | LineKind::Other => {
                writeln!(output, "{}", lines[i].raw).unwrap();
                i += 1;
            }
            LineKind::Guarded { .. } | LineKind::TempDecl { .. } => {
                // Find the first guarded statement to establish the group guard and indent.
                let base_scope = scan.line_scope[i];
                let mut probe = i;
                let mut probe_indent = None;
                let mut group_guard = None;
                let mut used_before_decl: HashSet<&str> = HashSet::new();
                let mut declared_in_span: HashSet<&str> = HashSet::new();
                let mut failed_lead = false;

                while probe < n {
                    match &lines[probe].kind {
                        LineKind::Comment => probe += 1,
                        LineKind::TempDecl { indent, decls } => {
                            if scan.line_scope[probe] != base_scope
                                || scan.line_shadows[probe]
                                || probe_indent.is_some_and(|p| p != *indent)
                            {
                                failed_lead = true;
                                break;
                            }
                            probe_indent = Some(*indent);
                            for &(name, rhs) in decls {
                                for_each_temp_in_expr(rhs, |tok| {
                                    used_before_decl.insert(tok);
                                });
                                if used_before_decl.contains(name)
                                    || !declared_in_span.insert(name)
                                {
                                    failed_lead = true;
                                    break;
                                }
                            }
                            if failed_lead {
                                break;
                            }
                            probe += 1;
                        }
                        LineKind::Guarded { indent, guard, .. } => {
                            if scan.line_scope[probe] == base_scope
                                && probe_indent.is_none_or(|p| p == *indent)
                            {
                                probe_indent = Some(*indent);
                                group_guard = Some(*guard);
                            }
                            break;
                        }
                        LineKind::Other => break,
                    }
                }

                let (Some(group_indent), Some(guard)) = (probe_indent, group_guard) else {
                    writeln!(output, "{}", lines[i].raw).unwrap();
                    i += 1;
                    continue;
                };
                if failed_lead {
                    writeln!(output, "{}", lines[i].raw).unwrap();
                    i += 1;
                    continue;
                }

                // Extend candidate span forward until a barrier, incompatible guard, or
                // unabsorbable declaration.
                let mut k = probe;
                let mut last_guarded = None;
                while k < n {
                    if scan.line_scope[k] != base_scope {
                        break;
                    }
                    match &lines[k].kind {
                        LineKind::Comment => k += 1,
                        LineKind::Guarded {
                            indent,
                            guard: g,
                            expr,
                            ..
                        } if *indent == group_indent && *g == guard => {
                            for_each_temp_in_expr(expr, |tok| {
                                used_before_decl.insert(tok);
                            });
                            last_guarded = Some(k);
                            k += 1;
                        }
                        LineKind::TempDecl { indent, decls }
                            if *indent == group_indent && !scan.line_shadows[k] =>
                        {
                            let mut conflict = false;
                            for &(name, rhs) in decls {
                                for_each_temp_in_expr(rhs, |tok| {
                                    used_before_decl.insert(tok);
                                });
                                if used_before_decl.contains(name)
                                    || !declared_in_span.insert(name)
                                {
                                    conflict = true;
                                    break;
                                }
                            }
                            if conflict {
                                break;
                            }
                            k += 1;
                        }
                        _ => break,
                    }
                }

                let Some(mut end) = last_guarded else {
                    writeln!(output, "{}", lines[i].raw).unwrap();
                    i += 1;
                    continue;
                };

                // Shrink `end` backwards if any `TempDecl` in `i..=end` is referenced
                // between `lines[end].end` and the end of its enclosing HLSL scope.
                let mut live_heap: BinaryHeap<(usize, usize)> = BinaryHeap::new();
                for idx in i..=end {
                    if let LineKind::TempDecl { decls, .. } = &lines[idx].kind {
                        let scope_end = scan.scope_close[scan.line_scope[idx]];
                        let max_pos = decls
                            .iter()
                            .filter_map(|&(name, _)| max_use_before(&scan.temp_uses, name, scope_end))
                            .max()
                            .unwrap_or(lines[idx].start);
                        live_heap.push((max_pos, idx));
                    }
                }

                let mut valid = true;
                while let Some(&(max_pos, idx)) = live_heap.peek() {
                    if idx > end {
                        live_heap.pop();
                        continue;
                    }
                    if max_pos < lines[end].end {
                        break;
                    }
                    match prev_guarded(&lines, i, idx) {
                        Some(new_end) => {
                            end = new_end;
                            live_heap.pop();
                        }
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }

                if !valid {
                    writeln!(output, "{}", lines[i].raw).unwrap();
                    i += 1;
                    continue;
                }

                writeln!(output, "{group_indent}if ({}) {{", guard.expr()).unwrap();
                for line in &lines[i..=end] {
                    match &line.kind {
                        LineKind::Guarded { stmt, .. } => {
                            writeln!(output, "{group_indent}    {stmt}").unwrap();
                        }
                        LineKind::TempDecl { .. } => {
                            let body = line.raw.trim_start_matches(' ');
                            writeln!(output, "{group_indent}    {body}").unwrap();
                        }
                        LineKind::Comment => {
                            writeln!(output, "{}", line.raw).unwrap();
                        }
                        LineKind::Other => unreachable!(),
                    }
                }
                writeln!(output, "{group_indent}}}").unwrap();
                i = end + 1;
            }
        }
    }

    output
}

#[cfg(test)]
#[path = "../tests/hlsl/exec_group.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/hlsl/exec_group_wave32.rs"]
mod wave32_intrinsic_tests;
