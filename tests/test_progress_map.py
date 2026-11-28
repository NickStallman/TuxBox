import json
from pathlib import Path, PureWindowsPath
from tempfile import TemporaryDirectory
import unittest
from xml.etree import ElementTree as ET

from project_tuxbox.progress_map import culling_readbacks, library_table, render_map, shader_checks, write_map


class ProgressMapTests(unittest.TestCase):
    def test_active_libraries_match_title_imports_and_observed_loads(self):
        evidence = Path(__file__).resolve().parents[1] / "evidence"
        inventory = json.loads((evidence / "frosty-xbox-library-inventory.json").read_text())
        report = json.loads((evidence / "matrix-awakens/independent-menu.json").read_text())
        status = json.loads((evidence / "library-status.json").read_text())
        self.assertEqual(inventory["title"]["sha256"], report["executable_sha256"])
        direct = {entry["module"].casefold() for entry in inventory["title"]["imports"]}
        loaded = report["loader"]["modules"]
        filenames = {PureWindowsPath(path).name.casefold() for path in loaded}
        binary_hashes = {entry["sha256"] for entry in loaded.values()}
        active = {dll["name"].casefold() for dll in inventory["libraries"]
                  if dll["name"].casefold() in direct | filenames or dll["sha256"] in binary_hashes}
        tracked = {dll["name"].casefold() for dll in status["libraries"]}
        external = {dll["name"].casefold() for dll in status.get("external_libraries", [])}
        self.assertFalse(tracked & external)
        self.assertEqual(active, tracked | external)

    def test_compute_survey_requires_matching_archive_and_valid_counts(self):
        inventory = json.loads((Path(__file__).resolve().parents[1] /
                                "evidence/matrix-awakens/shader-inventory.json").read_text())
        archive = inventory["archives"]["global"]["member_sha256"]
        with TemporaryDirectory() as directory:
            root = Path(directory)
            dest = root / "matrix-awakens"
            dest.mkdir()
            path = dest / "compute-archive-survey.json"
            receipt = {"archive_sha256": archive, "compiler_sha256": "12345678",
                       "stage": {"entries": 2508, "translated": 2254}}
            path.write_text(json.dumps(receipt))
            self.assertTrue(any("2,254/2,508" in note for note in shader_checks(root, inventory)))
            receipt["archive_sha256"] = "different"
            path.write_text(json.dumps(receipt))
            self.assertFalse(any("Global compute archive" in note for note in shader_checks(root, inventory)))
            receipt["archive_sha256"] = archive
            receipt["stage"]["translated"] = 2509
            path.write_text(json.dumps(receipt))
            self.assertFalse(any("Global compute archive" in note for note in shader_checks(root, inventory)))

    def test_culling_readbacks_require_matching_source_and_current_gpu_bytecode(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            dest = root / "matrix-awakens"
            dest.mkdir()
            retail = dict(global_archive_sha256="archive", compiler_sha256="old", d3d12_xs_sha256="provider",
                          entry=5, source_sha256="source", dxil_sha256="bytecode", family={"archive_entries": [5, 6]})
            (dest / "instance-cull-retail.json").write_text(json.dumps(retail))
            program = dict(entry=5, source_sha256="source", dxil_sha256="bytecode",
                           cases={"sparse": {"checked_words": 48, "checked_texels": 16}})
            run = dict(name="InstanceCull", compiler_sha256="current", global_archive_sha256="archive",
                       provider_sha256="provider", programs=[program])
            for suffix, adapter in (("amd", "1002:13c0"), ("nvidia", "10de:2705")):
                (dest / f"instance-cull-{suffix}.json").write_text(json.dumps(dict(run, adapter=adapter)))
            self.assertIn("1/2", culling_readbacks(root, "archive", "current", "InstanceCull"))
            self.assertFalse(culling_readbacks(root, "archive", "old", "InstanceCull"))
            self.assertFalse(culling_readbacks(root, "other", "current", "InstanceCull"))
            for changed in (dict(dxil_sha256="different"), dict(cases={})):
                (dest / "instance-cull-amd.json").write_text(json.dumps(
                    dict(run, adapter="1002:13c0", programs=[dict(program, **changed)])))
                self.assertFalse(culling_readbacks(root, "archive", "current", "InstanceCull"))
            # A corrected build may emit new DXIL for the same Xbox program.
            updated = dict(program, dxil_sha256="corrected")
            for suffix, adapter in (("amd", "1002:13c0"), ("nvidia", "10de:2705")):
                (dest / f"instance-cull-{suffix}.json").write_text(json.dumps(
                    dict(run, adapter=adapter, provider_sha256="new provider", programs=[updated])))
            self.assertIn("1/2", culling_readbacks(root, "archive", "current", "InstanceCull"))
            (dest / "instance-cull-nvidia.json").write_text(json.dumps(
                dict(run, adapter="10de:2705", programs=[updated])))
            self.assertFalse(culling_readbacks(root, "archive", "current", "InstanceCull"))

    def test_workspace_map_matches_saved_evidence(self):
        root = Path(__file__).resolve().parents[1]
        self.assertEqual((root / "docs/progress.svg").read_text(encoding="utf-8"),
                         render_map(root / "evidence"),
                         "Regenerate the map with python tuxbox.py status --svg docs/progress.svg")

    def write_instruction_evidence(self, root):
        dest = root / "shader-instructions"
        dest.mkdir(parents=True)
        (dest / "dxil.json").write_text(json.dumps({"compiled": ["Sop2 0x0 s_add_u32"], "failed": {}}))
        (dest / "gpu-checks.json").write_text(json.dumps({"adapters": {
            "AMD 1002:0001": {"instructions": ["v_add_f32", "v_sub_f32"]},
            "NVIDIA 10de:0001": {"instructions": ["v_add_f32"]}}}))
        (dest / "recorded-use.json").write_text(json.dumps({"title": "Example 1.0", "instructions": {
            "s_add_u32": 5, "v_add_f32": 3, "v_sub_f32": 2, "s_nop": 1}}))
        return dest

    def test_instruction_panel_counts_compiled_and_checked_instructions(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertNotIn(">RDNA 2 instructions<", render_map(root))
            self.write_instruction_evidence(root)
            graphic = render_map(root)
            # s_add_u32 compiled alone; v_add_f32 and v_sub_f32 ran in a GPU check.
            self.assertIn(">3/1,085<", graphic)
            self.assertIn("Used in recorded shaders", graphic)
            self.assertNotIn("Example 1.0", graphic)
            # Of the four used, three are converted, and one ran on both vendors.
            self.assertIn(">3/4<", graphic)
            self.assertIn(">1/4<", graphic)

    def write_library_audit(self, root):
        dest = root / "matrix-awakens"
        dest.mkdir()
        (root / "matrix-awakens-launch.json").write_text(json.dumps({"sha256": "game"}))
        (dest / "run.json").write_text("{}")
        ref = f"{root.name}/matrix-awakens/run.json"
        paths = [
            {"name": "Decode frames", "status": "verified", "libraries": ["codec.dll"],
             "provider": "Game package", "scope": "Decoded frames reached the game.", "evidence": [ref]},
            {"name": "Play audio", "status": "partial", "libraries": ["audio.dll", "output.dll"],
             "provider": "Proton", "scope": "Only menu audio checked.", "evidence": [ref]},
        ]
        audit = {"title_sha256": "game", "paths": paths,
                 "optional": [{"name": "Chat", "reason": "No gameplay use observed.", "evidence": [ref]}]}
        path = dest / "library-paths.json"
        path.write_text(json.dumps(audit))
        return path, audit

    def test_library_table_shows_working_provider_and_open_behavior(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_library_audit(root)
            table = library_table(root)
            self.assertIn("1/2 required library behaviors verified", table)
            self.assertIn("Verified · Decode frames · Game package", table)
            self.assertIn("audio.dll, output.dll", table)
            self.assertIn("Only menu audio checked.", table)
            self.assertIn("Chat · No gameplay use observed.", table)
            output = root / "output/map.svg"
            write_map(root, output)
            labels = [node.text for node in ET.parse(output).findall(".//{*}text")]
            self.assertIn("Verified", labels)
            self.assertIn("Partial", labels)
            self.assertNotIn("Chat", labels)

    def test_library_audit_checks_title_evidence_and_unique_behaviors(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            path, audit = self.write_library_audit(root)
            path.write_text(json.dumps(dict(audit, title_sha256="other")))
            self.assertIn("No matching", library_table(root))
            self.assertIn("Library behaviors · Unmeasured", render_map(root))
            path.write_text(json.dumps(dict(audit, paths=audit["paths"]*2)))
            with self.assertRaisesRegex(ValueError, "duplicate"):
                library_table(root)
            path.write_text(json.dumps(audit))
            (path.parent / "run.json").unlink()
            with self.assertRaisesRegex(ValueError, "Missing evidence"):
                render_map(root)

    def test_overall_weights_library_behaviors_and_instructions_equally(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_library_audit(root)
            # Inventorying extra exports does not create a new required behavior.
            (root / "library-status.json").write_text(json.dumps({"libraries": [
                {"name": "codec.dll", "export_coverage": {"total": 1000, "verified": []}}]}))
            dest = self.write_instruction_evidence(root)
            graphic = render_map(root)
            # (1/2 + 3/1,085) / 2
            self.assertIn(">25.1%<", graphic)
            self.assertIn("Library behaviors · 1/2 verified (50.0%)", graphic)
            self.assertIn("RDNA 2 instructions · 3/1,085 (0.3%)", graphic)
            (dest / "dxil.json").unlink()
            self.assertIn("RDNA 2 instructions · Unmeasured", render_map(root))
            self.assertNotIn(">25.1%<", render_map(root))

    def test_the_map_names_no_game(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_library_audit(root)
            self.write_instruction_evidence(root)
            (root / "matrix-awakens-launch.json").write_text(json.dumps({"sha256": "game", "progress_receipt": "latest.json"}))
            (root / "latest.json").write_text(json.dumps({"title_sha256": "game", "runs": {"tuxbox": {
                "log_sha256": "tuxbox", "verified_milestones": ["load_world"]}}}))
            graphic = render_map(root)
            for name in ("Frosty", "Example 1.0", "retail", "Decoded frames reached the game."):
                self.assertNotIn(name, graphic)
