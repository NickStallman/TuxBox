from pathlib import Path
import os
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

from project_tuxbox.console_cbv import configure_console_cbv, console_cbv_layer
from project_tuxbox.host import PINNED_PROTON, SHADER_CACHE, Host, claim_package_drive, configure_mesa_shader_cache, configure_nvidia_shader_cache, configure_pipeline_cache, limit_malloc_growth, proton_installation, report_unindexed_nvidia_cache, requires_offline, seed_shader_cache, warn_scarlett_dxc


class ScarlettWindowsDxc(unittest.TestCase):
    def test_warns_only_for_scarlett_and_does_not_modify_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            report = Mock()
            warn_scarlett_dxc(root / "other.exe", report)
            report.assert_not_called()
            exe = root / "game_scarlett_master.exe"
            warn_scarlett_dxc(exe, report)
            self.assertIn("dxcompiler.dll missing", report.call_args.args[0])
            self.assertIn("dxil.dll missing", report.call_args.args[0])
            self.assertEqual(list(root.iterdir()), [])
            (root / "dxcompiler.dll").write_bytes(b"not PE")
            report.reset_mock()
            warn_scarlett_dxc(exe, report)
            self.assertIn("unreadable or not a valid PE", report.call_args.args[0])
            self.assertEqual((root / "dxcompiler.dll").read_bytes(), b"not PE")

    def test_requires_x64_dll_headers(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("dxcompiler.dll", "dxil.dll"):
                (root / name).touch()
            report = Mock()
            pe = Mock()
            pe.FILE_HEADER = SimpleNamespace(Machine=0x8664, Characteristics=0x2000)
            pe.OPTIONAL_HEADER = SimpleNamespace(Magic=0x20b)
            with patch("pefile.PE", return_value=pe):
                warn_scarlett_dxc(root / "game_scarlett_master.exe", report)
                report.assert_not_called()
                self.assertEqual(pe.close.call_count, 2)
                pe.FILE_HEADER.Machine = 0x14c
                warn_scarlett_dxc(root / "game_scarlett_master.exe", report)
                self.assertIn("not an x64 Windows DLL", report.call_args.args[0])


class SelectProton(unittest.TestCase):
    def test_explicit_runner_in_path_with_spaces(self):
        with tempfile.TemporaryDirectory(prefix="provider with spaces ") as tmp:
            install = Path(tmp).resolve()
            runner = install / "proton"
            runner.touch()
            self.assertEqual(proton_installation(str(runner)), install)
            self.assertEqual(proton_installation(str(install)), install)

    @unittest.skipUnless(sys.platform == "linux", "Proton runner symlinks require Linux")
    def test_explicit_runner_symlink_to_renamed_script(self):
        with tempfile.TemporaryDirectory() as tmp:
            install = Path(tmp).resolve()
            (install / "proton-launcher").touch()
            runner = install / "proton"
            runner.symlink_to("proton-launcher")
            self.assertEqual(proton_installation(str(runner)), install)

    def test_default_is_the_pinned_release(self):
        for relative in (f"compatibilitytools.d/{PINNED_PROTON}-x86_64",
                         f"Steam Library/compatibilitytools.d/{PINNED_PROTON}",
                         f"Steam Library/steamapps/common/{PINNED_PROTON}"):
            with self.subTest(relative), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                (root / "steamapps").mkdir()
                library = root / "Steam Library"
                (root / "steamapps/libraryfolders.vdf").write_text(
                    f'"libraryfolders" {{ "0" {{ "path" "{library}" }} }}')
                install = root / relative
                install.mkdir(parents=True)
                (install / "proton").touch()
                with patch.dict(os.environ, {"STEAM_COMPAT_CLIENT_INSTALL_PATH": str(root)}), \
                        patch.object(Path, "home", return_value=root):
                    self.assertEqual(proton_installation(None), install)

    def test_another_installed_proton_is_not_a_default(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            other = root / "steamapps/common/Proton - Experimental"
            other.mkdir(parents=True)
            (other / "proton").touch()
            with patch.dict(os.environ, {"STEAM_COMPAT_CLIENT_INSTALL_PATH": str(root)}), \
                    patch.object(Path, "home", return_value=root):
                with self.assertRaisesRegex(ValueError, PINNED_PROTON):
                    proton_installation(None)

    def test_default_finds_flatpak_steam_tools(self):
        for suffix in ("", "-x86_64"):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as tmp:
                home = Path(tmp).resolve()
                steam = home / ".var/app/com.valvesoftware.Steam/.local/share/Steam"
                install = steam / "compatibilitytools.d" / f"{PINNED_PROTON}{suffix}"
                install.mkdir(parents=True)
                (install / "proton").touch()
                with patch.dict(os.environ, {}, clear=True), \
                        patch.object(Path, "home", return_value=home):
                    self.assertEqual(proton_installation(None), install)

    def test_default_reads_library_folders_from_each_steam_installation(self):
        for relative in (".local/share/Steam",
                         ".var/app/com.valvesoftware.Steam/.local/share/Steam"):
            with self.subTest(steam=relative), tempfile.TemporaryDirectory() as tmp:
                home = Path(tmp).resolve()
                steam = home / relative
                (steam / "steamapps").mkdir(parents=True)
                library = home / "Other Steam Library"
                (steam / "steamapps/libraryfolders.vdf").write_text(
                    f'"libraryfolders" {{ "0" {{ "path" "{library.as_posix()}" }} }}')
                install = library / "steamapps/common" / PINNED_PROTON
                install.mkdir(parents=True)
                (install / "proton").touch()
                with patch.dict(os.environ, {}, clear=True), \
                        patch.object(Path, "home", return_value=home):
                    self.assertEqual(proton_installation(None), install)

    def test_selected_steam_client_keeps_priority(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            selected = home / "Selected Steam"
            flatpak = home / ".var/app/com.valvesoftware.Steam/.local/share/Steam"
            for steam in (selected, flatpak):
                install = steam / "compatibilitytools.d" / PINNED_PROTON
                install.mkdir(parents=True)
                (install / "proton").touch()
            with patch.dict(os.environ, {"STEAM_COMPAT_CLIENT_INSTALL_PATH": str(selected)},
                            clear=True), patch.object(Path, "home", return_value=home):
                self.assertEqual(proton_installation(None),
                                 selected / "compatibilitytools.d" / PINNED_PROTON)

    @unittest.skipUnless(sys.platform == "linux", "Wine prefix drive links and POSIX modes require Linux")
    def test_linux_title_drive_uses_prefix_and_preserves_existing_assignment(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp)
            drive = prefix / "dosdevices/t:"
            drive.parent.mkdir()
            host = Host.__new__(Host)
            host.backend, host.command = "proton", ["umu-run"]
            host.env = {"WINEPREFIX": tmp, "TUXBOX_RUNTIME_SHADERS": "0"}
            log = Mock()

            def configured_target():
                host.run_game(prefix / "game.exe", ["-nullrhi"], log)
                return log.run.call_args.kwargs["env"].get("TUXBOX_WINE_TITLE_TEMP")

            scratch = prefix / "drive_t"
            self.assertEqual(configured_target(), str(scratch))
            self.assertEqual(scratch.stat().st_mode & 0o777, 0o700)
            drive.symlink_to("../custom")
            self.assertIsNone(configured_target())
            (prefix / "custom").mkdir()
            self.assertEqual(configured_target(), str(prefix / "custom"))
            host.backend = "native"
            self.assertIsNone(configured_target())

    @unittest.skipUnless(sys.platform == "linux", "Wine prefix drive links require Linux")
    def test_linux_package_drive_is_restored_after_the_title_exits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            prefix, package, other = root / "prefix", root / "game", root / "user-g"
            dosdevices = prefix / "dosdevices"
            for folder in (package, other, dosdevices):
                folder.mkdir(parents=True)
            host = Host.__new__(Host)
            host.backend, host.command = "proton", ["umu-run"]
            host.env = {"WINEPREFIX": str(prefix), "TUXBOX_RUNTIME_SHADERS": "0"}
            log = Mock()
            drive = dosdevices / "g:"

            def provider_mounts_package(*args, **kwargs):
                # Stand in for the GDK provider's mount manager call.
                if "TUXBOX_WINE_PACKAGE_DRIVE" not in kwargs["env"]:
                    return
                drive.unlink(missing_ok=True)
                (dosdevices / "g::").unlink(missing_ok=True)
                drive.symlink_to(kwargs["env"]["TUXBOX_WINE_PACKAGE_DRIVE"])

            def launch(package_dir):
                log.run.side_effect = provider_mounts_package
                host.run_game(package / "game.exe", ["-nullrhi"], log, package=package_dir)
                return log.run.call_args.kwargs["env"].get("TUXBOX_WINE_PACKAGE_DRIVE")

            # A prefix without G: gets the package during the launch and no G: after it.
            self.assertEqual(launch(package), str(package))
            self.assertFalse(os.path.lexists(drive))
            self.assertIsNone(launch(None))

            # The user's own G: and its device link come back after the title exits.
            drive.symlink_to(other)
            (dosdevices / "g::").symlink_to("/dev/sdz1")
            self.assertEqual(launch(package), str(package))
            self.assertEqual(os.readlink(drive), str(other))
            self.assertEqual(os.readlink(dosdevices / "g::"), "/dev/sdz1")
            self.assertFalse((prefix / "tuxbox-package-drive.json").exists())

            # A launch killed before it could restore G: is repaired by the next one.
            (dosdevices / "g::").unlink()
            self.assertTrue(claim_package_drive(prefix, package))
            drive.unlink()
            drive.symlink_to(package)
            self.assertIsNone(launch(None))
            self.assertEqual(os.readlink(drive), str(other))
            self.assertFalse(os.path.lexists(dosdevices / "g::"))

            # A G: that is not a link is left alone and the package is not mapped.
            drive.unlink()
            drive.mkdir()
            self.assertIsNone(launch(package))
            self.assertTrue(drive.is_dir())
            host.backend = "native"
            self.assertIsNone(launch(package))

    @unittest.skipUnless(sys.platform == "linux", "Wine prefix drive links require Linux")
    def test_linux_package_drive_record_is_removed_when_setup_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp).resolve()
            drive = prefix / "dosdevices/g:"
            drive.parent.mkdir()
            drive.symlink_to("../user-g")
            host = Host.__new__(Host)
            host.backend, host.command = "proton", ["umu-run"]
            host.env = {"WINEPREFIX": str(prefix)}
            log = Mock()

            with patch("project_tuxbox.host.configure_console_cbv", side_effect=OSError("CBV setup failed")):
                with self.assertRaisesRegex(OSError, "CBV setup failed"):
                    host.run_game(prefix / "game.exe", [], log, package=prefix / "package")

            log.run.assert_not_called()
            self.assertEqual(os.readlink(drive), "../user-g")
            self.assertFalse((prefix / "tuxbox-package-drive.json").exists())


@unittest.skipUnless(sys.platform == "linux", "Network namespaces require Linux")
class SeedShaderCache(unittest.TestCase):
    def prefixes(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        return Path(tmp.name)

    def cache(self, prefix, name, stamp):
        cache = prefix / SHADER_CACHE
        (cache / "by_source").mkdir(parents=True)
        (cache / "by_source" / name).write_text(name)
        os.utime(cache, (stamp, stamp))

    def test_a_new_prefix_reuses_the_most_recent_cache(self):
        root = self.prefixes()
        self.cache(root / "Proton - Experimental", "recent.cso", 2000)
        self.cache(root / "GE-Proton11-12-x86_64", "older.cso", 1000)
        lines = []
        seed_shader_cache(root / "GE-Proton11-14-x86_64", lines.append)
        seeded = root / "GE-Proton11-14-x86_64" / SHADER_CACHE / "by_source"
        self.assertEqual([p.name for p in seeded.iterdir()], ["recent.cso"])
        self.assertEqual(lines, ["Reusing the shader cache from the Proton - Experimental prefix"])
        # The donor keeps its own cache.
        self.assertTrue((root / "Proton - Experimental" / SHADER_CACHE / "by_source/recent.cso").is_file())

    def test_an_existing_cache_and_a_lone_prefix_are_left_alone(self):
        root = self.prefixes()
        self.cache(root / "GE-Proton11-14-x86_64", "own.cso", 1000)
        self.cache(root / "Proton - Experimental", "other.cso", 2000)
        lines = []
        seed_shader_cache(root / "GE-Proton11-14-x86_64", lines.append)
        seeded = root / "GE-Proton11-14-x86_64" / SHADER_CACHE / "by_source"
        self.assertEqual([p.name for p in seeded.iterdir()], ["own.cso"])
        seed_shader_cache(root / "wine", lines.append)
        alone = self.prefixes()
        seed_shader_cache(alone / "wine", lines.append)
        self.assertFalse((alone / "wine" / SHADER_CACHE).exists())
        self.assertEqual(lines, ["Reusing the shader cache from the Proton - Experimental prefix"])

    def test_a_failed_copy_leaves_no_partial_cache(self):
        root = self.prefixes()
        self.cache(root / "Proton - Experimental", "recent.cso", 2000)
        failure = subprocess.CalledProcessError(1, ["cp"], stderr="No space left on device")
        lines = []
        with patch("project_tuxbox.host.subprocess.run", side_effect=failure):
            seed_shader_cache(root / "GE-Proton11-14-x86_64", lines.append)
        temp = root / "GE-Proton11-14-x86_64" / SHADER_CACHE.parent
        self.assertEqual(list(temp.iterdir()), [])
        self.assertTrue(lines[0].startswith("Could not reuse the shader cache from Proton - Experimental"))

    def test_only_a_new_default_prefix_is_seeded(self):
        root = self.prefixes()
        explicit = root / "chosen"
        with patch("project_tuxbox.host.shutil.which", side_effect=lambda tool: f"/usr/bin/{tool}"), \
             patch("project_tuxbox.host.subprocess.check_output", return_value="wine-10.0\n"), \
             patch("project_tuxbox.host.seed_shader_cache") as seed, \
             patch.dict(os.environ, {"XDG_DATA_HOME": str(root)}):
            Host("wine", None, explicit)
            seed.assert_not_called()
            Host("wine", None, None)
            seed.assert_called_once()
            self.assertEqual(seed.call_args.args[0], (root / "project-tuxbox/prefixes/wine").resolve())
            Host("wine", None, None)
            seed.assert_called_once()


@unittest.skipUnless(sys.platform == "linux", "Network namespaces require Linux")
class OfflineLaunch(unittest.TestCase):
    def host(self, probe_code=0, stderr=""):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        probe = subprocess.CompletedProcess([], probe_code, stderr=stderr)
        with patch("project_tuxbox.host.shutil.which", side_effect=lambda tool: f"/usr/bin/{tool}"), \
             patch("project_tuxbox.host.subprocess.check_output", return_value="wine-10.0\n"), \
             patch("project_tuxbox.host.subprocess.run", return_value=probe) as run:
            host = Host("wine", None, Path(tmp.name), offline=True)
        return host, run

    def test_wraps_every_launch_in_a_network_namespace(self):
        host, run = self.host()
        wrapper = ["/usr/bin/unshare", "--user", "--map-current-user", "--net", "--"]
        run.assert_called_once()
        self.assertEqual(run.call_args.args[0], [*wrapper, "true"])
        self.assertEqual(host.command, [*wrapper, "/usr/bin/wine"])
        self.assertEqual(host.env["UMU_RUNTIME_UPDATE"], "0")
        self.assertTrue(host.offline)

    def test_refuses_to_launch_online_when_isolation_fails(self):
        with self.assertRaisesRegex(ValueError, "Operation not permitted"):
            self.host(1, "unshare: unshare failed: Operation not permitted\n")

    def test_project_scarlett_is_recognised_in_any_case(self):
        self.assertTrue(requires_offline(Path("stage/Game_Scarlett_Master.exe")))
        self.assertFalse(requires_offline(Path("stage/Lanes.exe")))

    def test_native_backend_rejects_offline(self):
        with patch("project_tuxbox.host.platform.machine", return_value="x86_64"), \
             self.assertRaisesRegex(ValueError, "--offline"):
            Host("native", None, None, offline=True)


class ConsoleConstantBuffers(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.stage = self.root / "Sample"
        self.stage.mkdir()
        self.exe = self.stage / "game.exe"
        self.built = self.root / "build/d3d12-smoke/d3d12_xs.dll"
        self.built.parent.mkdir(parents=True)
        self.built.write_bytes(b"a built TuxBox provider")
        self.provider = self.stage / "D3D12_XS.DLL"
        self.provider.write_bytes(self.built.read_bytes())
        self.layer = str(self.root / "build/frosty-cbv-layer")
        self.log = Mock()
        for target, value in (("project_tuxbox.shader_prep.TUXBOX_PROVIDER", self.built),
                              ("project_tuxbox.shader_prep.ROOT", self.root)):
            override = patch(target, value)
            override.start()
            self.addCleanup(override.stop)

    def test_direct_stock_launch_preserves_user_settings_and_host_environment(self):
        host = Host.__new__(Host)
        host.backend, host.command = "proton", ["umu-run"]
        host.env = {"PROTONPATH": str(self.root), "VK_LAYER_PATH": "/user/layers",
                    "VK_ADD_LAYER_PATH": "/user/extra", "VK_INSTANCE_LAYERS": "overlay",
                    "VKD3D_CONFIG": "force_static_cbv", "VKD3D_DISABLE_EXTENSIONS": "first;second",
                    "PRESSURE_VESSEL_FILESYSTEMS_RO": "/user/mount", "TUXBOX_RUNTIME_SHADERS": "0"}
        layer = "/tuxbox/build/frosty-cbv-layer"
        with patch("project_tuxbox.console_cbv.console_cbv_layer", return_value=layer):
            host.run_game(self.exe, [], self.log)
            env = self.log.run.call_args.kwargs["env"]
            self.assertEqual(env["VK_LAYER_PATH"], layer + ":/user/layers")
            self.assertEqual(env["VK_ADD_LAYER_PATH"], "/user/extra")
            self.assertEqual(env["VK_INSTANCE_LAYERS"], "VK_LAYER_TUXBOX_console_cbv:overlay")
            self.assertEqual(env["VKD3D_DISABLE_EXTENSIONS"], "first;second")
            self.assertEqual(env["VKD3D_CONFIG"], "force_raw_va_cbv,force_static_cbv")
            self.assertEqual(env["PRESSURE_VESSEL_FILESYSTEMS_RO"], layer + ":/user/mount")
            before = env.copy()
            configure_console_cbv("proton", env, self.exe, self.log.status)
            self.assertEqual(env, before)
            self.assertEqual(host.env["VK_LAYER_PATH"], "/user/layers")
            host.run_game(self.exe, ["-NullRHI"], self.log)
            self.assertEqual(self.log.run.call_args.kwargs["env"]["VK_INSTANCE_LAYERS"], "overlay")

    def test_explicit_fork_feature_and_wine_with_an_older_staged_copy(self):
        sample = self.root / "build/lanes/d3d12_xs.dll"
        sample.parent.mkdir(parents=True)
        sample.write_bytes(b"older sample")
        self.provider.write_bytes(sample.read_bytes())
        (self.root / "tuxbox-features").write_text("cbv_16_byte_alignment\n")
        env = {"PROTONPATH": str(self.root),
               "VKD3D_DISABLE_EXTENSIONS": "first;VK_EXT_descriptor_buffer;VK_EXT_descriptor_heap",
               "VKD3D_CONFIG": "other"}
        with patch("project_tuxbox.console_cbv.console_cbv_layer", return_value=self.layer) as layer:
            configure_console_cbv("proton", env, self.exe, self.log.status)
            self.assertEqual(env["VKD3D_CONFIG"], "cbv_16_byte_alignment,force_raw_va_cbv,other")
            self.assertEqual(env["VKD3D_DISABLE_EXTENSIONS"], "first;VK_EXT_descriptor_buffer;VK_EXT_descriptor_heap")
            layer.assert_not_called()
            wine = {"PROTONPATH": str(self.root)}
            configure_console_cbv("wine", wine, self.exe, self.log.status)
            self.assertEqual(wine["VK_ADD_LAYER_PATH"], self.layer)
            # Without the raw address, NVIDIA rounds a root CBV down to 64 bytes.
            self.assertEqual(wine["VKD3D_CONFIG"], "force_raw_va_cbv")
            self.assertNotIn("VKD3D_DISABLE_EXTENSIONS", wine)
            fork = {"PROTONPATH": str(self.root)}
            configure_console_cbv("proton", fork, self.exe, self.log.status)
            self.assertEqual(fork["VKD3D_DISABLE_EXTENSIONS"], "VK_EXT_descriptor_buffer,VK_EXT_descriptor_heap")
            self.assertNotIn("PRESSURE_VESSEL_FILESYSTEMS_RO", wine)

    def test_native_foreign_missing_and_explicit_comparison(self):
        with patch("project_tuxbox.console_cbv.console_cbv_layer", return_value=self.layer) as layer:
            env = {}
            configure_console_cbv("native", env, self.exe, self.log.status)
            self.provider.write_bytes(b"unknown")
            configure_console_cbv("proton", env, self.exe, self.log.status)
            self.provider.unlink()
            configure_console_cbv("proton", env, self.exe, self.log.status)
            self.assertEqual(env, {})
            layer.assert_not_called()
            env["TUXBOX_REFERENCE_CBV"] = "1"
            configure_console_cbv("proton", env, self.exe, self.log.status)
            self.assertEqual(env["VK_INSTANCE_LAYERS"], "VK_LAYER_TUXBOX_console_cbv")

    def test_rebuilds_missing_or_outdated_layer(self):
        sources = self.root / "tools/reference/cbv-layer"
        sources.mkdir(parents=True)
        script = self.root / "tools/build_frosty_cbv_layer.sh"
        layer = Path(self.layer)
        layer.mkdir(parents=True)
        for path in (script, sources / "layer.c", sources / "frosty.json"):
            path.touch()
            os.utime(path, (10, 10))
        env = {"CC": "clang"}
        with patch("project_tuxbox.console_cbv.ROOT", self.root), \
             patch("project_tuxbox.console_cbv.subprocess.run") as build:
            self.assertEqual(console_cbv_layer(env), self.layer)
            build.assert_called_once_with([script, layer], env=env, check=True)
            build.reset_mock()
            for path in (layer / "frosty.json", layer / "frosty_cbv.so"):
                path.touch()
                os.utime(path, (20, 20))
            console_cbv_layer(env)
            build.assert_not_called()
            os.utime(sources / "layer.c", (30, 30))
            console_cbv_layer(env)
            build.assert_called_once_with([script, layer], env=env, check=True)


class PipelineCache(unittest.TestCase):
    def test_defaults_to_the_prefix(self):
        with tempfile.TemporaryDirectory() as tmp:
            env = {}
            configure_pipeline_cache(env, Path(tmp))
            self.assertEqual(env, {"VKD3D_SHADER_CACHE_PATH": str(Path(tmp) / "shadercache")})
            self.assertTrue((Path(tmp) / "shadercache").is_dir())

    def test_a_set_path_or_disabled_cache_stays(self):
        for value in ("/games/vkd3d", "0"):
            env = {"VKD3D_SHADER_CACHE_PATH": value}
            configure_pipeline_cache(env, Path("/unused"))
            self.assertEqual(env, {"VKD3D_SHADER_CACHE_PATH": value})


class MallocLimits(unittest.TestCase):
    def test_limits_are_added_and_a_user_value_is_kept(self):
        env = {"MALLOC_ARENA_MAX": "2"}
        limit_malloc_growth(env)
        self.assertEqual(env["MALLOC_ARENA_MAX"], "2")
        self.assertEqual(env["MALLOC_MMAP_THRESHOLD_"], "1048576")
        self.assertEqual(env["MALLOC_TRIM_THRESHOLD_"], "8388608")


class MesaShaderCache(unittest.TestCase):
    def test_prefix_gets_its_own_driver_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp) / "prefix"
            env = {"WINEPREFIX": str(prefix)}
            reports = []
            configure_mesa_shader_cache(env, reports.append)
            cache = prefix / "shadercache"
            self.assertEqual(env["MESA_SHADER_CACHE_DIR"], str(cache))
            self.assertEqual(env["MESA_SHADER_CACHE_MAX_SIZE"], "4G")
            self.assertTrue(cache.is_dir())
            self.assertEqual(reports, [f"Mesa shader cache: {cache}"])

    def test_explicit_directory_size_and_disabled_cache_stay(self):
        env = {"WINEPREFIX": "/unused", "MESA_SHADER_CACHE_DIR": "/games/mesa"}
        configure_mesa_shader_cache(env)
        self.assertEqual(env["MESA_SHADER_CACHE_DIR"], "/games/mesa")
        self.assertEqual(env["MESA_SHADER_CACHE_MAX_SIZE"], "4G")
        sized = {"WINEPREFIX": "/unused", "MESA_SHADER_CACHE_DIR": "/games/mesa",
                 "MESA_SHADER_CACHE_MAX_SIZE": "12G"}
        reports = []
        configure_mesa_shader_cache(sized, reports.append)
        self.assertEqual(sized["MESA_SHADER_CACHE_MAX_SIZE"], "12G")
        self.assertEqual(reports, ["Mesa shader cache: /games/mesa (path set by the environment)"])
        for value in ("1", "true"):
            disabled = {"WINEPREFIX": "/unused", "MESA_SHADER_CACHE_DISABLE": value}
            configure_mesa_shader_cache(disabled)
            self.assertEqual(list(disabled), ["WINEPREFIX", "MESA_SHADER_CACHE_DISABLE"])

    def test_launch_without_a_prefix_leaves_the_default_cache(self):
        env = {}
        configure_mesa_shader_cache(env)
        self.assertEqual(env, {})

    def test_the_host_sets_it_before_any_gpu_work(self):
        # Pipeline preparation runs with the host's own environment, so the
        # directory must be there, not only in the title's launch environment.
        with tempfile.TemporaryDirectory() as tmp, \
             patch("project_tuxbox.host.shutil.which", side_effect=lambda tool: f"/usr/bin/{tool}"), \
             patch("project_tuxbox.host.subprocess.check_output", return_value="wine-10.0\n"), \
             patch.dict(os.environ, clear=False) as environ:
            environ.pop("MESA_SHADER_CACHE_DIR", None)
            environ.pop("MESA_SHADER_CACHE_DISABLE", None)
            host = Host("wine", None, Path(tmp))
            cache = Path(tmp).resolve() / "shadercache"
            self.assertEqual(host.env["MESA_SHADER_CACHE_DIR"], str(cache))
            self.assertEqual(host.env["VKD3D_SHADER_CACHE_PATH"], str(cache))


class NvidiaShaderCache(unittest.TestCase):
    def test_prefix_gets_its_own_driver_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp) / "prefix"
            env = {"WINEPREFIX": str(prefix)}
            configure_nvidia_shader_cache(env)
            cache = prefix / "shadercache"
            self.assertEqual(env["__GL_SHADER_DISK_CACHE"], "1")
            self.assertEqual(env["__GL_SHADER_DISK_CACHE_PATH"], str(cache))
            self.assertEqual(env["__GL_SHADER_DISK_CACHE_SKIP_CLEANUP"], "1")
            self.assertEqual(env["__GL_SHADER_DISK_CACHE_SIZE"], str(4 << 30))
            self.assertTrue(cache.is_dir())

    def test_explicit_path_and_disabled_cache_stay(self):
        env = {"WINEPREFIX": "/unused", "__GL_SHADER_DISK_CACHE_PATH": "/games/nv",
               "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP": "0"}
        configure_nvidia_shader_cache(env)
        self.assertEqual(env["__GL_SHADER_DISK_CACHE_PATH"], "/games/nv")
        self.assertEqual(env["__GL_SHADER_DISK_CACHE_SKIP_CLEANUP"], "0")
        self.assertEqual(env["__GL_SHADER_DISK_CACHE_SIZE"], str(4 << 30))
        sized = {"WINEPREFIX": "/unused", "__GL_SHADER_DISK_CACHE_PATH": "/games/nv",
                 "__GL_SHADER_DISK_CACHE_SIZE": "1024"}
        reports = []
        configure_nvidia_shader_cache(sized, reports.append)
        self.assertEqual(sized["__GL_SHADER_DISK_CACHE_SIZE"], "1024")
        self.assertEqual(sized["__GL_SHADER_DISK_CACHE_SKIP_CLEANUP"], "1")
        self.assertEqual(reports, ["NVIDIA shader cache: /games/nv (path set by the environment)"])
        disabled = {"WINEPREFIX": "/unused", "__GL_SHADER_DISK_CACHE": "0"}
        configure_nvidia_shader_cache(disabled)
        self.assertEqual(list(disabled), ["WINEPREFIX", "__GL_SHADER_DISK_CACHE"])

    def test_launch_without_a_prefix_leaves_the_default_cache(self):
        env = {}
        configure_nvidia_shader_cache(env)
        self.assertEqual(env, {})

    @staticmethod
    def driver_cache(cache, name, entries, stored, toc=None):
        """A cache file pair: an index of (offset, size) entries and a sparse .bin of stored bytes."""
        bucket = cache / "GLCache" / "61f5fb60d33740dc5bc6da0ef0f7e3fc" / "565982cbdff4b94c"
        bucket.mkdir(parents=True, exist_ok=True)
        header = b"CDVN" + bytes(28)
        if toc is None:
            toc = header + b"".join(struct.pack("<16sII", bytes(16), *entry) for entry in entries)
        (bucket / f"{name}.toc").write_bytes(toc)
        blob = bucket / f"{name}.bin"
        with open(blob, "wb") as file:
            file.write(header)
            file.truncate(stored)
        return blob

    def test_a_fully_indexed_cache_is_not_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            # Each record has 32 bytes of its own before the payload its entry sizes.
            self.driver_cache(cache, "full", [(32, 7515), (7579, 1000)], 7579 + 32 + 1000)
            self.driver_cache(cache, "empty", [], 32)
            reports = []
            report_unindexed_nvidia_cache(cache, reports.append)
            self.assertEqual(reports, [])

    def test_a_cache_past_its_index_is_reported_and_left_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp) / "prefix"
            cache = prefix / "shadercache"
            # The measured file: the last entry ends 11 KB past 4 GiB in an 8.3 GB .bin.
            large = self.driver_cache(cache, "large", [(32, 7514), (0xFFFF27CB, 66486)], 8304209601)
            small = self.driver_cache(cache, "small", [(32, 968)], 1000 + (3 << 20))
            before = {path: (path.stat().st_size, path.stat().st_mtime_ns) for path in cache.rglob("*.*")}
            reports = []
            own = {"__GL_SHADER_DISK_CACHE_APP_NAME": "mine"}
            configure_nvidia_shader_cache({"WINEPREFIX": str(prefix), **own}, reports.append)
            self.assertEqual(len(reports), 3)
            self.assertIn(f"{large} has stopped being indexed: 3,824 MiB of its 7.7 GiB is past the end", reports[1])
            self.assertIn(f"{small} has stopped being indexed: 3 MiB of its 0.0 GiB is past the end", reports[2])
            self.assertIn("Move the .bin and .toc aside", reports[1])
            self.assertEqual({path: (path.stat().st_size, path.stat().st_mtime_ns) for path in cache.rglob("*.*")},
                             before)
            explicit = []
            configure_nvidia_shader_cache({"__GL_SHADER_DISK_CACHE_PATH": str(cache)}, explicit.append)
            self.assertEqual(explicit[1:], reports[1:])

    def test_a_launch_writes_a_file_with_room_and_reads_the_rest(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp) / "prefix"
            cache = prefix / "shadercache"

            def launch(**extra):
                env = {"WINEPREFIX": str(prefix), **extra}
                configure_nvidia_shader_cache(env)
                return (env.get("__GL_SHADER_DISK_CACHE_APP_NAME"),
                        env.get("__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME"))

            self.assertEqual(launch(), ("tuxbox1", None))
            # The driver's own file, past its index, is still read. A bare header is not.
            self.driver_cache(cache, "92f74448a56b9fe8", [], 5 << 30)
            self.driver_cache(cache, "92f74448a56b9fe6", [], 32)
            self.driver_cache(cache, "tuxbox10", [], (1 << 30) - 1)
            self.assertEqual(launch(), ("tuxbox1", "92f74448a56b9fe8"))
            self.driver_cache(cache, "tuxbox12", [], 1 << 30)
            self.assertEqual(launch(), ("tuxbox2", "92f74448a56b9fe8;tuxbox10;tuxbox12"))
            for number in range(2, 11):
                self.driver_cache(cache, f"tuxbox{number}0", [], 1 << 30 if number < 10 else 64)
            name, read = launch()
            self.assertEqual(name, "tuxbox10")
            self.assertEqual(len(read.split(";")), 11)
            self.assertNotIn("tuxbox100", read.split(";"))
            self.assertEqual(launch(__GL_SHADER_DISK_CACHE_APP_NAME="mine"), ("mine", None))
            self.assertEqual(launch(__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME="theirs"), (None, "theirs"))

    def test_a_cache_at_the_index_limit_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            blob = self.driver_cache(cache, "limit", [(0xFFFFFF00, 224)], 1 << 32)
            reports = []
            report_unindexed_nvidia_cache(cache, reports.append)
            self.assertEqual(len(reports), 1)
            self.assertIn(f"{blob} has stopped being indexed: it has reached 4 GiB", reports[0])

    def test_unreadable_cache_files_are_not_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            report = Mock()
            report_unindexed_nvidia_cache(cache, report)
            header = b"CDVN" + bytes(28)
            self.driver_cache(cache, "nothing", [], 5 << 20, toc=b"")
            self.driver_cache(cache, "short", [], 5 << 20, toc=header[:20])
            self.driver_cache(cache, "torn", [], 5 << 20, toc=header + bytes(23))
            self.driver_cache(cache, "foreign", [], 5 << 20, toc=bytes(32 + 24))
            self.driver_cache(cache, "alone", [(32, 968)], 5 << 20).unlink()
            (cache / "GLCache" / "61f5fb60d33740dc5bc6da0ef0f7e3fc" / "565982cbdff4b94c" / "folder.toc").mkdir()
            report_unindexed_nvidia_cache(cache, report)
            report_unindexed_nvidia_cache(cache / "missing", report)
            report.assert_not_called()

    def test_a_device_bucket_beside_glcache_fills_only_missing_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp) / "prefix"
            device = "0ce6f6b7af2931dc61b40b11eb345732"
            app = "92f4bb582845de36"
            cache = prefix / "shadercache"
            misplaced = cache / device / app
            misplaced.mkdir(parents=True)
            (misplaced / "92f74448a56b9fe8.bin").write_bytes(b"x" * 50)
            (misplaced / "92f74448a56b9fe8.toc").write_bytes(b"t" * 8)
            (misplaced / "extra.bin").write_bytes(b"new")
            live = cache / "GLCache" / device / app
            live.mkdir(parents=True)
            (live / "92f74448a56b9fe8.bin").write_bytes(b"y" * 10)
            (live / "92f74448a56b9fe8.toc").write_bytes(b"u" * 4)
            other = cache / "fossilize"
            other.mkdir()
            (other / "keep").write_bytes(b"stay")
            configure_nvidia_shader_cache({"WINEPREFIX": str(prefix)})
            self.assertEqual((live / "92f74448a56b9fe8.bin").read_bytes(), b"y" * 10)
            self.assertEqual((live / "92f74448a56b9fe8.toc").read_bytes(), b"u" * 4)
            self.assertEqual((live / "extra.bin").read_bytes(), b"new")
            self.assertFalse((cache / device).exists())
            self.assertEqual((other / "keep").read_bytes(), b"stay")
