"""Install the checked-in plugin packages without touching the real profile."""

import json
import os
import runpy
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "tools/install_plugins.py"


class PluginPackages(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.config = Path(self.scratch.name) / "config"

    def tearDown(self):
        self.scratch.cleanup()

    def run_tool(self, *args, installer=INSTALLER):
        return subprocess.run(
            [sys.executable, str(installer), "--config-dir", str(self.config), *args],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )

    def test_catalog_lists_every_plugin_and_validates_payloads(self):
        result = self.run_tool("--list")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("api-cost", result.stdout)
        self.assertIn("unmerged-branches", result.stdout)
        self.assertIn("sidebar-controls", result.stdout)
        self.assertIn("agent-update", result.stdout)
        result = self.run_tool("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.config.exists())

    def test_install_all_includes_helper_and_preserves_profile(self):
        self.config.mkdir()
        (self.config / "config.json").write_text('{"private":"keep"}')
        (self.config / "profile.json").write_text('{"plugin_source_dir":"/selected/source"}')
        result = self.run_tool("--all")
        self.assertEqual(result.returncode, 0, result.stderr)
        for relative in (
            "plugins/api-cost.lua",
            "plugins/sidebar-controls.lua",
            "plugins/agent-update.lua",
        ):
            self.assertEqual((self.config / relative).read_bytes(), (ROOT / relative).read_bytes())
        self.assertEqual((self.config / "config.json").read_text(), '{"private":"keep"}')
        self.assertEqual((self.config / "profile.json").read_text(), '{"plugin_source_dir":"/selected/source"}')
        self.assertFalse((self.config / "plugins/unmerged-branches.lua").exists())
        result = self.run_tool("api-cost")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(list(self.config.rglob("*.before-plugin-update-*")))

    def test_task_sources_installs_service_without_enabling_sources(self):
        result = self.run_tool("task-sources")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.config / "service/task-sources.lua").read_bytes(),
            (ROOT / "lua/task_sources/service.lua").read_bytes(),
        )
        self.assertFalse((self.config / "task-sources.json").exists())
        self.assertFalse((self.config / "plugins/task-sources.lua").exists())

    def test_a_package_installs_the_helpers_it_requires(self):
        result = self.run_tool("agent-plugins")
        self.assertEqual(result.returncode, 0, result.stderr)
        for relative in (
            "plugins/agent-plugins.lua",
            "lua/agent_settings/files.lua",
            "lua/agent_settings/json.lua",
            "lua/agent_settings/toml.lua",
        ):
            self.assertEqual((self.config / relative).read_bytes(), (ROOT / relative).read_bytes())
        self.assertFalse((self.config / "plugins/agent-limit.lua").exists())

    def test_conflict_aborts_before_creating_any_other_payload(self):
        destination = self.config / "plugins/api-cost.lua"
        destination.parent.mkdir(parents=True)
        destination.write_text("-- owner customisation\n")
        result = self.run_tool("--all")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Different file", result.stderr)
        self.assertEqual(destination.read_text(), "-- owner customisation\n")
        self.assertFalse((self.config / "plugins/sidebar-controls.lua").exists())

    def test_update_retains_recoverable_previous_plugin(self):
        destination = self.config / "plugins/api-cost.lua"
        destination.parent.mkdir(parents=True)
        destination.write_text("-- previous plugin\n")
        result = self.run_tool("api-cost", "--update")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(destination.read_bytes(), (ROOT / "plugins/api-cost.lua").read_bytes())
        backups = list(destination.parent.glob("api-cost.lua.before-plugin-update-*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(), "-- previous plugin\n")

    def test_dry_run_and_unknown_name_leave_no_files(self):
        self.assertEqual(self.run_tool("--all", "--dry-run").returncode, 0)
        self.assertFalse(self.config.exists())
        result = self.run_tool("does-not-exist")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown plugins", result.stderr)
        self.assertFalse(self.config.exists())

    def test_symlink_escape_cannot_overwrite_another_folder(self):
        outside = Path(self.scratch.name) / "outside"
        outside.mkdir()
        sentinel = outside / "api-cost.lua"
        sentinel.write_text("keep")
        self.config.mkdir()
        (self.config / "plugins").symlink_to(outside, target_is_directory=True)
        result = self.run_tool("api-cost", "--update")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("linked install target", result.stderr)
        self.assertEqual(sentinel.read_text(), "keep")


    def test_repository_switch_refuses_a_python_dot_registration_and_changes_nothing(self):
        state = Path(self.scratch.name) / "registered-dot"
        state.mkdir(mode=0o700)
        (state / "settings.json").write_text('{"private":"retained"}')
        path = self.config / "providers/dot.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({
            "id": "dot", "label": "Dot",
            "command": [sys.executable, str(self.config / "lua/dot/cli.py"), "chat", "--state-dir", str(state)],
            "broadcast": True, "cloud": True, "chat_snapshot": str(state / "transcript.json"),
            "working_patterns": ["Dot is working"], "prompt_patterns": [],
        }))
        for dry_run in ([], ["--dry-run"]):
            before = {str(p): p.read_bytes() for p in Path(self.scratch.name).rglob("*") if p.is_file()}
            result = self.run_tool("--repo-sources", *dry_run)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("custom Dot provider", result.stderr)
            self.assertFalse((self.config / "profile.json").exists())
            self.assertEqual({str(p): p.read_bytes() for p in Path(self.scratch.name).rglob("*") if p.is_file()}, before)

    def test_repository_switch_preserves_a_profile_edited_during_validation(self):
        profile = self.config / "profile.json"
        self.config.mkdir()
        profile.write_text('{"personal":"before"}')
        tool = runpy.run_path(str(INSTALLER))
        original_loads = json.loads
        changed = False

        def edit_after_read(data, *args, **kwargs):
            nonlocal changed
            result = original_loads(data, *args, **kwargs)
            if result == {"personal": "before"} and not changed:
                changed = True
                profile.write_text('{"personal":"after"}')
            return result

        with patch.object(json, "loads", side_effect=edit_after_read):
            self.assertEqual(tool["main"](["--repo-sources", "--config-dir", str(self.config)]), 1)
        self.assertTrue(changed)
        self.assertEqual(profile.read_text(), '{"personal":"after"}')

    def test_failed_profile_publication_keeps_the_profile(self):
        profile = self.config / "profile.json"
        self.config.mkdir()
        profile.write_text('{"personal":"retained"}')
        tool = runpy.run_path(str(INSTALLER))
        original_replace = os.replace

        def fail_publication(source, destination, *args, **kwargs):
            if Path(destination) == profile:
                raise OSError("profile publication fixture")
            return original_replace(source, destination, *args, **kwargs)

        with patch.object(os, "replace", side_effect=fail_publication):
            self.assertEqual(tool["main"](["--repo-sources", "--config-dir", str(self.config)]), 1)
        self.assertEqual(profile.read_text(), '{"personal":"retained"}')
        self.assertEqual([p.name for p in self.config.iterdir() if p.name.startswith(".plugin-profile-")], [])

    def future_package(self):
        root = Path(self.scratch.name) / "future-deck"
        installer = root / "tools/install_plugins.py"
        installer.parent.mkdir(parents=True)
        installer.write_bytes(INSTALLER.read_bytes())
        for relative, content in {
            "plugins/future.lua": "-- future plugin\n",
            "lua/future/helper.py": "# helper\n",
        }.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        catalog = {
            "version": 1,
            "plugins": {
                "future": {
                    "kind": "optional",
                    "description": "A newly added plugin",
                    "files": [
                        {"source": path, "target": path}
                        for path in ("plugins/future.lua", "lua/future/helper.py")
                    ],
                }
            },
        }
        manifest = root / "plugins/catalog.json"
        manifest.write_text(json.dumps(catalog))
        return installer, manifest, catalog

    def test_future_plugin_and_helper_install_without_installer_changes(self):
        installer, _, _ = self.future_package()
        result = self.run_tool("--all", installer=installer)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.config / "plugins/future.lua").read_text(), "-- future plugin\n")
        self.assertEqual((self.config / "lua/future/helper.py").read_text(), "# helper\n")

    def test_public_export_omits_private_payload_and_remains_installable(self):
        installer, _, _ = self.future_package()
        root = installer.parents[1]
        repository = root.parent / "repository"
        deck = repository
        root.rename(deck)
        private = deck / "private"
        (private / "plugins").mkdir(parents=True)
        (private / "plugins/personal.lua").write_text("-- private owner code\n")
        (private / "catalog.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "plugins": {
                        "personal": {
                            "kind": "private",
                            "description": "Private plugin",
                            "files": [
                                {
                                    "source": "private/plugins/personal.lua",
                                    "target": "plugins/personal.lua",
                                }
                            ],
                        }
                    },
                }
            )
        )
        (deck / ".gitattributes").write_bytes((ROOT / ".gitattributes").read_bytes())
        exporter = deck / "tools/export_source.py"
        exporter.write_bytes((ROOT / "tools/export_source.py").read_bytes())
        git = [
            "git",
            "-C",
            str(repository),
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Plugin fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ]
        for args in (("init", "--quiet"), ("add", "."), ("commit", "--quiet", "-m", "fixture")):
            subprocess.run([*git, *args], check=True, capture_output=True)
        output = Path(self.scratch.name) / "public.tar"
        result = subprocess.run(
            [sys.executable, str(exporter), "--output", str(output)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        public = Path(self.scratch.name) / "public"
        with tarfile.open(output) as archive:
            self.assertFalse(any("private" in Path(name).parts for name in archive.getnames()))
            archive.extractall(public, filter="data")
        result = self.run_tool("--all", installer=public / "cepheus/tools/install_plugins.py")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.config / "plugins/future.lua").read_text(), "-- future plugin\n")
        self.assertFalse((self.config / "plugins/personal.lua").exists())
        result = self.run_tool("personal", installer=deck / "tools/install_plugins.py")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.config / "plugins/personal.lua").read_text(), "-- private owner code\n"
        )
        # An accidentally removed export attribute must fail closed before an
        # archive containing private source can be created.
        (deck / ".gitattributes").write_text("")
        subprocess.run([*git, "add", "."], check=True, capture_output=True)
        subprocess.run(
            [*git, "commit", "--quiet", "-m", "missing exclusion"], check=True, capture_output=True
        )
        rejected = Path(self.scratch.name) / "rejected.tar"
        result = subprocess.run(
            [sys.executable, str(exporter), "--output", str(rejected)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(rejected.exists())

    def test_unsafe_catalog_paths_and_unregistered_plugins_are_rejected(self):
        installer, manifest, catalog = self.future_package()
        payload = catalog["plugins"]["future"]["files"][1]
        for target in ("../outside.py", "/absolute.py", "config.json"):
            with self.subTest(target=target):
                payload["target"] = target
                manifest.write_text(json.dumps(catalog))
                result = self.run_tool("--all", installer=installer)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.config.exists())
        payload["target"] = payload["source"]
        manifest.write_text(json.dumps(catalog))
        (manifest.parent / "unregistered.lua").write_text("-- missing catalog entry\n")
        result = self.run_tool("--check", installer=installer)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be registered", result.stderr)


if __name__ == "__main__":
    unittest.main()
