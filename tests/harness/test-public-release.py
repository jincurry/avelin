"""Publication-boundary tests; no build, installation, network, or credentials."""
from __future__ import annotations

import argparse
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
import zipfile


SCRIPT = Path(__file__).resolve().parents[2] / "scripts/export-public-release.py"
SPEC = importlib.util.spec_from_file_location("public_release", SCRIPT)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


def fixture_metadata():
    workspace = "C:/Users/private-builder/work/project"
    local = "path+file:///" + workspace + "/apps/demo#1.0.0"
    upstream = "registry+https://github.com/rust-lang/crates.io-index#dependency@1.0.0"
    return {"version": 1, "workspace_root": workspace,
            "workspace_members": [local], "workspace_default_members": [local],
            "target_directory": workspace + "/target", "metadata": {"private": "not-exported"},
            "packages": [
                {"id": local, "name": "demo", "version": "1.0.0", "source": None,
                 "manifest_path": workspace + "/apps/demo/Cargo.toml", "features": {"default": []},
                 "authors": ["Private Builder"], "targets": [{"name": "demo", "kind": ["bin"],
                 "crate_types": ["bin"], "edition": "2024", "src_path": workspace + "/apps/demo/src/main.rs"}],
                 "dependencies": [{"name": "dependency", "source": "registry+https://github.com/rust-lang/crates.io-index",
                                   "req": "^1.0", "kind": None, "features": []}]},
                {"id": upstream, "name": "dependency", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index", "license": "MIT",
                 "manifest_path": "C:/Users/private-builder/.cargo/registry/dependency/Cargo.toml",
                 "targets": [{"name": "dependency", "kind": ["lib"], "src_path": "private"}],
                 "dependencies": [], "features": {}}],
            "resolve": {"root": None, "nodes": [
                {"id": local, "features": [], "dependencies": [upstream],
                 "deps": [{"name": "dependency", "pkg": upstream,
                           "dep_kinds": [{"kind": None, "target": None}]}]},
                {"id": upstream, "features": [], "dependencies": [], "deps": []}]}}


def write(root, name, data):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else release.json_bytes(data))


def seal(root, name):
    write(root, name, {"schema_version": 1, "algorithm": "SHA256",
                       "files": release.records(root, {name})})


class ProjectionTests(unittest.TestCase):
    def test_projection_retains_dependency_identity_edges_features_and_relative_workspace(self):
        graph = release.project_metadata(release.json_bytes(fixture_metadata()), "x86_64-pc-windows-msvc", ["demo"])
        text = json.dumps(graph)
        self.assertNotIn("private-builder", text)
        self.assertNotIn("Private Builder", text)
        self.assertNotIn("src_path", text)
        local = graph["packages"][0]
        upstream = graph["packages"][1]
        self.assertEqual(local["workspace_manifest"], "apps/demo/Cargo.toml")
        self.assertEqual(local["features"], {"default": []})
        self.assertEqual(graph["resolve"]["nodes"][0]["dependencies"], [upstream["id"]])
        self.assertEqual(graph["workspace_members"], [local["id"]])
        self.assertEqual(upstream["source"], fixture_metadata()["packages"][1]["source"])

    def test_local_dependency_outside_workspace_is_rejected(self):
        metadata = fixture_metadata()
        metadata["packages"][0]["dependencies"][0]["path"] = "C:/Users/private-builder/elsewhere"
        with self.assertRaisesRegex(ValueError, "escapes"):
            release.project_metadata(release.json_bytes(metadata), "target", ["demo"])

    def test_unknown_graph_reference_is_rejected(self):
        metadata = fixture_metadata()
        metadata["resolve"]["nodes"][0]["dependencies"].append("unknown")
        with self.assertRaises(KeyError):
            release.project_metadata(release.json_bytes(metadata), "target", ["demo"])

    def test_scanner_rejects_private_pe_bytes_without_modifying_them(self):
        scanner = release.PrivacyScanner(["private-build-account"])
        for data in (b"MZ" + "private-build-account".encode(),
                     b"MZ_" + "C:\\Users\\private-builder\\source.rs".encode("utf-16-le"),
                     b"MZ\x00" + ("gh" + "p_" + "a" * 35).encode()):
            original = bytes(data)
            with self.assertRaises(ValueError):
                scanner.inspect("bin/demo.exe", data)
            self.assertEqual(data, original)

    def test_scanner_preserves_copyright_names_and_public_urls(self):
        data = b"Copyright 2026 Upstream Author. https://api.github.com/users/upstream/events"
        release.PrivacyScanner().inspect("licenses/LICENSE", data)
        release.PrivacyScanner().inspect("bin/runtime.exe", b"mask-border-mask-composite-mask-type-mask-repeat-mask-size")

    def test_paths_reject_traversal_windows_aliases_and_streams(self):
        for path in ("../LICENSE", "a/../b", "C:/private", "a\\b", "a//b", "a/CON.txt", "a/b. ", "x:stream"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                release.relative(path)


class ExportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.repo_tmp = tempfile.TemporaryDirectory(prefix="avelin-release-source-")
        cls.addClassCleanup(cls.repo_tmp.cleanup)
        cls.repo = Path(cls.repo_tmp.name)
        def git(*args):
            return subprocess.run(["git", *args], cwd=cls.repo, capture_output=True, check=True).stdout
        git("init", "--quiet")
        git("config", "user.name", "Release Test")
        git("config", "user.email", "release-test@example.invalid")
        git("config", "core.autocrlf", "false")
        git("config", "commit.gpgsign", "false")
        for name, data in {".gitattributes": b"* text=auto eol=lf\n", "Cargo.lock": b"version = 4\n",
                           "src/main.rs": b"fn main() {}\n"}.items():
            write(cls.repo, name, data)
        git("add", ".")
        git("commit", "--quiet", "-m", "Fixture")
        cls.commit = git("rev-parse", "HEAD").decode().strip()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="avelin-public-release-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.kit = self.root / "input"
        self.kit.mkdir()
        source = {"schema_version": 1, "algorithm": "SHA256", "git_head": self.commit,
                  "working_tree_dirty": False, "files": release.records(self.repo, set())}
        source["files"] = [item for item in source["files"] if not item["path"].startswith(".git/")]
        write(self.kit, "SOURCE-SNAPSHOT.json", source)
        write(self.kit, "README.md", ("# Avelin Rust Windows x64 开发者工具包\n\n" + release.LOCAL_README_NOTICE).encode("utf-8"))
        write(self.kit, "LICENSE", b"Unchanged project MIT license bytes.\r\n")
        write(self.kit, "bin/avelin-app.exe", b"MZ\x00fixed executable bytes")
        write(self.kit, "third-party/licenses/dependency/LICENSE", b"Unchanged upstream copyright.\r\n")
        write(self.kit, "third-party/NOTICE.md", b"Declared limitations remain unchanged.\n")
        write(self.kit, "third-party/sources/dependency/lib.rs", b"// Upstream source preserved\n")
        write(self.kit, "third-party/unknowns.json", [{"issue": "Unchanged limitation"}])
        metadata = release.json_bytes(fixture_metadata())
        write(self.kit, "third-party/metadata/x86_64-pc-windows-msvc.json.gz", gzip.compress(metadata))
        inventory = {"schema_version": 1, "cargo_metadata": [{"target": "x86_64-pc-windows-msvc", "roots": ["demo"],
                     "arguments": ["C:/private/cargo.exe", "metadata"], "json_sha256": release.sha(metadata),
                     "path": "metadata/x86_64-pc-windows-msvc.json.gz"}]}
        write(self.kit, "third-party/inventory.json", inventory)
        build = {"schema_version": 1, "candidate_id": "fixture", "engine": "rust",
                 "platform": "x86_64-pc-windows-msvc", "signed": False, "cli_version": "avelin-cli 0.1.0-preview.1",
                 "git_head": self.commit, "source_snapshot_sha256": release.sha((self.kit / "SOURCE-SNAPSHOT.json").read_bytes()),
                 "license_evidence": {"inventory_sha256": release.sha((self.kit / "third-party/inventory.json").read_bytes()),
                                      "raw_local_metadata_preserved": True}}
        write(self.kit, "BUILD-INFO.json", build)
        self.reseal()
        self.args = argparse.Namespace(kit=self.kit, output=self.root / "output", project_root=self.repo,
                                       expected_kit_hashes_sha256=release.sha((self.kit / "HASHES.json").read_bytes()),
                                       commit=self.commit, version="0.1.0-preview.1", deny_text=["private-builder"])

    def reseal(self):
        seal(self.kit / "third-party", "FILES.json")
        seal(self.kit, "HASHES.json")

    def refresh_expected(self):
        self.reseal()
        self.args.expected_kit_hashes_sha256 = release.sha((self.kit / "HASHES.json").read_bytes())

    def test_export_preserves_inputs_licenses_executables_and_seals_zip(self):
        before = {p.relative_to(self.kit).as_posix(): p.read_bytes() for p in release.tree(self.kit)}
        result = release.export(self.args)
        output = self.args.output / release.KIT_NAME
        self.assertEqual(result["status"], "PASSED")
        self.assertEqual(before, {p.relative_to(self.kit).as_posix(): p.read_bytes() for p in release.tree(self.kit)})
        for name in ("LICENSE", "bin/avelin-app.exe", "third-party/licenses/dependency/LICENSE",
                     "third-party/sources/dependency/lib.rs", "third-party/NOTICE.md", "third-party/unknowns.json"):
            self.assertEqual((output / name).read_bytes(), before[name])
        self.assertFalse((output / "third-party/metadata/x86_64-pc-windows-msvc.json.gz").exists())
        release.exact_inventory(output, "HASHES.json")
        release.exact_inventory(output / "third-party", "FILES.json")
        with zipfile.ZipFile(self.args.output / result["archive"]) as archive:
            self.assertTrue(all(name.startswith(release.KIT_NAME + "/") for name in archive.namelist()))
            self.assertEqual(len(archive.namelist()), result["delivered_files"])
        build = release.read_json(output / "BUILD-INFO.json")
        self.assertEqual(build["git_head"], self.commit)
        self.assertFalse(build["license_evidence"]["raw_local_metadata_preserved"])

    def test_extra_or_changed_input_file_is_rejected(self):
        for name, data in (("extra.txt", b"unlisted"), ("bin/avelin-app.exe", b"changed")):
            with self.subTest(name=name):
                path = self.kit / name
                previous = path.read_bytes() if path.exists() else None
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    release.export(self.args)
                if previous is None:
                    path.unlink()
                else:
                    path.write_bytes(previous)

    def test_private_binary_is_rejected_even_with_fresh_integrity_records(self):
        path = self.kit / "bin/avelin-app.exe"
        path.write_bytes(b"MZ" + "C:/Users/private-builder/source.rs".encode())
        self.refresh_expected()
        with self.assertRaisesRegex(ValueError, "Private identifier|user-directory"):
            release.export(self.args)
        self.assertIn(b"private-builder", path.read_bytes())

    def test_delivered_mutation_after_sealing_cannot_produce_success(self):
        original_check = release.exact_inventory
        changed = False
        def mutate_after_seal(root, name):
            nonlocal changed
            result = original_check(root, name)
            if root == self.args.output / release.KIT_NAME and not changed:
                changed = True
                (root / "bin/avelin-app.exe").write_bytes(b"MZ changed after sealing")
            return result
        with mock.patch.object(release, "exact_inventory", side_effect=mutate_after_seal):
            with self.assertRaisesRegex(ValueError, "after sealing"):
                release.export(self.args)
        self.assertFalse((self.args.output / "public-export-verification.json").exists())

    def test_existing_output_and_wrong_expected_digest_are_rejected(self):
        self.args.expected_kit_hashes_sha256 = "0" * 64
        with self.assertRaisesRegex(ValueError, "approved candidate"):
            release.export(self.args)
        self.args.output.mkdir()
        with self.assertRaisesRegex(ValueError, "must be new"):
            release.export(self.args)

    def test_output_cannot_change_original_kit_membership(self):
        self.args.output = self.kit / "public"
        with self.assertRaisesRegex(ValueError, "outside the original kit"):
            release.export(self.args)
        self.assertFalse(self.args.output.exists())

    def test_unknown_metadata_record_field_is_not_silently_dropped(self):
        value = release.read_json(self.kit / "third-party/inventory.json")
        value["cargo_metadata"][0]["unexpected_private_field"] = "private"
        with self.assertRaisesRegex(ValueError, "Unrecognized"):
            release.public_inventory(self.kit, value)

    def test_source_commit_mismatch_is_rejected(self):
        source = release.read_json(self.kit / "SOURCE-SNAPSHOT.json")
        source["files"][0]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "differs"):
            release.bind_source(self.repo, self.commit, source)
        source = release.read_json(self.kit / "SOURCE-SNAPSHOT.json")
        source["files"].pop()
        with self.assertRaisesRegex(ValueError, "file sets"):
            release.bind_source(self.repo, self.commit, source)

    def test_git_text_crlf_comparison_retains_packaging_hash(self):
        source = release.read_json(self.kit / "SOURCE-SNAPSHOT.json")
        item = next(item for item in source["files"] if item["path"] == "src/main.rs")
        data = (self.repo / item["path"]).read_bytes().replace(b"\n", b"\r\n")
        item.update(size=len(data), sha256=release.sha(data))
        original = copy.deepcopy(item)
        result = release.bind_source(self.repo, self.commit, source)
        bound = next(item for item in result["files"] if item["path"] == "src/main.rs")
        self.assertEqual(bound["sha256"], original["sha256"])
        self.assertEqual(bound["commit_comparison"], "git_text_lf_matches_packaging_crlf")
        self.assertNotEqual(bound["git_blob_sha256"], bound["sha256"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
