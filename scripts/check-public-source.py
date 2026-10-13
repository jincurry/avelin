#!/usr/bin/env python3
"""Read-only checks for the working-tree files Git would publish.

Python 3.11+ and Git are sufficient. Run before staging and again before pushing.
Tracked files must match their index versions; stage the intended content and
rerun if they differ. This is a targeted check, not proof that a repository
contains no secrets. It does not scan ignored files, Git history, compressed
archives, or release ZIPs.
"""
from __future__ import annotations

import argparse
import bisect
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import posixpath
import re
import stat
import subprocess
import sys
from urllib.parse import unquote, urlsplit


MAX_BYTES = 20 * 1024 * 1024
PRODUCT_SUFFIXES = {".exe", ".dll", ".pdb", ".log", ".zip", ".pyc", ".pyo"}
REVIEWED_BINARY_INPUTS = {
    "examples/nimbus/branding/nimbus.png",
}
CREDENTIAL_PATTERNS = {
    "private_key": re.compile(r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----"),
    "credential_token": re.compile(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{25,}"
        r"|sk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{30,}"
        r"|(?:AKIA|ASIA)[A-Z0-9]{16}|AIza[A-Za-z0-9_-]{35}"
        r"|xox[baprs]-[A-Za-z0-9-]{20,})\b"
    ),
}
ESCAPED_PUNCTUATION = re.compile(r"\\([!\"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~ ])")


def git(root: Path, *arguments: str) -> bytes:
    process = subprocess.run(
        ["git", "-C", str(root), *arguments], capture_output=True, check=False
    )
    if process.returncode:
        # Git's stderr can contain absolute paths or remote credentials.
        raise RuntimeError("Git command failed; run from an accessible Git working tree")
    return process.stdout


def git_paths(root: Path, *arguments: str) -> set[str]:
    return {os.fsdecode(value) for value in git(root, *arguments, "-z").split(b"\0") if value}


def is_link(metadata: os.stat_result) -> bool:
    return stat.S_ISLNK(metadata.st_mode) or bool(
        getattr(metadata, "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    )


def checked_metadata(path: Path) -> os.stat_result:
    # Inspect spelling before resolve() could hide a junction or symlink.
    for ancestor in reversed((path, *path.parents)):
        metadata = ancestor.lstat()
        if is_link(metadata):
            raise ValueError("linked_path")
    return metadata


def mask_markdown_code(text: str) -> str:
    """Preserve character/line positions while excluding examples and comments."""
    def blank(value: str) -> str:
        return re.sub(r"[^\r\n]", " ", value)

    lines = []
    fence = None
    for line in text.splitlines(keepends=True):
        match = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if fence:
            lines.append(blank(line))
            if match and match[1][0] == fence[0] and len(match[1]) >= len(fence):
                if not line[match.end():].strip():
                    fence = None
        elif match:
            fence = match[1]
            lines.append(blank(line))
        else:
            lines.append(line)
    masked = "".join(lines)
    masked = re.sub(r"<!--.*?-->", lambda match: blank(match[0]), masked, flags=re.S)
    return re.sub(
        r"(?<!`)(`+)(?!`)(.*?)(?<!`)\1(?!`)",
        lambda match: blank(match[0]), masked, flags=re.S,
    )


def destination(text: str, start: int) -> tuple[str, int] | None:
    """Read a Markdown destination, honoring angle brackets and balanced ()."""
    position = start
    while position < len(text) and text[position].isspace():
        position += 1
    if position >= len(text):
        return None
    beginning = position
    if text[position] == "<":
        position += 1
        beginning = position
        while position < len(text) and text[position] not in "\r\n":
            if text[position] == "\\" and position + 1 < len(text):
                position += 2
                continue
            if text[position] == ">":
                return text[beginning:position], position + 1
            position += 1
        return None
    depth = 0
    while position < len(text):
        character = text[position]
        if character == "\\" and position + 1 < len(text):
            position += 2
            continue
        if character.isspace() or (character == ")" and depth == 0):
            break
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        position += 1
    if depth or position == beginning:
        return None
    return text[beginning:position], position


def inline_link_end(text: str, position: int) -> bool:
    while position < len(text) and text[position].isspace():
        position += 1
    if position < len(text) and text[position] in "\"'(":
        closing = ")" if text[position] == "(" else text[position]
        position += 1
        while position < len(text):
            if text[position] == "\\" and position + 1 < len(text):
                position += 2
                continue
            if text[position] == closing:
                position += 1
                break
            position += 1
        while position < len(text) and text[position].isspace():
            position += 1
    return position < len(text) and text[position] == ")"


def markdown_destinations(text: str):
    masked = mask_markdown_code(text)
    for match in re.finditer(r"^ {0,3}\[[^\]\r\n]+\]:[ \t]*", masked, re.M):
        parsed = destination(masked, match.end())
        if parsed:
            yield match.start(), parsed[0]
    # A closing label followed immediately by '(' identifies both links/images.
    # Unescaped ')' inside the destination must balance; optional titles work.
    for match in re.finditer(r"(?<!\\)\]\(", masked):
        parsed = destination(masked, match.end())
        if parsed and inline_link_end(masked, parsed[1]):
            yield match.start(), parsed[0]


def local_link_target(source: str, raw: str) -> tuple[str, str] | None:
    value = ESCAPED_PUNCTUATION.sub(r"\1", raw)
    if not value or value.startswith(("#", "//")):
        return None
    if PureWindowsPath(value).drive or value.lower().startswith("file:"):
        return "absolute_local_link", ""
    try:
        parsed = urlsplit(value)
    except ValueError:
        return "invalid_link", ""
    if parsed.scheme or parsed.netloc:
        return None
    path = unquote(parsed.path)
    if not path:
        return None
    if path.startswith(("/", "\\")) or PureWindowsPath(path).drive:
        return "absolute_local_link", ""
    normalized = posixpath.normpath(posixpath.join(posixpath.dirname(source), path))
    if normalized == ".." or normalized.startswith("../"):
        return "link_outside_repository", ""
    return "relative", normalized


def home_patterns() -> list[re.Pattern]:
    homes = {str(Path.home())}
    homes.update(os.environ[key] for key in ("HOME", "USERPROFILE") if os.environ.get(key))
    patterns = []
    for home in homes:
        normalized = re.sub(r"[\\/]+", "/", home).rstrip("/")
        if normalized and (Path(home).is_absolute() or PureWindowsPath(home).is_absolute()):
            patterns.append(re.compile(re.escape(normalized) + r"(?=$|[/\s'\"`<>\]\)])", re.I))
    return patterns


def audit(root: Path) -> dict:
    root = Path(os.fsdecode(git(root, "rev-parse", "--show-toplevel")).strip()).absolute()
    candidates = git_paths(root, "ls-files", "--cached", "--others", "--exclude-standard")
    tracked_ignored = git_paths(root, "ls-files", "--cached", "--ignored", "--exclude-standard")
    unstaged = git_paths(root, "diff", "--no-ext-diff", "--no-textconv", "--name-only",
                         "--ignore-submodules=none")
    # Git can list a deleted tracked file; its absence remains a finding.
    public = candidates - tracked_ignored
    findings = []
    text_files = {}
    binary_count = 0

    def finding(path: str, category: str, line: int | None = None):
        item = {"path": path, "category": category}
        if line is not None:
            item["line"] = line
        findings.append(item)

    for relative in sorted(candidates):
        if relative in tracked_ignored:
            finding(relative, "ignored_but_tracked")
            continue  # Do not read ignored content, even if accidentally tracked.
        if relative in unstaged:
            # A clean working copy must never conceal a secret already in the index.
            finding(relative, "index_worktree_mismatch")
            continue
        path_parts = PurePosixPath(relative)
        if path_parts.is_absolute() or PureWindowsPath(relative).drive or ".." in path_parts.parts:
            finding(relative, "unsafe_candidate_path")
            continue
        path = root.joinpath(*path_parts.parts)
        try:
            metadata = checked_metadata(path)
        except ValueError:
            finding(relative, "linked_path")
            continue
        except OSError:
            finding(relative, "missing_or_unreadable_candidate")
            continue
        if not stat.S_ISREG(metadata.st_mode):
            finding(relative, "non_regular_candidate")
            continue
        if metadata.st_size > MAX_BYTES:
            finding(relative, "file_over_20_mib")
            continue
        if path.suffix.lower() in PRODUCT_SUFFIXES or "__pycache__" in path.parts:
            finding(relative, "generated_product_or_cache")
            continue
        if relative in REVIEWED_BINARY_INPUTS:
            binary_count += 1
            continue
        try:
            data = path.read_bytes()
            text = data.decode("utf-16" if data.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8-sig")
        except UnicodeError:
            finding(relative, "unreviewed_binary_or_text_encoding")
            continue
        except OSError:
            finding(relative, "unreadable_candidate")
            continue
        if "\0" in text:
            finding(relative, "unreviewed_binary_or_text_encoding")
            continue
        text_files[relative] = text

    homes = home_patterns()
    relative_links = 0
    for relative, text in text_files.items():
        newline_positions = [match.start() for match in re.finditer("\n", text)]
        for category, pattern in CREDENTIAL_PATTERNS.items():
            match = pattern.search(text)
            if match:
                finding(relative, category, bisect.bisect_left(newline_positions, match.start()) + 1)
        # Match ordinary, JSON-escaped, and percent-encoded home path spellings.
        for number, line in enumerate(text.splitlines(), 1):
            normalized = re.sub(r"[\\/]+", "/", unquote(line))
            if any(pattern.search(normalized) for pattern in homes):
                finding(relative, "current_user_home_path", number)
                break
        if PurePosixPath(relative).suffix.lower() not in {".md", ".markdown"}:
            continue
        for offset, raw in markdown_destinations(text):
            target = local_link_target(relative, raw)
            if target is None:
                continue
            category, normalized = target
            line = bisect.bisect_left(newline_positions, offset) + 1
            if category != "relative":
                finding(relative, category, line)
                continue
            relative_links += 1
            # GitHub directory links are valid if the directory contains public files.
            prefix = normalized.rstrip("/") + "/"
            if (normalized != "." and normalized not in public
                    and not any(item.startswith(prefix) for item in public)):
                finding(relative, "link_target_not_public", line)

    return {
        "schema_version": 1,
        "status": "PASSED" if not findings else "FAILED",
        "candidate_files": len(candidates),
        "tracked_worktree_differences": len(unstaged),
        "checked_text_files": len(text_files),
        "reviewed_binary_inputs": binary_count,
        "relative_markdown_links": relative_links,
        "findings": findings,
        "limitations": [
            "Tracked working-tree/index differences fail; stage the intended content and rerun.",
            "Targeted working-tree checks cannot prove the absence of secrets.",
            "Ignored content, Git history, archives and release packages are not scanned.",
            "Markdown checks cover inline/image links and reference definitions, not HTML links or remote URLs.",
            "Existing third-party copyright text is preserved; license approval is outside this check.",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).absolute().parents[1])
    arguments = parser.parse_args()
    try:
        report = audit(arguments.root)
    except (OSError, RuntimeError) as error:
        report = {"schema_version": 1, "status": "ERROR", "category": type(error).__name__,
                  "message": "Cannot inspect the Git working tree; no file contents are included in this error."}
    print(json.dumps(report, ensure_ascii=True, indent=2))
    return 0 if report["status"] == "PASSED" else 1


if __name__ == "__main__":
    sys.exit(main())
