#!/usr/bin/env python3
"""Collect locked local licensing evidence; no build, install, or network access.

Python 3.11+ and the standard library suffice. Cargo/rustc must already be on
PATH, and --locked --offline metadata must resolve from the local Cargo cache.
This is an evidence collector, not a license compatibility or legal approval
engine. Upstream supplements are separately reviewed, pinned repository inputs.
"""
from __future__ import annotations

import argparse
import collections
import datetime
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tomllib

LICENSE_NAME = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|unlicense|ofl)(?:[._-].*)?$", re.I)
CODE_EXTENSIONS = {".rs", ".c", ".cpp", ".h", ".nsh", ".nsi", ".py", ".js"}
NATIVE_EXTENSIONS = {".dll", ".lib", ".a", ".so", ".dylib", ".exe"}
FONT_EXTENSIONS = {".ttf", ".otf", ".woff", ".woff2"}
ROOTS = {"x86_64-pc-windows-msvc": ["avelin-app", "avelin-cli"],
         "i686-pc-windows-msvc": ["avelin-nsis-bridge"]}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def reject_link(path: Path):
    """Use lstat attributes: Path.is_junction is unavailable on Python 3.11."""
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    if (stat.S_ISLNK(metadata.st_mode) or
            getattr(metadata, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT):
        raise RuntimeError(f"Linked evidence path is not accepted: {path}")


def no_reparse(path: Path) -> Path:
    # Check the original path before resolve() could hide a linked root/ancestor.
    absolute = path.absolute()
    for component in reversed((absolute, *absolute.parents)):
        reject_link(component)
    return absolute


def file_hash(path: Path) -> str:
    no_reparse(path)
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_json(path: Path):
    no_reparse(path)
    return json.loads(path.read_text(encoding="utf-8-sig"))


def save(path: Path, value):
    no_reparse(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def checked_relative(value: str) -> str:
    normalized = value.replace("\\", "/")
    p = PurePosixPath(normalized)
    # Inspect raw parts before PurePath normalizes away empty/dot components;
    # trailing dots/spaces can also alias Windows paths outside this spelling.
    if p.is_absolute() or ":" in value or any(
            x in {"", ".", ".."} or x.endswith((".", " ")) for x in normalized.split("/")):
        raise ValueError(f"Unsafe relative path: {value}")
    return p.as_posix()


def walk_files(root: Path):
    """No symlink/junction traversal; fail instead of silently dropping evidence."""
    no_reparse(root)
    for directory, names, filenames in os.walk(root, followlinks=False):
        names.sort()
        for name in names + filenames:
            p = Path(directory) / name
            reject_link(p)
        for name in sorted(filenames):
            yield Path(directory) / name


def license_candidate(path: Path, root: Path) -> bool:
    rel = path.relative_to(root)
    return (LICENSE_NAME.match(path.name) is not None or
            any(p.lower() in {"licenses", "licences"} for p in rel.parts[:-1])) and path.suffix.lower() not in CODE_EXTENSIONS


def copy_evidence(source: Path, output: Path, relative: str, **extra):
    relative = checked_relative(relative)
    target = output / relative
    no_reparse(source)
    no_reparse(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    data = source.read_bytes()
    if target.exists() and target.read_bytes() != data:
        raise RuntimeError(f"Conflicting output: {relative}")
    target.write_bytes(data)
    return {"path": relative, "size": len(data), "sha256": digest(data), **extra}


def command(args, cwd: Path) -> bytes:
    result = subprocess.run([str(x) for x in args], cwd=cwd, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"Read-only command failed ({result.returncode}): {' '.join(map(str, args))}\n"
                           + result.stderr.decode("utf-8", errors="replace"))
    return result.stdout


def dependency_roles(metadata, root_names):
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    queue = collections.deque((p["id"], "runtime") for p in packages.values()
                              if p["source"] is None and p["name"] in root_names)
    visited = set()
    roles = collections.defaultdict(set)
    while queue:
        package_id, role = queue.popleft()
        if (package_id, role) in visited:
            continue
        visited.add((package_id, role))
        roles[package_id].add(role)
        for edge in nodes.get(package_id, {}).get("deps", []):
            dependency = packages[edge["pkg"]]
            is_macro = any("proc-macro" in t["kind"] for t in dependency["targets"])
            for kind in edge["dep_kinds"]:
                next_role = role
                if kind["kind"] == "dev":
                    next_role = "dev"
                elif role != "dev" and (kind["kind"] == "build" or is_macro):
                    next_role = "build"
                queue.append((edge["pkg"], next_role))
    return roles


def collect(args):
    project = no_reparse(args.project_root)
    output = no_reparse(args.output)
    nsis = no_reparse(args.nsis_root)
    if output.exists() and any(output.iterdir()):
        raise RuntimeError("Output must be new or empty; preserve the previous evidence and choose a new directory")
    output.mkdir(parents=True, exist_ok=True)
    lock_path = project / "Cargo.lock"
    before_lock = file_hash(lock_path)
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    lock_map = {(p["name"], p["version"], p.get("source")): p for p in lock["package"]}
    package_map, roles_by_target, metadata_records = {}, {}, []
    for target, roots in ROOTS.items():
        if target == "i686-pc-windows-msvc":
            # Full-workspace i686 metadata pulls in an unshipped 32-bit GUI.
            # Bind the real bridge-only target with Cargo's filtered tree and
            # require exact agreement with its metadata dependency subgraph.
            invocation = ["cargo", "tree", "--locked", "--offline", "-p", "avelin-nsis-bridge",
                          "--target", target, "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
            raw = command(invocation, project)
            actual = {tuple(match.groups()) for line in raw.decode().splitlines()
                      if (match := re.match(r"^([\w-]+) v([^\s]+)", line))}
            roles = dependency_roles(metadata, roots)
            by_id = {p["id"]: p for p in metadata["packages"]}
            expected = {(by_id[p]["name"], by_id[p]["version"]) for p, kinds in roles.items()
                        if kinds & {"runtime", "build"}}
            if actual - expected:
                raise RuntimeError(f"i686 bridge has inputs absent from collected metadata: {actual - expected}")
            # Workspace feature unification can add serde_json/indexmap or
            # digest features to the GUI graph; Cargo tree is authoritative
            # for the delivered bridge. Record those over-included nodes.
            roles_by_target[target] = {p: kinds for p, kinds in roles.items()
                                       if (by_id[p]["name"], by_id[p]["version"]) in actual
                                       and kinds & {"runtime", "build"}}
            evidence_path = output / "metadata" / (target + "-bridge-tree.txt.gz")
            evidence_path.write_bytes(gzip.compress(raw, mtime=0))
            metadata_records.append({"target": target, "roots": roots, "arguments": invocation,
                                     "raw_sha256": digest(raw), "path": evidence_path.relative_to(output).as_posix(),
                                     "excluded_workspace_feature_unification": sorted(expected - actual),
                                     "method": "Every actual normal/build bridge Cargo tree package bound to metadata; workspace-feature-only nodes excluded; no unshipped 32-bit GUI resolution"})
            continue
        invocation = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
                      "--filter-platform", target, "--features", "avelin-app/gui"]
        raw = command(invocation, project)
        metadata = json.loads(raw)
        metadata_path = output / "metadata" / (target + ".json.gz")
        metadata_path.parent.mkdir(parents=True, exist_ok=True)
        # Exact raw evidence, compressed without wall-clock gzip timestamps.
        metadata_path.write_bytes(gzip.compress(raw, mtime=0))
        metadata_records.append({"target": target, "roots": roots, "arguments": invocation,
                                 "json_sha256": digest(raw), "path": metadata_path.relative_to(output).as_posix()})
        roles_by_target[target] = dependency_roles(metadata, roots)
        for package in metadata["packages"]:
            package_map[package["id"]] = package

    inputs = project / "packaging" / "third-party" / "inputs"
    supplements_path = inputs / "supplements.json"
    supplements = read_json(supplements_path) if supplements_path.exists() else {"crates": [], "archives": []}
    supplement_input = (copy_evidence(supplements_path, output, "metadata/supplements.json",
                                      origin="Reviewed offline generator input; upstream URLs and pinned revisions retained")
                        if supplements_path.exists() else None)
    supplement_map = {(p["package"], p["version"]): p for p in supplements["crates"]}
    unknowns, crates, assets, native, source_records = [], [], [], [], []
    resolved_keys = set()
    for package in sorted(package_map.values(), key=lambda x: (x["name"], x["version"], x["id"])):
        if package["source"] is None:
            continue
        key = (package["name"], package["version"], package["source"])
        if key not in lock_map:
            raise RuntimeError(f"Metadata package is not locked: {key}")
        resolved_keys.add(key)
        root = Path(package["manifest_path"]).parent
        package_slug = package["name"] + "-" + package["version"]
        files = list(walk_files(root))
        texts = []
        for path in files:
            relative = path.relative_to(root).as_posix()
            if license_candidate(path, root) or (package.get("license_file") and path == root / package["license_file"]):
                texts.append(copy_evidence(path, output, f"licenses/crates/{package_slug}/{relative}",
                                           origin="locked local crate", original_path=relative))
            if path.suffix.lower() in FONT_EXTENSIONS:
                assets.append({"crate": package_slug, "path": relative, "size": path.stat().st_size,
                               "sha256": file_hash(path), "kind": "font",
                               "scope": "test fixture; not embedded by Avelin" if "tests" in path.relative_to(root).parts else "requires embed-use review"})
            if path.suffix.lower() in NATIVE_EXTENSIONS:
                native.append({"crate": package_slug, "path": relative, "size": path.stat().st_size,
                               "sha256": file_hash(path), "scope": "local crate native input; see target/role records"})
        vcs_path = root / ".cargo_vcs_info.json"
        vcs = read_json(vcs_path) if vcs_path.exists() else None
        supplement = supplement_map.get((package["name"], package["version"]))
        declarations, binding_evidence = [], []
        if supplement:
            if supplement.get("vcs_commit") and (not vcs or vcs.get("git", {}).get("sha1") != supplement["vcs_commit"]):
                raise RuntimeError(f"Supplement no longer matches crate commit: {package_slug}")
            binding = supplement.get("version_binding")
            if binding == "License-only upstream follow-up; source tree unchanged":
                comparison = supplement["comparison"]
                proof_path = inputs / checked_relative(comparison["path"])
                if file_hash(proof_path) != comparison["sha256"]:
                    raise RuntimeError(f"Upstream comparison digest mismatch: {package_slug}")
                proof = read_json(proof_path)
                if comparison["from_commit"] != supplement["vcs_commit"] or comparison["to_commit"] != supplement["license_commit"]:
                    raise RuntimeError(f"Upstream comparison identity mismatch: {package_slug}")
                if comparison["provider"] == "github":
                    valid = (proof["base_commit"]["sha"] == comparison["from_commit"]
                             and proof["merge_base_commit"]["sha"] == comparison["from_commit"]
                             and proof["commits"][-1]["sha"] == comparison["to_commit"]
                             and [(f["filename"], f["status"]) for f in proof["files"]] == [("LICENSE", "added")])
                elif comparison["provider"] == "gitlab":
                    valid = (not proof.get("compare_timeout") and proof["commit"]["id"] == comparison["to_commit"]
                             and [(f["old_path"], f["new_path"], f["new_file"], f["deleted_file"]) for f in proof["diffs"]]
                             == [("LICENSE", "LICENSE", True, False)])
                else:
                    valid = False
                if not valid or any(r["commit"] != supplement["license_commit"] for r in supplement.get("files", [])):
                    raise RuntimeError(f"Upstream follow-up is not a LICENSE-only addition: {package_slug}")
                binding_evidence.append(copy_evidence(proof_path, output, f"metadata/upstream/{package_slug}-comparison.json",
                                                      origin_url=comparison["url"], from_commit=comparison["from_commit"],
                                                      to_commit=comparison["to_commit"]))
            for record in supplement.get("files", []):
                source = inputs / checked_relative(record["path"])
                if file_hash(source) != record["sha256"]:
                    raise RuntimeError(f"Supplement digest mismatch: {source}")
                texts.append(copy_evidence(source, output, f"licenses/upstream/{package_slug}/{Path(record['path']).name}",
                                           origin="pinned upstream supplement", url=record["url"],
                                           version_binding=supplement["version_binding"]))
            if supplement.get("preserve_source_and_declarations"):
                for name in ["README.md", "Cargo.toml", "Cargo.toml.orig"]:
                    path = root / name
                    if path.is_file():
                        declarations.append(copy_evidence(path, output, f"declarations/{package_slug}/{name}",
                                                          origin="Unchanged published crate; SPDX/license declaration, not standalone full license text"))
                unknowns.append({"component": package_slug, "issue": supplement["unresolved_reason"],
                                 "declarations": declarations, "upstream_review": supplement.get("upstream_review")})
            elif binding not in {"Cargo .cargo_vcs_info.json exact commit", "License-only upstream follow-up; source tree unchanged"}:
                unknowns.append({"component": package_slug, "issue": "Supplement is not bound to this published crate commit", "detail": supplement.get("version_binding")})
        if not texts and not declarations:
            unknowns.append({"component": package_slug, "issue": "No actual license/NOTICE text found; SPDX declaration alone is not treated as clearance"})
        # Source delivery for the actual weak-copyleft crate; do not equate this
        # collection policy with a legal finding that the crate is linked.
        if (package.get("license") and "MPL-2.0" in package["license"]) or (supplement and supplement.get("preserve_source_and_declarations")):
            copied = [copy_evidence(p, output, f"sources/crates/{package_slug}/{p.relative_to(root).as_posix()}")
                      for p in files if p.name not in {".cargo-ok"}]
            reason = "Complete unchanged local crate source supplied with MPL notice" if "MPL-2.0" in (package.get("license") or "") else "Complete unchanged published crate source preserves actual license declarations where upstream omits standalone full text"
            source_records.append({"component": package_slug, "reason": reason, "files": copied})
        role_record = {target: sorted(roles.get(package["id"], {"resolved-but-not-reachable-from-delivered-roots"}))
                       for target, roles in roles_by_target.items()}
        crates.append({"name": package["name"], "version": package["version"], "cargo_id": package["id"],
                       "source": package["source"], "repository": package.get("repository"), "homepage": package.get("homepage"),
                       "crate_checksum": lock_map[key].get("checksum"), "declared_spdx": package.get("license"),
                       "declared_license_file": package.get("license_file"), "vcs": vcs,
                       "target_roles": role_record, "license_texts": texts, "license_declarations": declarations,
                       "license_binding_evidence": binding_evidence,
                       "scope_note": "Cargo resolved candidate set; feature unification and dev edges can over-include. Not a linker-level SBOM."})
        if package["name"] == "gpui-kit-assets":
            defaults = (root / "default-icons.txt").read_text().splitlines()
            icons = [root / "assets" / name for name in defaults if name]
            assets.append({"component": package_slug + "/Lucide-Feather", "kind": "embedded-svg-icons",
                           "declared_licenses": "ISC AND MIT (Feather-derived icons); see LICENSE-LUCIDE",
                           "source": "Avelin registers gpui_kit::assets::Assets; crate build.rs uses default-icons.txt",
                           "selection": copy_evidence(root / "default-icons.txt", output, f"assets/{package_slug}/default-icons.txt"),
                           "license": [x for x in texts if "LUCIDE" in x["path"]],
                           "files": [{"path": p.relative_to(root).as_posix(), "size": p.stat().st_size, "sha256": file_hash(p)} for p in icons]})

    nsis_version = command([nsis / "makensis.exe", "/VERSION"], project).decode().strip()
    if nsis_version not in {"v3.12", "3.12"}:
        raise RuntimeError(f"Expected reviewed NSIS 3.12, got {nsis_version}")
    nsis_files = []
    nsis_texts = []
    for path in walk_files(nsis):
        relative = path.relative_to(nsis).as_posix()
        nsis_files.append({"path": relative, "size": path.stat().st_size, "sha256": file_hash(path)})
        if license_candidate(path, nsis):
            nsis_texts.append(copy_evidence(path, output, f"licenses/nsis/{relative}", origin="NSIS 3.12 local distribution"))
    nsis_record = {"version": nsis_version, "declared_license_summary": "Zlib; bzip2; CPL-1.0 with NSIS LZMA linking exception; other notices where specified",
                   "official_license": "https://nsis.sourceforge.io/Docs/AppendixI.html", "files": nsis_files,
                   "license_texts": nsis_texts, "portable_policy": "Inventory the local NSIS tree; ship COPYING, Docs, Contrib, Examples, Include, Plugins, Stubs, Bin, makensis.exe and nsisconf.nsh. Exclude the root UI launchers, compiled help and machine-specific uninstaller listed below.",
                   "required_by_builder": ["makensis.exe", "Bin/makensis.exe", "Bin/zlib1.dll", "Stubs", "Include", "Plugins", "Contrib", "nsisconf.nsh", "COPYING"],
                   "optional_but_retained": ["Docs", "Examples", "Bin/zip2exe.exe", "Bin/MakeLangId.exe", "Bin/GenPat.exe"],
                   "exclude_from_redistributed_toolkit": ["NSIS.exe", "makensisw.exe", "NSIS.chm", "uninst-nsis.exe"]}
    save(output / "nsis-inventory.json", nsis_record)
    for archive in supplements.get("archives", []):
        path = inputs / checked_relative(archive["path"])
        if file_hash(path) != archive["sha256"]:
            raise RuntimeError(f"Source archive mismatch: {path}")
        copied = copy_evidence(path, output, "sources/" + path.name, origin_url=archive["url"], component=archive["component"])
        source_records.append(copied)
        if archive["component"] == "NSIS 3.12":
            with tarfile.open(path, "r:bz2") as tar:
                members = tar.getmembers()
                copying = next(m for m in members if m.name.endswith("/COPYING") and m.name.count("/") == 1)
                content = tar.extractfile(copying).read()
                if content.replace(b"\r\n", b"\n") != (nsis / "COPYING").read_bytes().replace(b"\r\n", b"\n"):
                    raise RuntimeError("Official source COPYING differs from local NSIS license")
                source_records.append({"component": "NSIS 3.12 source contents", "archive": copied["path"],
                                       "copying_matches_local": True,
                                       "members": [{"path": m.name, "size": m.size, "type": str(m.type)} for m in members]})
    if not any(x.get("component") == "NSIS 3.12" for x in source_records):
        unknowns.append({"component": "NSIS LZMA", "issue": "Matching NSIS 3.12 source archive not included; distribution source obligations remain unresolved"})

    rust_version = command(["rustc", "-Vv"], project).decode().strip()
    sysroot = Path(command(["rustc", "--print", "sysroot"], project).decode().strip())
    rust_texts = []
    for name in ["COPYRIGHT-library.html"]:
        path = sysroot / "share/doc/rust" / name
        if path.is_file():
            rust_texts.append(copy_evidence(path, output, "licenses/rust-standard-library/" + name,
                                            origin="Local Rust distribution; full rustc version recorded"))
        else:
            unknowns.append({"component": "Rust standard library", "issue": "Local COPYRIGHT-library.html missing"})
    rust_record = {"rustc": rust_version, "license_texts": rust_texts,
                   "scope": "Compiled Rust standard-library attribution; compiler executable itself is not shipped in the DevKit"}
    native.append({"component": "MSVC static CRT / Windows SDK", "scope": ".cargo/config.toml enables +crt-static on both Windows targets; OS import libraries also appear in crate inventory",
                   "distribution": "No Visual Studio compiler/SDK/standalone system DLLs are copied by this collector",
                   "status": "Redistribution rights derive from the builder's applicable Microsoft terms, not Cargo SPDX declarations"})
    unknowns.append({"component": "Microsoft build/runtime inputs", "issue": "Applicable Visual Studio/SDK redistribution entitlement is not certified by this collector; verify publisher entitlement before public distribution"})
    assets.append({"component": "Microsoft YaHei UI / Georgia / Windows fallback fonts", "kind": "system-font-reference",
                   "scope": "Selected by family name in view.rs; font bytes are not copied by the application or collector. No font redistribution license granted."})
    local_vendor = []
    for folder in [project / "vendor", project / "packaging/vendor"]:
        if folder.exists():
            local_vendor += [{"path": p.relative_to(project).as_posix(), "size": p.stat().st_size, "sha256": file_hash(p)} for p in walk_files(folder)]
    if local_vendor:
        unknowns.append({"component": "workspace vendor inputs", "issue": "Additional vendor tree found; records require explicit review", "files": local_vendor})
    else:
        native.append({"component": "workspace vendor trees", "scope": "No vendor/ or packaging/vendor/ tree exists; registry native libraries and full NSIS directory inventoried separately"})

    excluded = [{"name": p["name"], "version": p["version"], "source": p.get("source"), "checksum": p.get("checksum"),
                 "scope": "Locked but absent from both filtered Windows metadata graphs; not asserted to be delivered"}
                for p in lock["package"] if p.get("source") and (p["name"], p["version"], p.get("source")) not in resolved_keys]
    project_metadata = tomllib.loads((project / "Cargo.toml").read_text(encoding="utf-8-sig"))["workspace"]["package"]
    project_spdx = project_metadata.get("license")
    project_license = {"spdx": project_spdx, "status": "DECLARED" if project_spdx else "NOT_SELECTED",
                       "scope": "Avelin original content only; third-party files retain their own terms",
                       "license_texts": []}
    if project_spdx:
        project_license["license_texts"].append(copy_evidence(project / "LICENSE", output, "licenses/avelin/LICENSE",
                                                           origin="Project root LICENSE; workspace.package.license declaration"))
    inventory = {"schema_version": 1, "status": "COLLECTED_WITH_DECLARED_LIMITATIONS", "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                 "cargo_lock_sha256": before_lock, "cargo_metadata": metadata_records, "supplement_input": supplement_input,
                 "project_license": project_license,
                 "crates": crates, "locked_not_in_windows_graphs": excluded, "assets": assets, "native_inputs": native,
                 "rust_standard_library": rust_record, "nsis_inventory": "nsis-inventory.json", "sources": source_records,
                 "unknowns": unknowns, "limitations": ["No compatibility/legal certification", "Cargo metadata over-includes build/dev/feature-unified dependencies", "Source/header-only copyright notices are retained only for the components whose complete sources are supplied and recorded in sources; other crate headers are not exhaustively collected or classified by filename scanning", "No build, installation or network access performed by this generator"]}
    save(output / "inventory.json", inventory)
    save(output / "unknowns.json", unknowns)
    notice = ["# Third-party notices / 第三方许可材料", "", "This bundle records local locked-source evidence. It is not legal certification or a license grant for Avelin.",
              f"Avelin original content: {project_spdx or 'not selected'}; see the recorded project license text. Third-party terms remain separate.", "",
              f"Cargo.lock SHA256: `{before_lock}`", f"Windows metadata crates: {len(crates)}; other locked packages: {len(excluded)}.", "",
              "Actual license/NOTICE files are copied unchanged into `licenses/`; source locations, versions, declared SPDX expressions and hashes appear in `inventory.json`.",
              "The declared SPDX expression is reported verbatim; an OR expression is not silently replaced by a license selected by this collector.",
              "", "## Embedded icons and fonts", "", "GPUI Kit Assets bundles Lucide (ISC) and Feather-derived (MIT) SVG icons; preserve LICENSE-LUCIDE. Windows fonts are referenced by name, not redistributed.",
              "", "## NSIS and native inputs", "", "NSIS 3.12 COPYING includes zlib/libpng, bzip2, CPL-1.0 and the LZMA linking exception. The unchanged official source archive is included when recorded in inventory.json. Other component notices remain in the portable compiler tree.",
              "Rust standard-library attribution is in licenses/rust-standard-library. Microsoft compiler/SDK/runtime rights remain governed by the publisher's applicable Microsoft terms.",
              "", "## Declared limitations", ""]
    notice += [f"- **{item['component']}**: {item['issue']}" for item in unknowns]
    notice += ["", "## Crates", "", "| Crate | Declared SPDX | Collected texts |", "|---|---|---|"]
    notice += [f"| {p['name']} {p['version']} | {p['declared_spdx'] or 'unknown'} | {len(p['license_texts'])} |" for p in crates]
    (output / "NOTICE.md").write_text("\n".join(notice) + "\n", encoding="utf-8")
    if file_hash(lock_path) != before_lock:
        raise RuntimeError("Cargo.lock changed during read-only collection")
    files = [{"path": p.relative_to(output).as_posix(), "size": p.stat().st_size, "sha256": file_hash(p)}
             for p in sorted(walk_files(output)) if p.name != "FILES.json"]
    save(output / "FILES.json", {"schema_version": 1, "algorithm": "SHA256", "files": files})
    verified = verify(output)
    return {**verified, "crates": len(crates), "locked_not_in_windows_graphs": len(excluded), "unknowns": len(unknowns), "nsis_files": len(nsis_files), "output": str(output)}


def verify(output: Path):
    output = no_reparse(output)
    manifest = read_json(output / "FILES.json")
    if manifest.get("schema_version") != 1 or not manifest.get("files"):
        raise RuntimeError("Unsupported or empty FILES.json")
    seen = set()
    for entry in manifest["files"]:
        relative = checked_relative(entry["path"])
        if relative.casefold() in seen:
            raise RuntimeError(f"Repeated path: {relative}")
        seen.add(relative.casefold())
        path = output / relative
        no_reparse(path)
        if not path.is_file() or path.stat().st_size != entry["size"] or file_hash(path) != entry["sha256"]:
            raise RuntimeError(f"Evidence digest mismatch: {relative}")
    return {"status": "PASS", "scope": "Listed evidence bytes and hashes only; not legal/distribution clearance", "files": len(seen)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project-root", type=Path, default=Path(__file__).absolute().parents[1])
    parser.add_argument("--nsis-root", type=Path, default=Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "NSIS")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--verify", type=Path, help="Only verify the listed bytes of an existing bundle")
    args = parser.parse_args()
    if args.verify:
        result = verify(args.verify)
    else:
        if not args.output:
            parser.error("--output is required when collecting")
        result = collect(args)
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
