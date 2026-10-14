"""Offline structural checks for install scope, prerequisites and HTTPS update models."""
import copy
import json
from pathlib import Path

from jsonschema import Draft202012Validator
from referencing import Registry, Resource


def main():
    root = Path(__file__).resolve().parents[2] / "docs" / "schemas"
    base = "https://avelin.dev/schemas/m1/"
    registry = Registry()
    for name in ("model", "protocol", "presentation", "updates"):
        data = json.loads((root / f"{name}.schema.json").read_text(encoding="utf-8"))
        Draft202012Validator.check_schema(data)
        registry = registry.with_resource(base + f"{name}.schema.json", Resource.from_contents(data))

    def validator(name, definition=None):
        ref = base + f"{name}.schema.json" + (f"#/$defs/{definition}" if definition else "")
        return Draft202012Validator({"$ref": ref}, registry=registry)

    def fixture(name):
        return json.loads((root / "examples" / name).read_text(encoding="utf-8"))

    manifest = fixture("windows-manifest.json")
    release = fixture("update-release.json")
    checks = 0

    def accepts(name, data, definition=None):
        nonlocal checks
        validator(name, definition).validate(data)
        checks += 1

    def rejects(name, data, definition=None):
        nonlocal checks
        if validator(name, definition).is_valid(data):
            raise AssertionError(f"Unexpected accepted {name}/{definition}: {data}")
        checks += 1

    accepts("model", manifest, "Manifest")
    accepts("updates", release)
    for name, definition in [("manifest.json", "Manifest"), ("receipt.json", "Receipt"), ("install-plan.json", "InstallPlan")]:
        old = fixture(name)
        accepts("model", old, definition)
        accepts("model", dict(old, install_scope="current_user"), definition)
        rejects("model", dict(old, install_scope="all_users"), definition)
    rejects("model", dict(manifest, install_scope="system"), "Manifest")
    rejects("model", dict(manifest, prerequisites=[{"kind": "custom", "min_version": "1.0.0"}]), "Manifest")
    rejects("model", dict(manifest, prerequisites=[{"kind": "vc_redist_x64", "min_version": "14.1"}]), "Manifest")
    old = fixture("manifest.json")
    rejects("model", dict(old, updates=manifest["updates"]), "Manifest")
    rejects("model", dict(old, prerequisites=manifest["prerequisites"]), "Manifest")
    for changed in ({"feed_url": "http://example.com/feed"}, {"feed_url": "https://user@example.com/feed"}, {"feed_url": "https://example.com/feed", "unknown": True}, {"feed_url": "https://example.com/feed", "channel": "BAD"}):
        rejects("model", dict(manifest, updates=changed), "Manifest")
    for key, value in [("schema_version", 2), ("platform", "linux-x64"), ("setup_url", "http://example.com/setup.exe"), ("size", 0), ("size", 2147483649), ("sha256", "not-a-digest")]:
        rejects("updates", dict(release, **{key: value}))
    for command in ["check_for_updates", "download_update"]:
        accepts("protocol", {"command": command}, "TaskCommand")
        rejects("protocol", {"command": command, "url": "https://example.com/evil"}, "TaskCommand")
    status = {"response": "update_status", "available_version": "2.0.0", "detail": "Update found", "launched": False}
    accepts("protocol", status, "TaskResponse")
    accepts("protocol", dict(status, available_version=None), "TaskResponse")
    rejects("protocol", dict(status, launched="yes"), "TaskResponse")
    invalid = copy.deepcopy(status)
    del invalid["detail"]
    rejects("protocol", invalid, "TaskResponse")
    print(json.dumps({"status": "PASS", "checks": checks, "network_requests": 0}))


if __name__ == "__main__":
    main()
