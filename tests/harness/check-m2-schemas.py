"""Validate M2 additions and optional actual run evidence using local schemas only."""
import argparse
import copy
import json
from pathlib import Path

from jsonschema import Draft202012Validator
from referencing import Registry, Resource


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--run", type=Path, help="A historical completed M2 evidence directory")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2] / "docs" / "schemas"
    base = "https://avelin.dev/schemas/m1/"
    registry = Registry()
    for name in ("model", "protocol", "presentation"):
        data = json.loads((root / f"{name}.schema.json").read_text(encoding="utf-8"))
        Draft202012Validator.check_schema(data)
        registry = registry.with_resource(base + f"{name}.schema.json", Resource.from_contents(data))

    def validator(name, definition):
        return Draft202012Validator(
            {"$ref": base + f"{name}.schema.json#/$defs/{definition}"}, registry=registry
        )

    passed = []

    def check(label, data, name, definition):
        validator(name, definition).validate(data)
        passed.append(label)

    manifest = json.loads((root / "examples/manifest.json").read_text(encoding="utf-8"))
    manifest["desktop_shortcut"] = True
    manifest["start_menu_shortcut"] = True
    check("shortcut-defaults", manifest, "model", "Manifest")
    defaults = copy.deepcopy(manifest)
    del defaults["desktop_shortcut"]
    del defaults["start_menu_shortcut"]
    check("omitted-shortcut-defaults", defaults, "model", "Manifest")
    plan = json.loads((root / "examples/install-plan.json").read_text(encoding="utf-8"))
    plan["desktop_shortcut"] = False
    plan["start_menu_shortcut"] = False
    check("selected-options", plan, "model", "InstallPlan")
    receipt = json.loads((root / "examples/receipt.json").read_text(encoding="utf-8"))
    record = {"kind": "desktop", "path": "C:\\Users\\Example\\Desktop\\dev.example.app.lnk",
              "target": "C:\\Users\\Example\\App\\app.exe", "sha256": "a" * 64}
    receipt["shortcuts"] = [record]
    check("shortcut-ownership", receipt, "model", "Receipt")
    check("launch-intent", {"command": "launch_installed"}, "protocol", "TaskCommand")
    # The launch response is deliberately separate from the installation terminal snapshot.
    check("launch-result", {"response": "launch_result", "succeeded": True, "detail": ""},
          "protocol", "TaskResponse")
    selected = json.loads((root / "examples/components-command.json").read_text(encoding="utf-8"))
    check("components-frozen-ui-intent", selected, "protocol", "TaskCommand")
    check("legacy-ui-intent-still-accepted", {key: value for key, value in selected.items()
          if key not in {"selected_components", "operation", "migration_confirmed"}}, "protocol", "TaskCommand")
    invalid = [
        ("model", "Manifest", dict(manifest, desktop_shortcut="yes")),
        ("model", "ShortcutRecord", dict(record, kind="all_users")),
        ("model", "ShortcutRecord", dict(record, sha256="bad")),
        ("model", "Receipt", dict(receipt, shortcuts=[record] * 3)),
        ("protocol", "TaskCommand", {"command": "launch_installed", "path": "C:\\other.exe"}),
        ("protocol", "TaskCommand", dict(selected, selected_components=[])),
        ("protocol", "TaskCommand", dict(selected, selected_components=["extra"])),
        ("protocol", "TaskCommand", dict(selected, selected_components=["core", "core"])),
        ("protocol", "TaskCommand", dict(selected, migration_confirmed="yes")),
    ]
    for name, definition, value in invalid:
        if validator(name, definition).is_valid(value):
            raise AssertionError(f"Unexpected accepted negative: {name}/{definition}")

    if args.run:
        run = args.run.resolve()
        def read(name):
            return json.loads((run / name).read_text(encoding="utf-8-sig"))
        if read("summary.json")["status"] != "PASSED":
            raise AssertionError("Actual harness run must have passed")
        check("actual-manifest", read("embedded-manifest.json"), "model", "Manifest")
        for name in ("installed.receipt.json", "reinstalled.receipt.json"):
            check(name, read(name), "model", "Receipt")
        for name in ("install", "uninstall", "reinstall", "final-uninstall"):
            check(name + "-plan", read(name + ".evidence.json")["plan"], "model", "InstallPlan")
    print(json.dumps({"status": "PASS", "schemas": 3, "positive_cases": passed,
                      "negative_cases": len(invalid)}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
