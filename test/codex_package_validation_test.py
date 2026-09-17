import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "sync", Path(__file__).resolve().parents[1] / "tools/codex-artifacts/sync_codex_artifacts.py"
)
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


class PackageValidationTest(unittest.TestCase):
    def test_release_missing_a_complete_package_cannot_be_selected(self):
        missing = "codex-package-aarch64-apple-darwin.tar.gz"
        release = {"assets": [
            dict(name=asset["name"], digest="sha256:" + "a" * 64,
                 browser_download_url="https://example.invalid/asset", size=1)
            for asset in sync.CLI_ASSETS if asset["name"] != missing
        ]}
        with self.assertRaisesRegex(RuntimeError, missing):
            sync.select_assets(release)

    def make_package(self, directory, platform="macos", arch="aarch64", omit=None,
                     empty=None, overrides=None, extra=None):
        target = f"{arch}-{'pc-windows-msvc' if platform == 'windows' else 'apple-darwin'}"
        suffix = ".exe" if platform == "windows" else ""
        metadata = dict(layoutVersion=1, variant="codex", target=target, version="1.2.3",
                        entrypoint=f"bin/codex{suffix}", pathDir="codex-path",
                        resourcesDir="codex-resources")
        metadata.update(overrides or {})
        files = {
            "codex-package.json": json.dumps(metadata).encode(),
            f"bin/codex{suffix}": b"executable",
            f"bin/codex-code-mode-host{suffix}": b"host",
            f"codex-path/rg{suffix}": b"ripgrep",
            "codex-resources/resource": b"resource",
        }
        files.pop(omit, None)
        if empty:
            files[empty] = b""
        asset = dict(name=f"codex-package-{target}.tar.gz", platform=platform,
                     install_layout="codex_package_v1")
        archive = Path(directory) / asset["name"]
        with tarfile.open(archive, "w:gz") as output:
            for name, content in list(files.items()) + (extra or []):
                member = tarfile.TarInfo(name)
                member.size = len(content)
                output.addfile(member, io.BytesIO(content))
        return asset, archive

    def test_complete_packages_for_each_supported_target(self):
        with tempfile.TemporaryDirectory() as directory:
            for platform in ("macos", "windows"):
                for arch in ("aarch64", "x86_64"):
                    with self.subTest(platform=platform, arch=arch):
                        asset, archive = self.make_package(directory, platform, arch)
                        sync.validate_cli_package(asset, archive, "1.2.3")

    def test_windows_metadata_accepts_native_separators(self):
        with tempfile.TemporaryDirectory() as directory:
            asset, archive = self.make_package(directory, "windows", overrides={
                "entrypoint": "bin\\codex.exe"})
            sync.validate_cli_package(asset, archive, "1.2.3")

    def test_missing_or_empty_execution_components_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ("bin/codex", "bin/codex-code-mode-host", "codex-path/rg",
                         "codex-package.json", "codex-resources/resource"):
                for mutation in ("omit", "empty"):
                    if mutation == "empty" and name == "codex-resources/resource":
                        continue
                    with self.subTest(name=name, mutation=mutation):
                        asset, archive = self.make_package(directory, **{mutation: name})
                        with self.assertRaisesRegex(RuntimeError, "missing"):
                            sync.validate_cli_package(asset, archive, "1.2.3")

    def test_wrong_release_and_platform_metadata_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for overrides in ({"version": "1.2.4"}, {"target": "x86_64-apple-darwin"},
                              {"variant": "other"}, {"layoutVersion": 2},
                              {"entrypoint": "other/codex"}, {"pathDir": "../path"}):
                with self.subTest(overrides=overrides):
                    asset, archive = self.make_package(directory, overrides=overrides)
                    with self.assertRaises(RuntimeError):
                        sync.validate_cli_package(asset, archive, "1.2.3")

    def test_duplicate_and_unsafe_members_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for name in ("bin/codex-code-mode-host", "../outside", "/absolute"):
                with self.subTest(name=name):
                    asset, archive = self.make_package(directory, extra=[(name, b"bad")])
                    with self.assertRaises(RuntimeError):
                        sync.validate_cli_package(asset, archive, "1.2.3")

    def test_retained_legacy_asset_is_not_treated_as_complete_package(self):
        sync.validate_cli_package({"install_layout": "legacy_single_binary_archive"},
                                  Path("does-not-exist"), "1.2.3")


class PublicationOrderingTest(unittest.TestCase):
    def test_cli_component_preserves_the_published_v4_consumer_contract(self):
        fixture = json.loads((Path(__file__).parent / "fixtures/codex-artifacts-manifest-v4.json").read_text())
        generated = sync.manifest_v4_for({"tag_name": "rust-v1.2.3"}, [],
                                         "https://example.invalid", "artifacts")
        # Released Connector parsers reject unknown component fields. Package
        # layout belongs on each asset, without extending that closed contract.
        self.assertEqual(set(generated["components"]["codex_cli"]),
                         set(fixture["components"]["codex_cli"]))

    def test_component_metadata_correction_is_published_even_for_same_assets(self):
        release = {"tag_name": "rust-v1.2.3"}
        legacy = sync.manifest_for(release, [], sync.DEFAULT_PUBLIC_BASE, sync.DEFAULT_PREFIX)
        full = sync.manifest_v4_for(release, [], sync.DEFAULT_PUBLIC_BASE, sync.DEFAULT_PREFIX)
        desktop = sync.desktop_manifest_v4_for(full)
        for changed in (False, True):
            current = json.loads(json.dumps([legacy, full, desktop]))
            if changed:
                current[1]["components"]["codex_cli"]["macos_install_layout"] = "codex_package_v1"
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as directory, \
                    patch.object(sync, "request_json", return_value=release), \
                    patch.object(sync, "select_assets", return_value=[]), \
                    patch.object(sync, "validate_manifest"), \
                    patch.object(sync, "fetch_existing_manifest", side_effect=current), \
                    patch.object(sync, "publish") as publish:
                sync.run(SimpleNamespace(release_json=None, work_dir=directory, prepare_only=False))
                self.assertEqual(publish.call_count, int(changed))

    def test_failed_immutable_manifest_verification_preserves_latest(self):
        manifest = {"assets": [], "snapshot_id": "test"}
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(sync, "public_asset_is_exact", return_value=False), \
                patch.object(sync, "oss_cp") as upload:
            with self.assertRaisesRegex(RuntimeError, "immutable manifest"):
                sync.publish(manifest, manifest, manifest, {}, Path(directory))
            self.assertEqual(upload.call_count, 1)
            self.assertIn("/manifests/sha256/", upload.call_args.args[1])
            self.assertNotIn("latest.json", upload.call_args.args[1])

    def test_all_immutable_manifests_verified_before_first_latest_write(self):
        manifest = {"assets": [], "snapshot_id": "test"}
        events = []
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(sync, "public_asset_is_exact",
                             side_effect=lambda url, path: events.append(("verify", url)) or True), \
                patch.object(sync, "oss_cp",
                             side_effect=lambda path, url, *args: events.append(("write", url))):
            sync.publish(manifest, manifest, manifest, {}, Path(directory))
        first_write = next(i for i, event in enumerate(events) if event[0] == "write")
        self.assertEqual(sum("/manifests/sha256/" in url
                             for action, url in events[:first_write]), 6)
        self.assertEqual(sum(action == "write" for action, url in events), 3)


if __name__ == "__main__":
    unittest.main()
