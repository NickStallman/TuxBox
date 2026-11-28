"""Render docs/INSTRUCTIONS.md: every RDNA 2 instruction and how far TuxBox supports it.

Three inputs, all in the repository:
  - the translator's record of each instruction translated alone
    (tools/shader-compiler/src/tests/hlsl/instruction_inventory.txt, kept current by a Rust test);
  - evidence/shader-instructions/gpu-checks.json: instructions the GPU checks ran, by card;
  - evidence/shader-instructions/recorded-use.json: instructions in a title's translated shaders.
"""
from collections import Counter
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RECORD = ROOT / "tools/shader-compiler/src/tests/hlsl/instruction_inventory.txt"
EVIDENCE = ROOT / "evidence/shader-instructions"
DOCUMENT = ROOT / "docs/INSTRUCTIONS.md"

FAMILIES = {
    "Sop2": "Scalar, two operands (SOP2)", "Sopk": "Scalar with a constant (SOPK)",
    "Sop1": "Scalar, one operand (SOP1)", "Sopc": "Scalar compare (SOPC)",
    "Sopp": "Program control (SOPP)", "Smem": "Scalar memory (SMEM)",
    "Vop2": "Vector, two operands (VOP2)", "Vop1": "Vector, one operand (VOP1)",
    "Vopc": "Vector compare (VOPC)", "Vop3": "Vector, three operands (VOP3 and VOP3P)",
    "Vinterp": "Interpolation (VINTRP)", "Ds": "Group memory (DS)", "Flat": "Flat, global and scratch memory (FLAT)",
    "Mubuf": "Untyped buffers (MUBUF)", "Mtbuf": "Typed buffers (MTBUF)", "Mimg": "Images (MIMG)",
    "Exp": "Export (EXP)",
}
# Order of the summary columns.
STATES = ("yes", "context", "none", "unknown", "no")
HEADINGS = {"yes": "Translates", "context": "Seen translating", "none": "Nothing to emit",
            "unknown": "Unproven", "no": "Not translated"}


def read_record(text):
    rows = []
    for line in text.splitlines():
        head, _, outcome = line.partition(" | ")
        family, opcode, name = head.split()
        rows.append({"family": family, "opcode": opcode, "name": name, "outcome": outcome})
    return rows


def state(row, used):
    """How far the translator is known to handle one instruction."""
    outcome = row["outcome"]
    if outcome.startswith("yes"):
        return "yes"
    if outcome.startswith("nothing"):
        return "none"
    # Alone it lacked a resource or a stage, but real programs with it translate.
    if used:
        return "context"
    return "no" if outcome.startswith("unsupported") else "unknown"


def describe(row, kind):
    outcome = row["outcome"]
    only = outcome.partition(" (")[2].removesuffix(" only)")
    if kind == "yes":
        return f"Yes, {only.lower()} stage" if only else "Yes"
    if kind == "none":
        return "Nothing to emit"
    if kind == "context":
        return "Yes, in recorded shaders"
    if kind == "no":
        return "**No**"
    return f"Unproven: {outcome}"


def cards(checks):
    return {name: set(card["instructions"]) for name, card in sorted(checks["adapters"].items())}


def render(record_text, checks, use):
    rows = read_record(record_text)
    counts = use["instructions"]
    tested = cards(checks)
    for row in rows:
        row["use"] = counts.get(row["name"], 0)
        row["state"] = state(row, row["use"])
        row["cards"] = [name for name, found in tested.items() if row["name"] in found]
    names = {row["name"] for row in rows}
    used = {row["name"] for row in rows if row["use"]}
    checked = {row["name"] for row in rows if row["cards"]}
    out = ["# RDNA 2 instructions", "",
           "Every instruction the decoder names, from [AMD's machine-readable instruction set]"
           "(https://gpuopen.com/machine-readable-isa/) for RDNA 1 and RDNA 2, and how far TuxBox supports it. "
           "This file is generated: see [how to update it](SHADER_HARNESS.md#the-instruction-inventory). "
           "Don't edit it by hand.", "",
           "| Column | Meaning |", "| --- | --- |",
           "| Translator | The result of translating the instruction alone, with plain register operands. "
           "**Translates** means some statement was emitted, not that the result is correct |",
           "| GPU check | Cards on which a [translator GPU check](../tools/check_translator_gpu.py) ran a program "
           "holding the instruction. It doesn't show that every operand form or lane state was tested |",
           f"| Recorded shaders | Translated shaders of {use['title']} that hold it, of {use['translations']:,} |", "",
           "## Summary", "",
           f"- {len(names):,} instructions in {len(rows):,} encodings.",
           f"- {len(used)} are used by the recorded shaders; a GPU check runs {len(used & checked)} of those.",
           f"- {sum(row['state'] == 'no' for row in rows)} encodings have no translation, and "
           f"{sum(row['state'] == 'unknown' for row in rows)} are unproven: alone they need a bound resource "
           "or a stage, and no recorded shader uses them.", "",
           "| Family | Encodings | " + " | ".join(HEADINGS[s] for s in STATES) + " | Used | Used without a GPU check |",
           "| --- | ---: | " + "---: | " * len(STATES) + "---: | ---: |"]
    for family, title in FAMILIES.items():
        group = [row for row in rows if row["family"] == family]
        tally = Counter(row["state"] for row in group)
        cells = [f"[{title}](#{anchor(title)})", len(group), *(tally[s] for s in STATES),
                 sum(bool(row["use"]) for row in group), sum(bool(row["use"]) and not row["cards"] for row in group)]
        out.append("| " + " | ".join(map(str, cells)) + " |")
    gaps = sorted({(row["name"], row["use"]) for row in rows if row["use"] and not row["cards"]},
                  key=lambda item: (-item[1], item[0]))
    out += ["", "## Used without a GPU check", "", "Most used first. Each needs a check before it can be trusted.", "",
            "| Instruction | Recorded shaders |", "| --- | ---: |"]
    out += [f"| `{name}` | {count:,} |" for name, count in gaps]
    for family, title in FAMILIES.items():
        out += ["", f"## {title}", "", "| Instruction | Opcode | Translator | GPU check | Recorded shaders |",
                "| --- | --- | --- | --- | ---: |"]
        for row in rows:
            if row["family"] == family:
                out.append(f"| `{row['name']}` | {row['opcode']} | {describe(row, row['state'])} | "
                           f"{', '.join(row['cards']) or '—'} | {row['use'] and format(row['use'], ',') or '—'} |")
    return "\n".join(out) + "\n"


def anchor(title):
    kept = "".join(c for c in title.lower() if c.isalnum() or c in " -")
    return kept.replace(" ", "-")


def current():
    return render(RECORD.read_text(), json.loads((EVIDENCE / "gpu-checks.json").read_text()),
                  json.loads((EVIDENCE / "recorded-use.json").read_text()))
