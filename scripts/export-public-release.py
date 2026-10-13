#!/usr/bin/env python3
"""Export an integrity-checked developer kit without private Cargo cache paths.

Python 3.11+, Git, and the standard library only. No compilation, installation,
network access, upload, or changes to executable/license/source bytes occur.
Failed output is retained; always choose a new output directory for a retry.
"""
from __future__ import annotations

import argparse
import copy
import datetime
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
import stat
import subprocess
import zipfile


HEX = re.compile(r"[0-9a-f]{64}\Z")
RAW_METADATA_KEYS = {"target", "roots", "arguments", "json_sha256", "path"}
PACKAGE_FIELDS = ("name", "version", "license", "source", "repository", "homepage",
                  "edition", "rust_version", "features")
DEPENDENCY_FIELDS = ("name", "source", "req", "kind", "rename", "optional",
                     "uses_default_features", "features", "target", "registry")
TARGET_FIELDS = ("name", "kind", "crate_types", "edition")
KIT_NAME = "Avelin-Rust-DeveloperKit"
LOCAL_README_NOTICE = "原始元数据可能含本机构建路径，或包含未链接进本包的依赖。公开发布附件前应单独审阅这些材料。"
PUBLIC_README_NOTICE = "公开包使用相对路径和上游标识构成的依赖图；包含本机路径的原始 Cargo 元数据只保留在私有构包证据中。依赖图可能包含未链接进本包的依赖。导出细节见 PUBLIC-RELEASE.json。"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def no_links(path):
    path = Path(path).absolute()
    for item in reversed((path, *path.parents)):
        try:
            info = item.lstat()
        except FileNotFoundError:
            continue
        require(not (stat.S_ISLNK(info.st_mode) or
                     getattr(info, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT),
                "Symlink or reparse point is not accepted")
    return path


def relative(value):
    require(isinstance(value, str) and value and
            not re.search(r'[\\:"<>|?*\x00-\x1f]', value), "Invalid relative path")
    for part in value.split("/"):
        require(part not in {"", ".", ".."} and not part.endswith((".", " ")) and
                not re.match(r"^(CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])(?:\.|$)", part, re.I),
                "Ambiguous or reserved path component")
    return value


def child(root, name):
    return no_links(root / relative(name))


def tree(root):
    no_links(root)
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in sorted(dirs + files):
            path = no_links(Path(directory) / name)
            require(path.is_dir() or path.is_file(), "Non-regular evidence file")
        for name in sorted(files):
            yield Path(directory) / name


def read_json(path):
    return json.loads(no_links(path).read_text(encoding="utf-8-sig"))


def write_new(path, data):
    no_links(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def record(root, path):
    data = no_links(path).read_bytes()
    return {"path": relative(path.relative_to(root).as_posix()), "size": len(data), "sha256": sha(data)}


def records(root, excluded):
    return [record(root, path) for path in sorted(tree(root))
            if path.relative_to(root).as_posix() not in excluded]


def check_record(root, item):
    require(type(item.get("size")) is int and item["size"] >= 0 and
            isinstance(item.get("sha256"), str) and HEX.fullmatch(item["sha256"]),
            "Invalid file record")
    data = child(root, item["path"]).read_bytes()
    require(len(data) == item["size"] and sha(data) == item["sha256"],
            "Evidence content differs: " + item["path"])
    return data


def exact_inventory(root, filename):
    value = read_json(child(root, filename))
    require(value.get("schema_version") == 1 and value.get("algorithm") == "SHA256" and
            value.get("files"), "Unsupported or empty file inventory")
    names = {filename.casefold()}
    for item in value["files"]:
        key = relative(item["path"]).casefold()
        require(key not in names, "Duplicate inventory entry")
        names.add(key)
        check_record(root, item)
    actual = [p.relative_to(root).as_posix().casefold() for p in tree(root)]
    require(len(actual) == len(names) and set(actual) == names, "Inventory file set differs")
    return value


def git(project, *arguments, input_data=None):
    result = subprocess.run(["git", "--literal-pathspecs", *arguments], cwd=project,
                            input=input_data, capture_output=True, check=False)
    require(result.returncode == 0, "Read-only Git command failed: " + arguments[0])
    return result.stdout


def commit_blobs(project, commit):
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Commit must be a full lowercase SHA-1")
    require(git(project, "rev-parse", "--verify", commit + "^{commit}").decode().strip() == commit,
            "Requested commit does not exist")
    entries = []
    for entry in git(project, "ls-tree", "-r", "-z", commit).split(b"\0"):
        if not entry:
            continue
        meta, name = entry.split(b"\t", 1)
        mode, kind, oid = meta.split()
        require(mode in {b"100644", b"100755"} and kind == b"blob", "Linked/submodule source is not supported")
        entries.append((relative(name.decode("utf-8")), oid))
    response = git(project, "cat-file", "--batch", input_data=b"".join(oid + b"\n" for _, oid in entries))
    blobs, cursor = {}, 0
    for name, oid in entries:
        end = response.index(b"\n", cursor)
        got_oid, kind, size = response[cursor:end].split()
        require(got_oid == oid and kind == b"blob", "Unexpected Git object response")
        size = int(size)
        blobs[name] = response[end + 1:end + 1 + size]
        cursor = end + 2 + size
    require(cursor == len(response), "Unexpected trailing Git object data")
    return blobs


def bind_source(project, commit, original):
    """Retain original hashes and distinguish Git-normalized LF from CRLF bytes."""
    require(original.get("schema_version") == 1 and original.get("algorithm") == "SHA256",
            "Unsupported source snapshot")
    blobs = commit_blobs(project, commit)
    names, present, deleted = set(), {}, []
    for item in original["files"]:
        name = relative(item["path"])
        require(name.casefold() not in names, "Duplicate source snapshot path")
        names.add(name.casefold())
        if "state" in item:
            require(item["state"] == "deleted_in_working_tree" and "sha256" not in item and name not in blobs,
                    "Deleted snapshot file exists in the release commit")
            deleted.append(name)
        else:
            require(type(item.get("size")) is int and item["size"] >= 0 and HEX.fullmatch(item.get("sha256", "")),
                    "Invalid source snapshot record")
            present[name] = item
    require(set(present) == set(blobs), "Release commit and packaging source file sets differ")
    paths = b"".join(name.encode("utf-8") + b"\0" for name in blobs)
    attributes = git(project, "check-attr", "--source=" + commit, "-z", "--stdin", "text", input_data=paths).split(b"\0")
    text_attributes = {attributes[i].decode(): attributes[i + 2].decode()
                       for i in range(0, len(attributes) - 1, 3)}
    bound = []
    for name, blob in blobs.items():
        item = copy.deepcopy(present[name])
        if len(blob) == item["size"] and sha(blob) == item["sha256"]:
            comparison = "exact_git_blob"
        else:
            require(text_attributes.get(name) in {"set", "auto"} and b"\0" not in blob and b"\r" not in blob,
                    "Snapshot differs from the release source: " + name)
            checkout_bytes = blob.replace(b"\n", b"\r\n")
            require(len(checkout_bytes) == item["size"] and sha(checkout_bytes) == item["sha256"],
                    "Snapshot differs from the release source: " + name)
            comparison = "git_text_lf_matches_packaging_crlf"
        item.update(git_blob_sha256=sha(blob), git_blob_size=len(blob), commit_comparison=comparison)
        bound.append(item)
    result = copy.deepcopy(original)
    result.update(git_head=commit, working_tree_dirty=False, files=bound,
                  packaging_git_head=original.get("git_head"),
                  packaging_working_tree_dirty=original.get("working_tree_dirty"),
                  packaging_deleted_paths=deleted,
                  scope="Exact release commit file set matched to original packaging bytes; per-file comparison records Git LF/CRLF normalization. Original packaging hashes retained. This is not proof of binary build provenance or reproducibility.")
    return result


def local_relative(value, root):
    """Cargo paths may have Windows spelling even when this script runs elsewhere."""
    path_type = PureWindowsPath if PureWindowsPath(root).drive else PurePosixPath
    try:
        result = path_type(value).relative_to(path_type(root)).as_posix()
    except ValueError as error:
        raise ValueError("Cargo local path escapes its recorded workspace") from error
    return relative(result)


def project_metadata(raw, target, roots):
    """Explicit dependency graph projection; never recursively rewrite evidence."""
    metadata = json.loads(raw)
    require(metadata.get("version") == 1, "Unsupported Cargo metadata version")
    workspace = metadata["workspace_root"]
    require(PureWindowsPath(workspace).is_absolute() or PurePosixPath(workspace).is_absolute(),
            "Expected a raw Cargo workspace absolute path")
    mapping, locations = {}, {}
    for package in metadata["packages"]:
        old = package["id"]
        require(old not in mapping, "Duplicate Cargo package ID")
        if package["source"] is None:
            location = local_relative(package["manifest_path"], workspace)
            require(location.endswith("/Cargo.toml") or location == "Cargo.toml", "Unexpected Cargo manifest path")
            mapping[old] = "workspace:" + location + "#" + package["name"] + "@" + package["version"]
            locations[old] = location
        else:
            require(package["source"].startswith(("registry+https://", "sparse+https://", "git+https://")) and
                    old.startswith(("registry+https://", "sparse+https://", "git+https://")),
                    "Unsupported Cargo source; review before publication")
            mapping[old] = old
    packages = []
    for package in metadata["packages"]:
        item = {key: package.get(key) for key in PACKAGE_FIELDS}
        item["id"] = mapping[package["id"]]
        if package["id"] in locations:
            item["workspace_manifest"] = locations[package["id"]]
        item["targets"] = [{key: target_record.get(key) for key in TARGET_FIELDS}
                           for target_record in package["targets"]]
        item["dependencies"] = []
        for dependency in package["dependencies"]:
            dep = {key: dependency.get(key) for key in DEPENDENCY_FIELDS}
            if "path" in dependency:
                dep["workspace_path"] = local_relative(dependency["path"], workspace)
            item["dependencies"].append(dep)
        packages.append(item)
    nodes = []
    for node in metadata["resolve"]["nodes"]:
        nodes.append({"id": mapping[node["id"]], "features": node["features"],
                      "dependencies": [mapping[item] for item in node["dependencies"]],
                      "deps": [{"name": edge["name"], "pkg": mapping[edge["pkg"]],
                                "dep_kinds": [{"kind": kind["kind"], "target": kind["target"]}
                                              for kind in edge["dep_kinds"]]} for edge in node["deps"]]})
    return {"schema_version": 1, "format": "avelin-public-cargo-graph", "target": target,
            "roots": roots, "raw_json_sha256": sha(raw),
            "scope": "Explicit projection of Cargo metadata. Upstream package IDs, declared features and dependency edges are retained; workspace IDs and paths are relative. Local cache/output/source paths, authors and arbitrary package metadata are excluded. Resolved graph may over-include dev/build dependencies; not a linker-level SBOM.",
            "workspace_members": [mapping[item] for item in metadata["workspace_members"]],
            "workspace_default_members": [mapping[item] for item in metadata["workspace_default_members"]],
            "packages": packages, "resolve": {"root": mapping.get(metadata["resolve"].get("root")), "nodes": nodes}}


def public_inventory(kit, inventory):
    result = copy.deepcopy(inventory)
    replacements, removed = {}, []
    require(result.get("schema_version") == 1 and result.get("cargo_metadata"), "Unsupported license inventory")
    targets = set()
    for entry in result["cargo_metadata"]:
        require(set(entry) == RAW_METADATA_KEYS, "Unrecognized raw metadata record fields")
        target = entry["target"]
        require(re.fullmatch(r"[a-z0-9_-]+", target) and target not in targets, "Invalid or duplicate metadata target")
        targets.add(target)
        expected = "metadata/" + target + ".json.gz"
        require(entry["path"] == expected, "Unexpected raw metadata path")
        compressed = child(kit / "third-party", expected).read_bytes()
        raw = gzip.decompress(compressed)
        require(sha(raw) == entry["json_sha256"], "Raw Cargo metadata digest differs")
        graph = project_metadata(raw, target, entry["roots"])
        name = "metadata/" + target + ".public.json"
        data = json_bytes(graph)
        replacements["third-party/" + name] = data
        removed.append({"path": "third-party/" + expected, "size": len(compressed), "sha256": sha(compressed),
                        "raw_json_sha256": sha(raw), "reason": "Raw Cargo metadata includes local workspace and cache paths; public dependency graph supplied instead."})
        entry.clear()
        entry.update(target=target, roots=graph["roots"], format=graph["format"], path=name,
                     json_sha256=sha(data), raw_json_sha256=sha(raw),
                     projection="Explicit version 1 public dependency graph; original raw evidence retained privately")
    return result, replacements, removed


class PrivacyScanner:
    """Fail closed for known private identities, user directories, and token forms.

    This is a deterministic publication guard, not a general secret detector.
    Findings report only relative names/categories, never the matched secret.
    """
    def __init__(self, denied=()):
        automatic = [os.environ.get("USERNAME", ""), os.environ.get("COMPUTERNAME", "")]
        automatic += [os.environ.get("USERPROFILE", ""), os.environ.get("HOME", "")]
        self.denied = {word.casefold() for word in (*automatic, *denied) if len(word) >= 4}
        self.patterns = {
            "user-directory": re.compile(r"(?i)(?:[a-z]:[\\/]+users[\\/]+|(?<![a-z0-9])/(?:home|users)/)[a-z0-9_.-]+[\\/]"),
            "access-token": re.compile(r"(?<![A-Za-z0-9_])(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{30,}|AKIA[0-9A-Z]{16}|sk-(?:proj-)?[A-Za-z0-9_-]{32,})"),
            "private-key": re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"),
            "url-credentials": re.compile(r"https?://[^\s/:@]+:[^\s/@]+@"),
        }

    def inspect(self, name, data):
        texts = [name, data.decode("utf-8", errors="ignore")]
        # Both byte alignments catch embedded UTF-16 strings inside PE images.
        for offset in (0, 1):
            texts.append(data[offset:].decode("utf-16-le", errors="ignore"))
            texts.append(data[offset:].decode("utf-16-be", errors="ignore"))
        for text in texts:
            folded = text.casefold()
            require(not any(word in folded for word in self.denied), "Private identifier found: " + name)
            for category, pattern in self.patterns.items():
                require(not pattern.search(text), category + " found: " + name)
        if name.endswith(".gz"):
            self.inspect(name.removesuffix(".gz"), gzip.decompress(data))


def export(args):
    kit, output, project = map(no_links, (args.kit, args.output, args.project_root))
    require(kit.is_dir() and project.is_dir(), "Input kit and project must exist")
    require(not output.exists(), "Output must be new; preserve any failed output")
    require(not output.is_relative_to(kit), "Output must be outside the original kit")
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", args.version), "Invalid release version")
    require(HEX.fullmatch(args.expected_kit_hashes_sha256), "Expected kit digest must be SHA-256")
    require(sha(child(kit, "HASHES.json").read_bytes()) == args.expected_kit_hashes_sha256,
            "Input kit HASHES digest differs from the approved candidate")
    original_hashes = exact_inventory(kit, "HASHES.json")
    exact_inventory(kit / "third-party", "FILES.json")
    build = read_json(child(kit, "BUILD-INFO.json"))
    require(build.get("schema_version") == 1 and build.get("engine") == "rust" and
            build.get("platform") == "x86_64-pc-windows-msvc" and build.get("signed") is False,
            "Unsupported developer kit")
    require(build.get("cli_version") == "avelin-cli " + args.version, "Release version and CLI version differ")
    original_source_bytes = child(kit, "SOURCE-SNAPSHOT.json").read_bytes()
    require(build.get("source_snapshot_sha256") == sha(original_source_bytes), "Build source digest differs")
    source = bind_source(project, args.commit, json.loads(original_source_bytes))
    old_inventory = read_json(child(kit, "third-party/inventory.json"))
    require(build["license_evidence"]["inventory_sha256"] == sha(child(kit, "third-party/inventory.json").read_bytes()),
            "Build license inventory digest differs")
    inventory, replacements, removed = public_inventory(kit, old_inventory)
    replacements["third-party/inventory.json"] = json_bytes(inventory)
    replacements["SOURCE-SNAPSHOT.json"] = json_bytes(source)
    readme = child(kit, "README.md").read_bytes()
    old_notice = LOCAL_README_NOTICE.encode("utf-8")
    require(readme.startswith("# Avelin Rust Windows x64 开发者工具包".encode("utf-8")) and
            readme.count(old_notice) == 1, "Unrecognized developer-kit README; review its public notice")
    replacements["README.md"] = readme.replace(old_notice, PUBLIC_README_NOTICE.encode("utf-8"))
    build = copy.deepcopy(build)
    build.update(git_head=args.commit, source_snapshot_sha256=sha(replacements["SOURCE-SNAPSHOT.json"]))
    build["license_evidence"].update(inventory_sha256=sha(replacements["third-party/inventory.json"]),
                                      raw_local_metadata_preserved=False,
                                      public_metadata_projection="Explicit dependency graph with relative workspace paths")
    build["public_release"] = {"version": args.version, "tag": "v" + args.version, "source_commit": args.commit,
                               "original_candidate_id": build["candidate_id"],
                               "original_kit_hashes_sha256": args.expected_kit_hashes_sha256,
                               "original_source_snapshot_sha256": sha(original_source_bytes),
                               "executables_rebuilt_by_exporter": False,
                               "scope": "Publication export only. Source snapshot is bound to the release commit. Existing binaries and acceptance claims are not modified or independently re-established by this script."}
    replacements["BUILD-INFO.json"] = json_bytes(build)
    omitted = {item["path"] for item in removed}
    regenerated = {"HASHES.json", "third-party/FILES.json"}
    scanner = PrivacyScanner(args.deny_text)
    delivered = output / KIT_NAME
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir()
    delivered.mkdir()
    preserved = []
    for item in original_hashes["files"]:
        name = item["path"]
        if name in omitted or name in regenerated or name in replacements:
            continue
        data = check_record(kit, item)
        scanner.inspect(name, data)
        write_new(child(delivered, name), data)
        preserved.append(item)
    for name, data in replacements.items():
        scanner.inspect(name, data)
        write_new(child(delivered, name), data)
    exported_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    release = {"schema_version": 1, "version": args.version, "tag": "v" + args.version,
               "source_commit": args.commit, "exported_at_utc": exported_at,
               "original_kit_hashes_sha256": args.expected_kit_hashes_sha256,
               "removed_raw_metadata": removed,
               "replaced_files": sorted(replacements),
               "regenerated_inventories": sorted(regenerated),
               "preserved_files": preserved,
               "preservation": "Every original file except explicitly listed metadata/inventories is copied byte-for-byte. Executables, third-party licenses, NOTICE files, declarations and supplied source files are never rewritten.",
               "privacy": "Scanned UTF-8/UTF-16 bytes for configured private identifiers, user directories and recognizable credential forms. Original raw Cargo paths are excluded by explicit graph projection; not a guarantee against all possible secrets.",
               "build_provenance": "The original packaging source file set is matched to the release commit, allowing explicitly recorded Git LF/CRLF normalization. This exporter does not compile binaries or prove reproducibility. Existing kit README and validation limitations remain unchanged."}
    data = json_bytes(release)
    scanner.inspect("PUBLIC-RELEASE.json", data)
    write_new(delivered / "PUBLIC-RELEASE.json", data)
    license_files = {"schema_version": 1, "algorithm": "SHA256",
                     "files": records(delivered / "third-party", {"FILES.json"})}
    write_new(delivered / "third-party/FILES.json", json_bytes(license_files))
    hashes = {"schema_version": 1, "algorithm": "SHA256",
              "scope": "Every delivered regular file except HASHES.json itself",
              "files": records(delivered, {"HASHES.json"})}
    sealed_hashes = json_bytes(hashes)
    write_new(delivered / "HASHES.json", sealed_hashes)
    sealed_records = {item["path"]: item for item in hashes["files"]}
    sealed_records["HASHES.json"] = {"size": len(sealed_hashes), "sha256": sha(sealed_hashes)}
    exact_inventory(delivered, "HASHES.json")
    exact_inventory(delivered / "third-party", "FILES.json")
    # Detect a concurrently edited input or commit-object mismatch before sealing.
    require(sha(child(kit, "HASHES.json").read_bytes()) == args.expected_kit_hashes_sha256,
            "Input inventory changed while exporting")
    exact_inventory(kit, "HASHES.json")
    zip_name = "Avelin-" + args.version + "-windows-x64.zip"
    archive_path = output / zip_name
    with zipfile.ZipFile(archive_path, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted(tree(delivered)):
            data = path.read_bytes()
            name = path.relative_to(delivered).as_posix()
            require(name in sealed_records and len(data) == sealed_records[name]["size"] and
                    sha(data) == sealed_records[name]["sha256"], "Delivered content changed after sealing: " + name)
            scanner.inspect(name, data)
            info = zipfile.ZipInfo(KIT_NAME + "/" + name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, data, compresslevel=9)
    expected_names = {KIT_NAME + "/" + p.relative_to(delivered).as_posix() for p in tree(delivered)}
    with zipfile.ZipFile(archive_path) as archive:
        require(len(archive.namelist()) == len(expected_names) and set(archive.namelist()) == expected_names,
                "Archive file set differs")
        for info in archive.infolist():
            name = info.filename[len(KIT_NAME) + 1:]
            data = archive.read(info)
            require(name in sealed_records and len(data) == sealed_records[name]["size"] and
                    sha(data) == sealed_records[name]["sha256"], "Archive read-back differs from sealed content")
    require(sha((delivered / "HASHES.json").read_bytes()) == sha(sealed_hashes), "Sealed inventory changed")
    exact_inventory(delivered, "HASHES.json")
    archive_hash = sha(archive_path.read_bytes())
    write_new(output / (zip_name + ".sha256"), (archive_hash + "  " + zip_name + "\n").encode())
    manifest_hash = sha(sealed_hashes)
    write_new(output / "Avelin-Rust-DeveloperKit.HASHES.sha256",
              (manifest_hash + "  " + KIT_NAME + "/HASHES.json\n").encode())
    result = {"schema_version": 1, "status": "PASSED", "version": args.version, "source_commit": args.commit,
              "archive": zip_name, "archive_sha256": archive_hash, "archive_size": archive_path.stat().st_size,
              "hashes_sha256": manifest_hash, "delivered_files": len(hashes["files"]) + 1,
              "original_kit_hashes_sha256": args.expected_kit_hashes_sha256,
              "preserved_files": len(preserved), "removed_raw_metadata_files": len(removed),
              "compilation_executed": False, "installation_executed": False, "publication_executed": False}
    write_new(output / "public-export-verification.json", json_bytes(result))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kit", type=Path, required=True)
    parser.add_argument("--expected-kit-hashes-sha256", required=True)
    parser.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--commit", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--deny-text", action="append", default=[], help="Additional private identifier; findings do not print its value")
    args = parser.parse_args()
    print(json.dumps(export(args), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
