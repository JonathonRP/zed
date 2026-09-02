#!/usr/bin/env python3
"""Allocate RP calendar versions and assemble deterministic release metadata."""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import urllib.parse
from collections.abc import Iterable


CALENDAR_TAG = re.compile(r"^rp-stable-(?P<date>[0-9]{8})\.(?P<patch>[1-9][0-9]*)$")


class MetadataError(RuntimeError):
    pass


@dataclasses.dataclass(frozen=True)
class CalendarTag:
    name: str
    version: str
    date: str
    patch: int
    commit: str


@dataclasses.dataclass(frozen=True)
class Allocation:
    version: str
    tag: str
    reused: bool


def release_asset_names(version: str) -> dict[str, str]:
    prefix = f"rp-stable-{version}"
    return {
        "windows_x86_64_installer": f"Zed-{prefix}-windows-x86_64.exe",
        "windows_x86_64_portable": f"zed-{prefix}-windows-x86_64-portable.zip",
        "windows_x86_64_remote_server": (
            f"zed-{prefix}-remote-server-windows-x86_64.zip"
        ),
        "linux_x86_64_remote_server": (
            f"zed-{prefix}-remote-server-linux-x86_64.gz"
        ),
    }


def parse_calendar_tags(tags: Iterable[tuple[str, str]]) -> list[CalendarTag]:
    parsed: list[CalendarTag] = []
    versions: dict[str, str] = {}

    for name, commit in tags:
        match = CALENDAR_TAG.fullmatch(name)
        if match is None:
            continue

        version = f"{match['date']}.{int(match['patch'])}"
        existing_commit = versions.get(version)
        if existing_commit is not None and existing_commit != commit:
            raise MetadataError(
                f"calendar version {version} points to multiple commits: "
                f"{existing_commit} and {commit}"
            )
        versions[version] = commit
        parsed.append(
            CalendarTag(
                name=name,
                version=version,
                date=match["date"],
                patch=int(match["patch"]),
                commit=commit,
            )
        )

    return parsed


def allocate_calendar_version(
    current_commit: str, utc_date: str, tags: Iterable[tuple[str, str]]
) -> Allocation:
    calendar_tags = parse_calendar_tags(tags)
    current_versions = {
        tag.version for tag in calendar_tags if tag.commit == current_commit
    }
    if len(current_versions) > 1:
        versions = ", ".join(sorted(current_versions))
        raise MetadataError(
            f"current commit {current_commit} has inconsistent RP calendar identities: {versions}"
        )
    if current_versions:
        version = current_versions.pop()
        return Allocation(version, f"rp-stable-{version}", True)

    next_patch = (
        max((tag.patch for tag in calendar_tags if tag.date == utc_date), default=0)
        + 1
    )
    version = f"{utc_date}.{next_patch}"
    if any(tag.version == version for tag in calendar_tags):
        raise MetadataError(f"refusing to allocate colliding calendar version {version}")
    return Allocation(version, f"rp-stable-{version}", False)


def run_git(repo: pathlib.Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise MetadataError(f"git {' '.join(args)} failed: {detail}")
    return result.stdout.strip()


def read_tags(repo: pathlib.Path) -> list[tuple[str, str]]:
    names = run_git(repo, "tag", "--list", "rp-stable-*").splitlines()
    tags = []
    for name in names:
        commit = run_git(repo, "rev-list", "-n", "1", f"refs/tags/{name}")
        if not commit:
            raise MetadataError(f"could not resolve RP tag {name}")
        tags.append((name, commit))
    return tags


def find_previous_calendar_tag(
    repo: pathlib.Path, current_commit: str, calendar_tags: Iterable[CalendarTag]
) -> CalendarTag | None:
    tags_by_commit: dict[str, list[CalendarTag]] = {}
    for tag in calendar_tags:
        if tag.commit != current_commit:
            tags_by_commit.setdefault(tag.commit, []).append(tag)

    for commit in run_git(repo, "rev-list", "--first-parent", current_commit).splitlines():
        candidates = tags_by_commit.get(commit, [])
        if len(candidates) > 1:
            versions = ", ".join(sorted(tag.version for tag in candidates))
            raise MetadataError(
                f"prior commit {commit} has inconsistent RP calendar identities: {versions}"
            )
        if candidates:
            return candidates[0]
    return None


def load_fragment_manifest(repo: pathlib.Path) -> list[dict[str, str]]:
    manifest_path = repo / "script" / "rp_release_notes" / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    fragments = manifest.get("fragments")
    if not isinstance(fragments, list):
        raise MetadataError(f"{manifest_path} must contain a fragments list")

    seen_ids: set[str] = set()
    seen_files: set[str] = set()
    for fragment in fragments:
        if not isinstance(fragment, dict):
            raise MetadataError(f"invalid fragment entry in {manifest_path}")
        fragment_id = fragment.get("id")
        fragment_file = fragment.get("file")
        if not isinstance(fragment_id, str) or not isinstance(fragment_file, str):
            raise MetadataError(f"fragment entries require string id and file fields")
        if fragment_id in seen_ids or fragment_file in seen_files:
            raise MetadataError(f"duplicate RP release-note fragment {fragment_id}")
        seen_ids.add(fragment_id)
        seen_files.add(fragment_file)
    return fragments


def select_fragments(
    repo: pathlib.Path,
    fragments: list[dict[str, str]],
    previous_tag: CalendarTag | None,
) -> list[dict[str, str]]:
    if previous_tag is None:
        return fragments

    changed = set(
        run_git(
            repo,
            "diff",
            "--name-only",
            f"{previous_tag.commit}..HEAD",
            "--",
            "script/rp_release_notes",
        ).splitlines()
    )
    return [fragment for fragment in fragments if fragment["file"] in changed]


def render_notes(
    repo: pathlib.Path,
    version: str,
    upstream_version: str,
    commit: str,
    fragments: list[dict[str, str]],
) -> str:
    sections = [
        f"# RP Fork Release Notes {version}",
        "",
        f"- **Upstream Zed version:** `{upstream_version}`",
        f"- **Source commit:** `{commit}`",
        "",
    ]
    if fragments:
        for fragment in fragments:
            path = repo / fragment["file"]
            body = path.read_text(encoding="utf-8").strip()
            if not body:
                raise MetadataError(f"release-note fragment {path} is empty")
            sections.extend([body, ""])
    else:
        sections.extend(
            [
                "No curated user-facing RP fork changes were recorded for this release.",
                "",
            ]
        )
    return "\n".join(sections)


def write_github_outputs(path: pathlib.Path, allocation: Allocation) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"version={allocation.version}\n")
        output.write(f"tag={allocation.tag}\n")
        output.write(f"reused={'true' if allocation.reused else 'false'}\n")


def generate_metadata(
    repo: pathlib.Path,
    output_dir: pathlib.Path,
    utc_date: str,
    github_output: pathlib.Path | None,
) -> None:
    current_commit = run_git(repo, "rev-parse", "HEAD")
    raw_tags = read_tags(repo)
    calendar_tags = parse_calendar_tags(raw_tags)
    allocation = allocate_calendar_version(current_commit, utc_date, raw_tags)
    previous_tag = find_previous_calendar_tag(repo, current_commit, calendar_tags)
    fragments = select_fragments(repo, load_fragment_manifest(repo), previous_tag)
    upstream_version = run_git(repo, "show", "HEAD:crates/zed/Cargo.toml")
    version_match = re.search(
        r"(?m)^version\s*=\s*\"([^\"]+)\"\s*$", upstream_version
    )
    if version_match is None:
        raise MetadataError("could not read the zed package version")
    upstream_version = version_match.group(1)

    notes = render_notes(
        repo, allocation.version, upstream_version, current_commit, fragments
    )
    notes_identity = hashlib.sha256(notes.encode("utf-8")).hexdigest()
    manifest = {
        "schema_version": 1,
        "channel": "rp-stable",
        "calendar_version": allocation.version,
        "tag": allocation.tag,
        "upstream_version": upstream_version,
        "commit": current_commit,
        "trust": {"signed": False, "label": "unsigned"},
        "notes_identity": f"sha256:{notes_identity}",
        "previous_calendar_tag": previous_tag.name if previous_tag else None,
        "reused_calendar_version": allocation.reused,
        "fragments": [fragment["id"] for fragment in fragments],
        "asset_names": release_asset_names(allocation.version),
    }

    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "release-notes.md").write_text(notes, encoding="utf-8")
    (output_dir / "rp-release-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if github_output is not None:
        write_github_outputs(github_output, allocation)


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def finalize_update_manifest(
    metadata_path: pathlib.Path,
    dist: pathlib.Path,
    repository: str,
    output_file: pathlib.Path,
) -> None:
    if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
        raise MetadataError(f"invalid GitHub repository identity {repository!r}")

    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    required = {
        "schema_version",
        "channel",
        "calendar_version",
        "tag",
        "upstream_version",
        "commit",
        "trust",
        "notes_identity",
        "asset_names",
    }
    missing = sorted(required.difference(metadata))
    if missing:
        raise MetadataError(
            f"{metadata_path} is missing required fields: {', '.join(missing)}"
        )
    if metadata["schema_version"] != 1 or metadata["channel"] != "rp-stable":
        raise MetadataError(f"{metadata_path} is not an RP stable schema version 1 manifest")
    if metadata["trust"] != {"signed": False, "label": "unsigned"}:
        raise MetadataError(f"{metadata_path} must explicitly describe unsigned assets")
    version = metadata["calendar_version"]
    if re.fullmatch(r"[0-9]{8}\.[1-9][0-9]*", version) is None:
        raise MetadataError(f"{metadata_path} has invalid calendar version {version!r}")
    if metadata["tag"] != f"rp-stable-{version}":
        raise MetadataError(f"{metadata_path} tag does not match its calendar version")
    if re.fullmatch(r"[0-9a-f]{40}", metadata["commit"]) is None:
        raise MetadataError(f"{metadata_path} does not contain a full commit SHA")
    if re.fullmatch(
        r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
        r"(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?",
        metadata["upstream_version"],
    ) is None:
        raise MetadataError(f"{metadata_path} has invalid upstream semver")
    if re.fullmatch(r"sha256:[0-9a-f]{64}", metadata["notes_identity"]) is None:
        raise MetadataError(f"{metadata_path} has invalid release-notes identity")
    if metadata["asset_names"] != release_asset_names(version):
        raise MetadataError(f"{metadata_path} has an unexpected asset set")

    base_url = (
        f"https://github.com/{repository}/releases/download/"
        f"{urllib.parse.quote(metadata['tag'], safe='')}/"
    )
    assets = {}
    for asset_id, name in metadata["asset_names"].items():
        path = dist / name
        if not path.is_file():
            raise MetadataError(f"missing expected release asset {path}")
        assets[asset_id] = {
            "name": name,
            "size": path.stat().st_size,
            "sha256": sha256_file(path),
            "url": base_url + urllib.parse.quote(name, safe=""),
        }

    update_manifest = {
        key: metadata[key]
        for key in (
            "schema_version",
            "channel",
            "calendar_version",
            "upstream_version",
            "commit",
            "tag",
            "trust",
            "notes_identity",
        )
    }
    update_manifest["assets"] = assets
    output_file.write_text(
        json.dumps(update_manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=pathlib.Path)
    parser.add_argument("--repo", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--github-output", type=pathlib.Path)
    parser.add_argument("--finalize-manifest", type=pathlib.Path)
    parser.add_argument("--dist", type=pathlib.Path)
    parser.add_argument("--repository")
    parser.add_argument("--output-file", type=pathlib.Path)
    parser.add_argument(
        "--date",
        default=datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d"),
        help="UTC date used for allocation (YYYYMMDD)",
    )
    args = parser.parse_args()

    if re.fullmatch(r"[0-9]{8}", args.date) is None:
        parser.error("--date must use YYYYMMDD")

    try:
        if args.finalize_manifest is not None:
            if args.dist is None or args.repository is None or args.output_file is None:
                parser.error(
                    "--finalize-manifest requires --dist, --repository, and --output-file"
                )
            finalize_update_manifest(
                args.finalize_manifest.resolve(),
                args.dist.resolve(),
                args.repository,
                args.output_file.resolve(),
            )
        else:
            if args.output_dir is None:
                parser.error("--output-dir is required when generating metadata")
            generate_metadata(
                args.repo.resolve(),
                args.output_dir.resolve(),
                args.date,
                args.github_output,
            )
    except (MetadataError, OSError, json.JSONDecodeError) as error:
        print(f"RP release metadata error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
