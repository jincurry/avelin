"""Check local draft schemas, legacy fixtures and optional saved M2/M3 evidence.

This is read-only. It neither runs an installer nor establishes GUI acceptance,
rollback reliability, current disk state or a published compatibility contract.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path

from jsonschema import Draft202012Validator
from referencing import Registry, Resource


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def read(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", type=Path, help="Historical completed M3 evidence directory")
    parser.add_argument("--m2-run", type=Path, help="Frozen historical M2 evidence directory")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2] / "docs" / "schemas"
    base = "https://avelin.dev/schemas/m1/"
    registry = Registry()  # No network retrieval: all references are registered below.
    for name in ("model", "protocol", "presentation"):
        schema = read(root / f"{name}.schema.json")
        Draft202012Validator.check_schema(schema)
        registry = registry.with_resource(base + f"{name}.schema.json", Resource.from_contents(schema))
    positives, negatives, evidence = [], [], []

    def validator(name, definition=None):
        ref = base + f"{name}.schema.json" + (f"#/$defs/{definition}" if definition else "")
        return Draft202012Validator({"$ref": ref}, registry=registry)

    def check(label, value, name, definition=None, array=False):
        for item in value if array else [value]:
            validator(name, definition).validate(item)
        positives.append(label)
        return value

    def reject(label, value, name, definition=None):
        require(not validator(name, definition).is_valid(value), f"Unexpected accepted negative: {label}")
        negatives.append(label)

    def example(filename, name, definition=None, array=False):
        return check("legacy/" + filename, read(root / filename), name, definition, array)

    manifest = example("examples/manifest.json", "model", "Manifest")
    receipt = example("examples/receipt.json", "model", "Receipt")
    plan = example("examples/install-plan.json", "model", "InstallPlan")
    command = example("examples/command.json", "protocol", "TaskCommand")
    example("examples/snapshot.json", "protocol", "TaskSnapshot")
    example("examples/events.json", "protocol", "BackendEvent", array=True)
    for result in ("succeeded", "cancelled", "failed", "unknown"):
        example(f"examples/replay-{result}.json", "protocol", "BackendEvent", array=True)
    presentation = example("presentation.example.json", "presentation")
    check("legacy/manifest-with-presentation", dict(manifest, presentation=presentation), "model", "Manifest")

    record = {"kind": "desktop", "path": r"C:\Users\Example\Desktop\dev.example.app.lnk",
              "target": r"C:\Users\Example\App\app.exe", "sha256": "a" * 64}
    check("m2/shortcut-defaults", dict(manifest, desktop_shortcut=True, start_menu_shortcut=True), "model", "Manifest")
    omitted = copy.deepcopy(manifest)
    omitted.pop("desktop_shortcut", None)
    omitted.pop("start_menu_shortcut", None)
    check("m2/omitted-shortcut-defaults", omitted, "model", "Manifest")
    check("m2/actual-choices", dict(plan, desktop_shortcut=False, start_menu_shortcut=False), "model", "InstallPlan")
    check("m2/shortcut-record", dict(receipt, shortcuts=[record]), "model", "Receipt")
    check("m2/launch-command", {"command": "launch_installed"}, "protocol", "TaskCommand")
    check("m2/launch-result", {"response": "launch_result", "succeeded": True, "detail": ""}, "protocol", "TaskResponse")

    before = copy.deepcopy(manifest["files"][0])
    after = dict(before, size=before["size"] + 1, sha256="b" * 64)
    added = {"path": "added.txt", "size": 1, "sha256": "c" * 64}
    removed = {"path": "removed.txt", "size": 2, "sha256": "d" * 64}
    unchanged = {"path": "unchanged.txt", "size": 3, "sha256": "e" * 64}
    upgrade = {
        "previous_version": "0.1.0", "previous_manifest_sha256": receipt["manifest_sha256"],
        "previous_receipt_sha256": digest(root / "examples/receipt.json"),
        "previous_task_id": receipt["task_id"], "previous_commit_id": receipt["commit_id"],
        "installation_id": receipt["installation_id"],
        "diff": {"added": [added], "replaced": [{"before": before, "after": after}],
                 "removed": [removed], "unchanged": [unchanged]},
        "staging_dir": r"C:\AvelinPrivate\m3-task\staging",
        "backup_dir": r"C:\AvelinPrivate\m3-task\backup", "required_space_bytes": 1048576,
    }
    upgraded_plan = dict(plan, operation="upgrade", task_id="m3-upgrade", app_version="0.2.0", upgrade=upgrade)
    check("m3/resource-replacement", upgrade["diff"]["replaced"][0], "model", "FileReplacement")
    check("m3/exhaustive-diff-shape", upgrade["diff"], "model", "UpgradeDiff")
    check("m3/upgrade-context", upgrade, "model", "UpgradePlan")
    check("m3/upgrade-plan", upgraded_plan, "model", "InstallPlan")
    check("m3/receipt-lineage", dict(receipt, previous_commit_id="previous-commit"), "model", "Receipt")
    check("m3/legacy-optional-lineage", dict(receipt, previous_commit_id=None), "model", "Receipt")
    check("m3/install-with-null-context", dict(plan, upgrade=None), "model", "InstallPlan")
    for operation_name in ("repair", "recover"):
        check(f"maintenance/{operation_name}-plan", dict(plan, operation=operation_name), "model", "InstallPlan")
        check(f"maintenance/{operation_name}-null-upgrade", dict(plan, operation=operation_name, upgrade=None), "model", "InstallPlan")
        reject(f"maintenance/{operation_name}-upgrade-context", dict(plan, operation=operation_name, upgrade=upgrade), "model", "InstallPlan")
    reject("maintenance/unknown-operation", dict(plan, operation="restore"), "model", "InstallPlan")
    text = {"upgrade.versions": "Version {previous_version} -> {version}",
            "window.upgrade_title": "Upgrade {app_name}", "action.upgrade": "Update"}
    check("m3/upgrade-presentation", dict(presentation, strings={"en-US": text}), "presentation")

    for key in upgrade:
        missing = copy.deepcopy(upgrade)
        del missing[key]
        reject("m3/missing-" + key, missing, "model", "UpgradePlan")
    for key in upgrade["diff"]:
        missing = copy.deepcopy(upgrade["diff"])
        del missing[key]
        reject("m3/missing-diff-" + key, missing, "model", "UpgradeDiff")
    for label, value, name, definition in [
        ("legacy/unknown-manifest-field", dict(manifest, unknown=True), "model", "Manifest"),
        ("legacy/schema-version", dict(manifest, schema_version=2), "model", "Manifest"),
        ("legacy/unknown-command-field", dict(command, unknown=True), "protocol", "TaskCommand"),
        ("m2/launch-external-path", {"command": "launch_installed", "path": "other.exe"}, "protocol", "TaskCommand"),
        ("m2/cancel-extra-field", {"command": "cancel", "path": "other.exe"}, "protocol", "TaskCommand"),
        ("m2/shortcut-kind", dict(record, kind="all_users"), "model", "ShortcutRecord"),
        ("m3/missing-context", dict(plan, operation="upgrade"), "model", "InstallPlan"),
        ("m3/null-context", dict(plan, operation="upgrade", upgrade=None), "model", "InstallPlan"),
        ("m3/install-context", dict(plan, upgrade=upgrade), "model", "InstallPlan"),
        ("m3/uninstall-context", dict(plan, operation="uninstall", upgrade=upgrade), "model", "InstallPlan"),
        ("m3/unknown-context-field", dict(upgrade, unknown=True), "model", "UpgradePlan"),
        ("m3/receipt-hash", dict(upgrade, previous_receipt_sha256="bad"), "model", "UpgradePlan"),
        ("m3/negative-space", dict(upgrade, required_space_bytes=-1), "model", "UpgradePlan"),
        ("m3/overflow-space", dict(upgrade, required_space_bytes=2 ** 64), "model", "UpgradePlan"),
        ("m3/unknown-diff-kind", dict(upgrade["diff"], renamed=[]), "model", "UpgradeDiff"),
        ("m3/missing-replacement-before", {"after": after}, "model", "FileReplacement"),
        ("m3/invalid-lineage", dict(receipt, previous_commit_id=42), "model", "Receipt"),
    ]:
        reject(label, value, name, definition)
    for label, value in [("missing", "Version {version}"), ("unknown", "{previous_version} {version} {other}")]:
        reject("m3/placeholder-" + label,
               dict(presentation, strings={"en-US": {"upgrade.versions": value}}), "presentation")

    component_manifest = example("examples/components-manifest.json", "model", "Manifest")
    component_plan = example("examples/components-install-plan.json", "model", "InstallPlan")
    component_receipt = example("examples/components-receipt.json", "model", "Receipt")
    modify_plan = example("examples/components-modify-plan.json", "model", "InstallPlan")
    example("examples/components-command.json", "protocol", "TaskCommand")
    # Schema branches reject component fields on legacy records and require
    # explicit ownership/selection on v2. Membership, sorting, exact file-set
    # equality, hashes and operation result evidence remain Rust obligations.
    reject("components/legacy-catalog", dict(manifest, components=component_manifest["components"]), "model", "Manifest")
    reject("components/legacy-file-owner", dict(manifest, files=component_manifest["files"]), "model", "Manifest")
    reject("components/legacy-selection", dict(receipt, selected_components=["core"]), "model", "Receipt")
    reject("components/legacy-plan-selection", dict(plan, selected_components=["core"]), "model", "InstallPlan")
    reject("components/v2-missing-owner", dict(component_manifest, files=manifest["files"]), "model", "Manifest")
    reject("components/v2-missing-core", dict(component_manifest, components=component_manifest["components"][1:]), "model", "Manifest")
    reject("components/v2-duplicate-core", dict(component_manifest, components=component_manifest["components"] + [component_manifest["components"][0]]), "model", "Manifest")
    for key in ("selected_components", "selection_source", "package_payload_bytes"):
        missing = copy.deepcopy(component_plan)
        missing.pop(key)
        reject("components/missing-plan-" + key, missing, "model", "InstallPlan")
    for selected in ([], ["extra"], ["core", "core"], ["core", "UPPER"]):
        reject("components/invalid-selection-" + repr(selected), dict(component_receipt, selected_components=selected), "model", "Receipt")
    core = component_manifest["components"][0]
    optional = component_manifest["components"][1]
    for value in (dict(core, required=False), dict(core, default_selected=False), dict(optional, required=True), dict(optional, requires=["core"]), dict(optional, name={"en-US": "Extra"})):
        reject("components/invalid-definition-" + repr(value), value, "model", "ComponentDefinition")
    reject("modify/schema-1", dict(modify_plan, schema_version=1), "model", "InstallPlan")
    reject("modify/missing-context", {k:v for k,v in modify_plan.items() if k != "upgrade"}, "model", "InstallPlan")
    for key in ("modify_snapshot_sha256", "backend_sha256", "selected_components", "previous_selected_components"):
        missing = copy.deepcopy(modify_plan)
        missing["upgrade"].pop(key)
        reject("modify/missing-" + key, missing, "model", "InstallPlan")
    replacement = copy.deepcopy(modify_plan)
    replacement["upgrade"]["diff"]["replaced"] = [{"before": component_manifest["files"][0], "after": component_manifest["files"][0]}]
    reject("modify/replacement-forbidden", replacement, "model", "InstallPlan")

    def completed_run(run, generation):
        run = run.resolve()
        summary = read(run / "summary.json")
        require(summary["status"] == "PASSED", f"{generation} harness has not passed: {run}")
        return run, summary

    def operation(run, generation, label, expected, manifest_file):
        report_file = run / f"{label}.report.json"
        report = read(report_file)
        saved = read(run / f"{label}.evidence.json")
        require(saved["report_sha256"] == digest(report_file), f"{label}: report bytes changed")
        require(report["result"] == "Succeeded" and report["exit_code"] == 0
                and report["coordinator_exit_code"] == 0 and report["product_lock_retained"] is False,
                f"{label}: saved operation did not confirm success")
        # M2 reports predate the top-level operation field. Their plan is authoritative here.
        require("operation" not in report or report["operation"] == expected, f"{label}: wrong report operation")
        actual = check(f"{generation}/{label}-plan", saved["plan"], "model", "InstallPlan")
        require(actual["operation"] == expected and actual["task_id"] == report["task_id"],
                f"{label}: mismatched task/operation")
        require(actual["manifest_sha256"] == digest(run / manifest_file), f"{label}: manifest bytes changed")
        require(report["events"], f"{label}: missing backend events")
        for event in report["events"]:
            require(event["task_id"] == report["task_id"] and event["protocol_version"] == 1,
                    f"{label}: event task/version mismatch")
            payload = {key: value for key, value in event.items()
                       if key not in {"task_id", "protocol_version", "seq", "coordinator_received_elapsed_ms"}}
            validator("protocol", "BackendEvent").validate(payload)
        positives.append(f"{generation}/{label}-saved-report-and-events")
        return actual

    if args.m2_run:
        run, _ = completed_run(args.m2_run, "M2")
        check("actual-m2/manifest", read(run / "embedded-manifest.json"), "model", "Manifest")
        for name in ("installed", "reinstalled"):
            check(f"actual-m2/{name}-receipt", read(run / f"{name}.receipt.json"), "model", "Receipt")
        for name, op in [("install", "install"), ("uninstall", "uninstall"),
                         ("reinstall", "install"), ("final-uninstall", "uninstall")]:
            operation(run, "actual-m2", name, op, "embedded-manifest.json")
        evidence.append({"generation": "M2", "directory": str(run), "status": "PASS"})

    if args.run:
        run, _ = completed_run(args.run, "M3")
        for name in ("a-old", "a-probe", "b", "b-alien"):
            check(f"actual-m3/{name}-manifest", read(run / f"{name}.embedded-manifest.json"), "model", "Manifest")
        old = check("actual-m3/installed-a-receipt", read(run / "installed-a.receipt.json"), "model", "Receipt")
        new = check("actual-m3/upgraded-b-receipt", read(run / "upgraded-b.receipt.json"), "model", "Receipt")
        operation(run, "actual-m3", "install-a", "install", "a-old.embedded-manifest.json")
        actual = operation(run, "actual-m3", "upgrade", "upgrade", "b.embedded-manifest.json")
        operation(run, "actual-m3", "uninstall-b", "uninstall", "b.embedded-manifest.json")
        context = actual["upgrade"]
        require(context["previous_receipt_sha256"] == digest(run / "installed-a.receipt.json")
                and context["previous_manifest_sha256"] == digest(run / "a-old.embedded-manifest.json"),
                "M3 previous receipt/manifest byte binding mismatch")
        require(context["previous_version"] == old["app_version"]
                and context["previous_task_id"] == old["task_id"]
                and context["previous_commit_id"] == old["commit_id"]
                and context["installation_id"] == old["installation_id"] == new["installation_id"]
                and new["previous_commit_id"] == old["commit_id"]
                and new["task_id"] == actual["task_id"] != old["task_id"]
                and new["commit_id"] != old["commit_id"], "M3 upgrade lineage mismatch")
        positives.append("actual-m3/recorded-upgrade-lineage")
        evidence.append({"generation": "M3", "directory": str(run), "status": "PASS"})

    print(json.dumps({"status": "PASS", "schemas": 3, "positive_cases": positives,
                      "negative_cases": negatives, "evidence": evidence,
                      "scope": "Offline structure and saved-evidence linkage only; SemVer, Windows paths, disk state, recovery and GUI require separate validation."},
                     ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
