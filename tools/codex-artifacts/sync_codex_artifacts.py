#!/usr/bin/env python3
"""Publish the complete Codex customer-install artifact set to Baijimu OSS."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from typing import Any
import urllib.error
import urllib.request


USER_AGENT = "baijimu-codex-upstream-artifact-sync/2"
DEFAULT_RELEASE_API = "https://api.github.com/repos/openai/codex/releases/latest"
DEFAULT_PUBLIC_BASE = "https://lowcode-common.oss-cn-beijing.aliyuncs.com"
DEFAULT_PREFIX = "codex-artifacts"

CLI_ASSET_NAMES = (
    "codex-aarch64-apple-darwin.tar.gz",
    "codex-x86_64-apple-darwin.tar.gz",
    "codex-aarch64-pc-windows-msvc.exe.zip",
    "codex-x86_64-pc-windows-msvc.exe.zip",
)

APP_ASSETS = (
    {
        "name": "codex-app-aarch64-apple-darwin.dmg",
        "platform": "macos",
        "arch": "aarch64",
        "upstream_url": "https://persistent.oaistatic.com/codex-app-prod/ChatGPT.dmg",
        "content_type": "application/x-apple-diskimage",
        "source_kind": "official_openai_static",
    },
    {
        "name": "codex-app-x86_64-apple-darwin.dmg",
        "platform": "macos",
        "arch": "x86_64",
        "upstream_url": "https://persistent.oaistatic.com/codex-app-prod/ChatGPT-latest-x64.dmg",
        "content_type": "application/x-apple-diskimage",
        "source_kind": "official_openai_static",
    },
    {
        "name": "codex-app-windows-x64.msix",
        "platform": "windows",
        "arch": "x86_64",
        "upstream_url": "https://codexapp.agentsmirror.com/latest/win-x64",
        "content_type": "application/vnd.ms-appx",
        "source_kind": "microsoft_store_signed_package_mirror",
    },
    {
        "name": "codex-app-windows-arm64.msix",
        "platform": "windows",
        "arch": "aarch64",
        "upstream_url": "https://codexapp.agentsmirror.com/latest/win-arm64",
        "content_type": "application/vnd.ms-appx",
        "source_kind": "microsoft_store_signed_package_mirror",
    },
)


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def request_json(url: str, token: str | None = None) -> dict[str, Any]:
    headers = {"Accept": "application/vnd.github+json", "User-Agent": USER_AGENT}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=90) as response:
        return json.load(response)


def fetch_existing_manifest(url: str) -> dict[str, Any] | None:
    try:
        with urllib.request.urlopen(
            urllib.request.Request(url, headers={"User-Agent": USER_AGENT}), timeout=60
        ) as response:
            return json.load(response)
    except (urllib.error.HTTPError, urllib.error.URLError, json.JSONDecodeError):
        return None


def select_assets(release: dict[str, Any]) -> list[dict[str, Any]]:
    by_name = {asset.get("name"): asset for asset in release.get("assets", [])}
    selected: list[dict[str, Any]] = []
    for name in CLI_ASSET_NAMES:
        source = by_name.get(name)
        if not source:
            raise RuntimeError(f"official Codex release is missing required asset: {name}")
        upstream_digest = source.get("digest")
        if not isinstance(upstream_digest, str) or not upstream_digest.startswith("sha256:"):
            raise RuntimeError(f"official Codex release has no SHA256 digest for: {name}")
        selected.append(
            {
                "name": name,
                "component": "codex_cli",
                "platform": "windows" if "pc-windows" in name else "macos",
                "arch": "aarch64" if "aarch64" in name else "x86_64",
                "upstream_url": source["browser_download_url"],
                "upstream_sha256": upstream_digest.removeprefix("sha256:"),
                "expected_size": int(source["size"]),
                "content_type": source.get("content_type")
                or ("application/zip" if name.endswith(".zip") else "application/gzip"),
                "source_kind": "official_openai_github_release",
            }
        )
    for source in APP_ASSETS:
        expected_env = {
            "codex-app-aarch64-apple-darwin.dmg": "CODEX_APP_MACOS_ARM64_SHA256",
            "codex-app-x86_64-apple-darwin.dmg": "CODEX_APP_MACOS_X64_SHA256",
            "codex-app-windows-x64.msix": "CODEX_APP_WINDOWS_X64_SHA256",
            "codex-app-windows-arm64.msix": "CODEX_APP_WINDOWS_ARM64_SHA256",
        }[source["name"]]
        expected_sha256 = os.environ.get(expected_env)
        if os.environ.get("REQUIRE_DESKTOP_SIGNATURE_DIGESTS", "1") == "1" and not expected_sha256:
            raise RuntimeError(
                f"{expected_env} is required; verify the desktop package signature on its native platform first"
            )
        selected.append(
            {
                **source,
                "component": "codex_desktop_app",
                "upstream_sha256": expected_sha256,
                "signature_verification": (
                    "codesign+spctl" if source["platform"] == "macos" else "signtool"
                ),
            }
        )
    names = [asset["name"] for asset in selected]
    if len(names) != len(set(names)):
        raise RuntimeError("duplicate artifact name in customer install contract")
    return selected


def download_asset(asset: dict[str, Any], destination: Path) -> dict[str, Any]:
    command = [
        "curl",
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--retry",
        "8",
        "--retry-all-errors",
        "--retry-delay",
        "3",
        "--connect-timeout",
        "30",
        "--max-time",
        "3600",
        "--output",
        str(destination),
        "--write-out",
        "%{url_effective}\n%{content_type}\n",
        asset["upstream_url"],
    ]
    completed = subprocess.run(command, check=True, text=True, capture_output=True)
    response_lines = completed.stdout.rstrip("\n").split("\n")
    effective_url = response_lines[-2] if len(response_lines) >= 2 else asset["upstream_url"]
    response_type = response_lines[-1] if response_lines else ""
    actual_size = destination.stat().st_size
    actual_sha256 = sha256_file(destination)
    expected_size = asset.get("expected_size")
    expected_sha256 = asset.get("upstream_sha256")
    if expected_size is not None and actual_size != expected_size:
        raise RuntimeError(
            f"upstream size mismatch for {asset['name']}: expected {expected_size}, got {actual_size}"
        )
    if expected_sha256 and actual_sha256 != expected_sha256:
        raise RuntimeError(
            f"upstream SHA256 mismatch for {asset['name']}: expected {expected_sha256}, got {actual_sha256}"
        )
    return {
        **asset,
        "path": destination,
        "sha256": actual_sha256,
        "size": actual_size,
        "effective_upstream_url": effective_url,
        "response_content_type": response_type,
    }


def snapshot_id(assets: list[dict[str, Any]]) -> str:
    identity = [
        {"name": asset["name"], "sha256": asset["sha256"], "size": asset["size"]}
        for asset in sorted(assets, key=lambda item: item["name"])
    ]
    payload = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()


def public_asset_is_exact(url: str, expected_path: Path) -> bool:
    check_path = expected_path.with_name(f"verify-existing-{expected_path.name}")
    completed = subprocess.run(
        [
            "curl",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--output",
            str(check_path),
            url,
        ],
        check=False,
    )
    if completed.returncode != 0:
        return False
    return check_path.stat().st_size == expected_path.stat().st_size and sha256_file(check_path) == sha256_file(expected_path)


def oss_cp(source: Path, target: str, content_type: str, cache_control: str) -> None:
    ossutil = os.environ.get("OSSUTIL") or shutil.which("ossutil")
    access_key_id = os.environ.get("OSS_ACCESS_KEY_ID")
    access_key_secret = os.environ.get("OSS_ACCESS_KEY_SECRET")
    if not ossutil:
        raise RuntimeError("ossutil not found; set OSSUTIL to the pinned executable")
    if not access_key_id or not access_key_secret:
        raise RuntimeError("OSS_ACCESS_KEY_ID and OSS_ACCESS_KEY_SECRET are required")
    endpoint = os.environ.get("OSS_ENDPOINT", "oss-cn-beijing.aliyuncs.com")
    region = os.environ.get("OSS_REGION", "cn-beijing")
    subprocess.run(
        [
            ossutil,
            "cp",
            str(source),
            target,
            "--access-key-id",
            access_key_id,
            "--access-key-secret",
            access_key_secret,
            "--endpoint",
            endpoint,
            "--region",
            region,
            "--force",
            "--no-progress",
            "--content-type",
            content_type,
            "--cache-control",
            cache_control,
        ],
        check=True,
    )


def manifest_for(
    release: dict[str, Any], assets: list[dict[str, Any]], public_base: str, prefix: str
) -> dict[str, Any]:
    sid = snapshot_id(assets)
    output_assets = []
    for asset in sorted(assets, key=lambda item: item["name"]):
        object_key = f"{prefix}/assets/sha256/{asset['sha256']}/{asset['name']}"
        output_assets.append(
            {
                "name": asset["name"],
                "component": asset["component"],
                "platform": asset["platform"],
                "arch": asset["arch"],
                "source_kind": asset["source_kind"],
                "upstream_url": asset["upstream_url"],
                "effective_upstream_url": asset["effective_upstream_url"],
                "upstream_sha256": asset.get("upstream_sha256"),
                "signature_verification": asset.get("signature_verification"),
                "mirror_url": f"{public_base.rstrip('/')}/{object_key}",
                "object_key": object_key,
                "sha256": asset["sha256"],
                "size": asset["size"],
                "size_bytes": asset["size"],
                "content_type": asset["content_type"],
            }
        )
    return {
        "schema_version": 2,
        "manifest_kind": "baijimu.codex.customer-install-artifacts",
        "source": "momoplan/baijimu-connector-codex",
        "snapshot_id": sid,
        "fetched_at": utc_now(),
        "components": {
            "codex_cli": {
                "source": "github.com/openai/codex",
                "tag_name": release.get("tag_name"),
                "published_at": release.get("published_at"),
                "html_url": release.get("html_url"),
            },
            "codex_desktop_app": {
                "source": "official OpenAI static distribution and Microsoft Store signed packages",
                "version_identity": "asset SHA256 digests",
            },
        },
        # Kept for consumers that still display the legacy CLI release field.
        "upstream_release": {
            "tag_name": release.get("tag_name"),
            "name": release.get("name"),
            "published_at": release.get("published_at"),
            "html_url": release.get("html_url"),
        },
        "required_assets": [asset["name"] for asset in output_assets],
        "assets": output_assets,
    }


def validate_manifest(manifest: dict[str, Any]) -> None:
    if manifest.get("schema_version") != 2:
        raise RuntimeError("unexpected manifest schema")
    assets = manifest.get("assets")
    if not isinstance(assets, list) or len(assets) != len(CLI_ASSET_NAMES) + len(APP_ASSETS):
        raise RuntimeError("manifest does not contain the complete customer install contract")
    required = set(CLI_ASSET_NAMES) | {asset["name"] for asset in APP_ASSETS}
    actual = {asset.get("name") for asset in assets}
    if actual != required or set(manifest.get("required_assets", [])) != required:
        raise RuntimeError(f"manifest asset set mismatch: expected {sorted(required)}, got {sorted(actual)}")
    for asset in assets:
        if not asset.get("mirror_url", "").startswith("https://"):
            raise RuntimeError(f"asset has invalid mirror URL: {asset.get('name')}")
        if len(asset.get("sha256", "")) != 64 or int(asset.get("size", 0)) <= 0:
            raise RuntimeError(f"asset has invalid integrity metadata: {asset.get('name')}")


def publish(manifest: dict[str, Any], files: dict[str, Path], work_dir: Path) -> None:
    bucket = os.environ.get("OSS_BUCKET", "lowcode-common")
    prefix = os.environ.get("OSS_PREFIX", DEFAULT_PREFIX).strip("/")
    public_base = os.environ.get("OSS_PUBLIC_BASE_URL", DEFAULT_PUBLIC_BASE).rstrip("/")
    for asset in manifest["assets"]:
        source = files[asset["name"]]
        if not public_asset_is_exact(asset["mirror_url"], source):
            oss_cp(
                source,
                f"oss://{bucket}/{asset['object_key']}",
                asset["content_type"],
                "public,max-age=31536000,immutable",
            )
            if not public_asset_is_exact(asset["mirror_url"], source):
                raise RuntimeError(f"public OSS read-back verification failed: {asset['name']}")

    manifest_path = work_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    manifest_sha = sha256_file(manifest_path)
    immutable_key = f"{prefix}/manifests/sha256/{manifest_sha}/manifest.json"
    immutable_url = f"{public_base}/{immutable_key}"
    if not public_asset_is_exact(immutable_url, manifest_path):
        oss_cp(
            manifest_path,
            f"oss://{bucket}/{immutable_key}",
            "application/json",
            "public,max-age=31536000,immutable",
        )
        if not public_asset_is_exact(immutable_url, manifest_path):
            raise RuntimeError("immutable manifest public read-back verification failed")

    # OSS replaces one object atomically. Publishing this pointer last prevents
    # customers from observing a manifest that references unavailable objects.
    oss_cp(
        manifest_path,
        f"oss://{bucket}/{prefix}/latest.json",
        "application/json",
        "no-cache, max-age=0",
    )
    latest_url = f"{public_base}/{prefix}/latest.json"
    if not public_asset_is_exact(latest_url, manifest_path):
        raise RuntimeError("latest manifest public read-back verification failed")
    print(f"published snapshot {manifest['snapshot_id']}")
    print(latest_url)


def run(args: argparse.Namespace) -> int:
    release_api = os.environ.get("GITHUB_RELEASE_API", DEFAULT_RELEASE_API)
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if args.release_json:
        release = json.loads(args.release_json.read_text(encoding="utf-8"))
    else:
        release = request_json(release_api, token)
    selected = select_assets(release)
    work_dir = Path(args.work_dir) if args.work_dir else Path(tempfile.mkdtemp(prefix="codex-artifacts-"))
    work_dir.mkdir(parents=True, exist_ok=True)
    downloads = work_dir / "downloads"
    downloads.mkdir(exist_ok=True)
    completed_assets = []
    for asset in selected:
        print(f"downloading {asset['name']} from {asset['upstream_url']}", flush=True)
        completed_assets.append(download_asset(asset, downloads / asset["name"]))
    prefix = os.environ.get("OSS_PREFIX", DEFAULT_PREFIX).strip("/")
    public_base = os.environ.get("OSS_PUBLIC_BASE_URL", DEFAULT_PUBLIC_BASE)
    manifest = manifest_for(release, completed_assets, public_base, prefix)
    validate_manifest(manifest)
    generated = work_dir / "generated-manifest.json"
    generated.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"prepared snapshot {manifest['snapshot_id']} at {generated}")
    if args.prepare_only:
        return 0
    current = fetch_existing_manifest(f"{public_base.rstrip('/')}/{prefix}/latest.json")
    if current and current.get("snapshot_id") == manifest["snapshot_id"]:
        # Keep the current fetched_at/immutable manifest stable, but fully verify
        # every content-addressed customer object before declaring a no-op.
        validate_manifest(current)
        if all(
            public_asset_is_exact(asset["mirror_url"], downloads / asset["name"])
            for asset in current["assets"]
        ):
            print(f"no changes; verified published snapshot {manifest['snapshot_id']}")
            return 0
    publish(manifest, {asset["name"]: asset["path"] for asset in completed_assets}, work_dir)
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--prepare-only", action="store_true", help="build and validate without OSS writes")
    parser.add_argument("--release-json", type=Path, help="release metadata fixture used by tests")
    parser.add_argument("--work-dir", help="retain downloads and generated manifest in this directory")
    return parser.parse_args()


if __name__ == "__main__":
    try:
        raise SystemExit(run(parse_args()))
    except Exception as error:
        print(f"codex artifact sync failed: {error}", file=sys.stderr)
        raise SystemExit(1)
