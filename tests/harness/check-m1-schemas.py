"""Validate local M1 schema drafts and fixtures; no network lookup or installation."""
import copy
import json
from pathlib import Path

from jsonschema import Draft202012Validator
from referencing import Registry, Resource


def main():
    root = Path(__file__).resolve().parents[2] / "docs" / "schemas"
    base = "https://avelin.dev/schemas/m1/"
    registry = Registry()
    schemas = {}
    for name in ("model", "protocol", "presentation"):
        schema = json.loads((root / f"{name}.schema.json").read_text(encoding="utf-8"))
        Draft202012Validator.check_schema(schema)
        schemas[name] = schema
        registry = registry.with_resource(base + f"{name}.schema.json", Resource.from_contents(schema))

    passed = []

    def validator(name, definition=None):
        ref = base + f"{name}.schema.json" + (f"#/$defs/{definition}" if definition else "")
        return Draft202012Validator({"$ref": ref}, registry=registry)

    def check(path, name, definition=None, array=False):
        data = json.loads((root / path).read_text(encoding="utf-8"))
        check_with = validator(name, definition)
        for item in data if array else [data]:
            check_with.validate(item)
        passed.append(path)
        return data

    manifest = check("examples/manifest.json", "model", "Manifest")
    check("examples/receipt.json", "model", "Receipt")
    check("examples/install-plan.json", "model", "InstallPlan")
    command = check("examples/command.json", "protocol", "TaskCommand")
    blocked = {"response": "preflight_blocked", "detail": "Example (PID 42): example.exe"}
    validator("protocol", "TaskResponse").validate(blocked)
    validator("protocol", "TaskResponse").validate(
        {"response": "launch_result", "succeeded": True, "detail": "Started"})
    passed.append("host-action-responses")
    check("examples/snapshot.json", "protocol", "TaskSnapshot")
    check("examples/events.json", "protocol", "BackendEvent", array=True)
    for result in ("succeeded", "cancelled", "failed", "unknown"):
        check(f"examples/replay-{result}.json", "protocol", "BackendEvent", array=True)
    presentation = check("presentation.example.json", "presentation")
    check("examples/components-manifest.json", "model", "Manifest")
    check("examples/components-install-plan.json", "model", "InstallPlan")
    check("examples/components-receipt.json", "model", "Receipt")
    check("examples/components-modify-plan.json", "model", "InstallPlan")
    check("examples/components-command.json", "protocol", "TaskCommand")
    with_ui = copy.deepcopy(manifest)
    with_ui["presentation"] = presentation
    validator("model", "Manifest").validate(with_ui)
    passed.append("manifest-with-presentation")

    negatives = [
        ("model", "Manifest", dict(manifest, unknown=True)),
        ("model", "Manifest", dict(manifest, schema_version=2)),
        ("protocol", "TaskCommand", dict(command, unknown=True)),
        ("protocol", "TaskResponse", dict(blocked, succeeded=False)),
        ("protocol", "TaskResponse", {"response": "preflight_blocked"}),
        ("protocol", "TaskResponse", dict(blocked, detail=False)),
        ("protocol", "BackendEvent", {"kind": "finish", "result": "Succeeded", "code": 3}),
        ("protocol", "BackendEvent", {"kind": "progress", "phase": "invented", "completed": 0, "total": 1}),
        ("presentation", None, dict(presentation, template="web")),
        ("presentation", None, dict(presentation, accent_color="#abcd")),
        ("presentation", None, dict(presentation, strings={"xx": {}})),
    ]
    for name, definition, data in negatives:
        if validator(name, definition).is_valid(data):
            raise AssertionError(f"Unexpected accepted negative for {name}/{definition}: {data}")
    print(json.dumps({"status": "PASS", "schemas": len(schemas), "positive_cases": passed,
                      "negative_cases": len(negatives)}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
