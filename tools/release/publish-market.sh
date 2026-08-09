#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <version> <connector-manifest> <oss-manifest>" >&2
  exit 2
fi

version="$1"
connector_manifest_path="$2"
oss_manifest_path="$3"
publication_status_file="${MARKET_PUBLICATION_STATUS_FILE:-$PWD/.codex-local-app-publication-status}"

if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid connector version: $version" >&2
  exit 2
fi
for file in "$connector_manifest_path" "$oss_manifest_path"; do
  if [ ! -f "$file" ]; then
    echo "required release manifest does not exist: $file" >&2
    exit 2
  fi
done
for dependency in curl jq sha256sum awk grep seq; do
  command -v "$dependency" >/dev/null 2>&1 || {
    echo "required market publisher dependency is unavailable: $dependency" >&2
    exit 127
  }
done

: "${LOCAL_APP_MARKET_PUBLISH_TOKEN:?LOCAL_APP_MARKET_PUBLISH_TOKEN is required}"
BAIJIMU_CLI="${BAIJIMU_CLI:-$(command -v baijimu || true)}"
if [ -z "$BAIJIMU_CLI" ] || [ ! -x "$BAIJIMU_CLI" ]; then
  echo "Baijimu CLI is required; set BAIJIMU_CLI to the pinned release binary" >&2
  exit 127
fi

connector_manifest="$(jq -ce \
  --arg version "$version" \
  '
    select(.schemaVersion == "2.0")
    | select(.id == "com.baijimu.connector.codex")
    | select(.version == $version)
    | select(.source.type == "github")
    | select(.source.repo == "momoplan/baijimu-connector-codex")
    | select(.source.revision == ("v" + $version))
    | select(.runtime.type == "process")
    | select((.runtime.command | type) == "string" and (.runtime.command | length) > 0)
    | select(.runtime.processOwnership == "host")
    | select(.runtime.args == ["start"])
    | select(.runtime.stopArgs == ["stop"])
    | select(.hostRequirements.minimumVersion == "0.2.40")
    | select((.hostRequirements.capabilities // []) | index("connector.process.host-managed.v1") != null)
  ' "$connector_manifest_path")" || {
  echo "connector manifest identity, GitHub source, version, or runtime contract is invalid" >&2
  exit 2
}

oss_manifest="$(jq -ce \
  --arg version "$version" \
  '
    select(.schemaVersion == "1.0")
    | select(.applicationId == "codex")
    | select(.connectorId == "com.baijimu.connector.codex")
    | select(.releaseTag == ("v" + $version))
    | select(.version == $version)
    | select((.artifacts | length) == 3)
    | select(all(.artifacts[];
        (.source | startswith("https://download.baijimu.com/local-app-artifacts/codex/releases/v" + $version + "/"))
        and (.checksum | test("^sha256:[0-9a-f]{64}$"))))
  ' "$oss_manifest_path")" || {
  echo "OSS manifest is invalid" >&2
  exit 2
}

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/codex-local-app-market.XXXXXX")"
cleanup() {
  rm -rf "$work_dir"
}
trap cleanup EXIT

declare -A checksums
declare -A sources
for platform in macos windows linux; do
  checksum="$(printf '%s' "$oss_manifest" | jq -er \
    --arg platform "$platform" \
    '.artifacts[] | select(.platform == $platform) | .checksum | sub("^sha256:"; "")')"
  source_url="$(printf '%s' "$oss_manifest" | jq -er \
    --arg platform "$platform" \
    '.artifacts[] | select(.platform == $platform) | .source')"
  curl -fsSL --retry 6 --retry-all-errors --retry-delay 3 \
    --connect-timeout 15 --max-time 900 \
    "$source_url" -o "$work_dir/$platform.zip"
  actual_checksum="$(sha256sum "$work_dir/$platform.zip" | awk '{print $1}')"
  if [ "$actual_checksum" != "$checksum" ]; then
    echo "anonymous OSS checksum mismatch for $platform" >&2
    exit 1
  fi
  checksums[$platform]="$checksum"
  sources[$platform]="$source_url"
done

market_manifest="$(jq -nc \
  --argjson connector "$connector_manifest" \
  --arg mac_source "${sources[macos]}" \
  --arg win_source "${sources[windows]}" \
  --arg linux_source "${sources[linux]}" \
  --arg mac_sha "sha256:${checksums[macos]}" \
  --arg win_sha "sha256:${checksums[windows]}" \
  --arg linux_sha "sha256:${checksums[linux]}" \
  '($connector + {
    applicationType: "connector",
    artifacts: [
      {platform: "macos", arch: "universal", source: $mac_source, checksum: $mac_sha},
      {platform: "windows", arch: "x86_64", source: $win_source, checksum: $win_sha},
      {platform: "linux", arch: "x86_64", source: $linux_source, checksum: $linux_sha}
    ]
  })')"
capabilities="$(printf '%s' "$connector_manifest" | jq -c '[.remoteCapabilities[]?.name]')"
publish_body="$work_dir/version.json"
jq -n \
  --arg version "$version" \
  --arg source "${sources[macos]}" \
  --arg repo "momoplan/baijimu-connector-codex" \
  --arg revision "v${version}" \
  --arg checksum "${checksums[macos]}" \
  --argjson capabilities "$capabilities" \
  --argjson manifest "$market_manifest" \
  '{version:$version,sourceType:"https",source:$source,repo:$repo,revision:$revision,
    checksum:$checksum,capabilities:$capabilities,manifest:$manifest}' \
  > "$publish_body"

auth_file="$work_dir/baijimu-auth.json"
BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" auth login \
  --token "$LOCAL_APP_MARKET_PUBLISH_TOKEN" \
  --workspace-id 1211 \
  --no-browser \
  --json \
  >/dev/null

app_json="$(BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app get codex --json)"
printf '%s' "$app_json" | jq -e \
  '(.data // .).id == "codex" and
   (.data // .).connectorId == "com.baijimu.connector.codex" and
   (.data // .).ownerWorkspaceId == 1211' \
  >/dev/null

existing_version="$(printf '%s' "$app_json" | jq -c \
  --arg version "$version" \
  'first((.data // .).versions[]? | select(.version == $version)) // empty')"
if [ -n "$existing_version" ]; then
  printf '%s' "$existing_version" | jq -e \
    --arg version "$version" \
    --arg source "${sources[macos]}" \
    --arg revision "v${version}" \
    --arg checksum "${checksums[macos]}" \
    --argjson expected_manifest "$market_manifest" \
    '.source == $source and
     .repo == "momoplan/baijimu-connector-codex" and
     .revision == $revision and
     .checksum == $checksum and
     .manifest == $expected_manifest' \
    >/dev/null || {
    echo "existing market version differs from the immutable release" >&2
    exit 1
  }
  echo "immutable market version $version already exists and matches"
else
  create_response="$(BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app version create codex \
    --data "@$publish_body" --json)"
  printf '%s' "$create_response" | jq -e \
    --arg version "$version" \
    '(.errorCode == "0") and ((.data.version // .data.appVersion.version) == $version)' \
    >/dev/null
  echo "created immutable market version $version"
fi

read_publication() {
  BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app publications --json \
    | jq -c --arg version "$version" \
      'first((.data // .)[]? | select(.appId == "codex" and .version == $version)) // empty'
}

publication="$(read_publication)"
publication_status="$(printf '%s' "$publication" | jq -r '.status // empty')"
case "$publication_status" in
  PENDING_REVIEW|PUBLISHED)
    echo "market version $version already has publication status $publication_status"
    ;;
  "")
    submit_response="$(BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app submit codex "$version" --json)"
    printf '%s' "$submit_response" | jq -e \
      '(.errorCode == "0") and
       ((.data.status // .data.publication.status) == "PENDING_REVIEW" or
        (.data.status // .data.publication.status) == "PUBLISHED")' \
      >/dev/null
    ;;
  *)
    echo "market publication is not retryable: $publication_status" >&2
    exit 1
    ;;
esac

for attempt in $(seq 1 20); do
  publication="$(read_publication)"
  publication_status="$(printf '%s' "$publication" | jq -r '.status // empty')"
  case "$publication_status" in
    PENDING_REVIEW|PUBLISHED)
      printf '%s\n' "$publication_status" > "$publication_status_file"
      publication_id="$(printf '%s' "$publication" | jq -r '.id // empty')"
      echo "submitted Codex local app $version; publication=${publication_id:-unknown}; status=$publication_status"
      exit 0
      ;;
  esac
  sleep 3
done

echo "market publication status did not become visible before timeout" >&2
exit 1
