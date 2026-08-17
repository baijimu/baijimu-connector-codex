#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <version> <connector-manifest> <oss-manifest>" >&2
  exit 2
fi

version="$1"
connector_manifest_path="$2"
oss_manifest_path="$3"
publication_status_file="${MARKET_PUBLICATION_STATUS_FILE:-$PWD/.local-app-publication-status}"

for name in LOCAL_APP_MARKET_PUBLISH_TOKEN LOCAL_APP_OWNER_WORKSPACE_ID BAIJIMU_CLI; do
  test -n "${!name:-}" || { echo "$name is required" >&2; exit 2; }
done
test -x "$BAIJIMU_CLI"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]
[[ "$LOCAL_APP_OWNER_WORKSPACE_ID" =~ ^[1-9][0-9]*$ ]]

connector_manifest="$(jq -ce --arg version "$version" '
  select(.schemaVersion == "2.0")
  | select(.version == $version)
  | select(.source.type == "github")
  | select(.source.revision == ("v" + $version))
  | select(.runtime.type == "process")
  | select(.runtime.processOwnership == "host")
  | select(.runtime.args == ["start"])
  | select(.runtime.stopArgs == ["stop"])
  | select((.hostRequirements.minimumVersion | type) == "string")
  | select((.hostRequirements.capabilities // []) | index("connector.process.host-managed.v1") != null)
  | select((.managedToolDependencies | type) == "array")
' "$connector_manifest_path")"

oss_manifest="$(jq -ce --arg version "$version" '
  select(.schemaVersion == "1.0")
  | select(.version == $version)
  | select(.releaseTag == ("v" + $version))
  | select((.applicationId | length) > 0)
  | select(.connectorId == $connectorId)
  | select((.artifacts | length) == 3)
  | select(all(.artifacts[];
      (.source | startswith("https://download.baijimu.com/"))
      and (.checksum | test("^sha256:[0-9a-f]{64}$"))))
' --arg connectorId "$(printf '%s' "$connector_manifest" | jq -r .id)" "$oss_manifest_path")"

app_id="$(printf '%s' "$oss_manifest" | jq -er .applicationId)"
connector_id="$(printf '%s' "$connector_manifest" | jq -er .id)"
source_repo="$(printf '%s' "$connector_manifest" | jq -er .source.repo)"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/local-app-market.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

declare -A checksums
declare -A sources
for platform in macos windows linux; do
  checksum="$(printf '%s' "$oss_manifest" | jq -er --arg platform "$platform"     '.artifacts[] | select(.platform == $platform) | .checksum | sub("^sha256:"; "")')"
  source_url="$(printf '%s' "$oss_manifest" | jq -er --arg platform "$platform"     '.artifacts[] | select(.platform == $platform) | .source')"
  curl -fsSL --retry 6 --retry-all-errors --retry-delay 3     --connect-timeout 15 --max-time 900 "$source_url" -o "$work_dir/$platform.zip"
  test "$(sha256sum "$work_dir/$platform.zip" | awk '{print $1}')" = "$checksum"
  checksums[$platform]="$checksum"
  sources[$platform]="$source_url"
done

market_manifest="$(jq -nc   --argjson connector "$connector_manifest"   --arg mac_source "${sources[macos]}"   --arg win_source "${sources[windows]}"   --arg linux_source "${sources[linux]}"   --arg mac_sha "sha256:${checksums[macos]}"   --arg win_sha "sha256:${checksums[windows]}"   --arg linux_sha "sha256:${checksums[linux]}"   '($connector + {
    applicationType: "connector",
    artifacts: [
      {platform:"macos",arch:"universal",source:$mac_source,checksum:$mac_sha},
      {platform:"windows",arch:"x86_64",source:$win_source,checksum:$win_sha},
      {platform:"linux",arch:"x86_64",source:$linux_source,checksum:$linux_sha}
    ]
  })')"
capabilities="$(printf '%s' "$connector_manifest" | jq -c '[.remoteCapabilities[]?.name]')"
publish_body="$work_dir/version.json"
jq -n   --arg version "$version"   --arg source "${sources[macos]}"   --arg repo "$source_repo"   --arg revision "v$version"   --arg checksum "${checksums[macos]}"   --argjson capabilities "$capabilities"   --argjson manifest "$market_manifest"   '{version:$version,sourceType:"https",source:$source,repo:$repo,revision:$revision,
    checksum:$checksum,capabilities:$capabilities,manifest:$manifest}' > "$publish_body"

auth_file="$work_dir/baijimu-auth.json"
BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" auth login   --token "$LOCAL_APP_MARKET_PUBLISH_TOKEN"   --workspace-id "$LOCAL_APP_OWNER_WORKSPACE_ID"   --no-browser --json >/dev/null

app_json="$(BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app get "$app_id" --json)"
printf '%s' "$app_json" | jq -e   --arg appId "$app_id"   --arg connectorId "$connector_id"   --argjson ownerWorkspaceId "$LOCAL_APP_OWNER_WORKSPACE_ID" '
  (.data // .).id == $appId
  and (.data // .).connectorId == $connectorId
  and (.data // .).ownerWorkspaceId == $ownerWorkspaceId' >/dev/null

existing_version="$(printf '%s' "$app_json" | jq -c --arg version "$version"   'first((.data // .).versions[]? | select(.version == $version)) // empty')"
if [ -n "$existing_version" ]; then
  printf '%s' "$existing_version" | jq -e     --arg source "${sources[macos]}"     --arg repo "$source_repo"     --arg revision "v$version"     --arg checksum "${checksums[macos]}"     --argjson expected_manifest "$market_manifest" '
      .source == $source and .repo == $repo and .revision == $revision
      and .checksum == $checksum and .manifest == $expected_manifest' >/dev/null
else
  BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app version create "$app_id"     --data "@$publish_body" --json | jq -e '.errorCode == "0"' >/dev/null
fi

read_publication() {
  BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app publications --json     | jq -c --arg appId "$app_id" --arg version "$version"       'first((.data // .)[]? | select(.appId == $appId and .version == $version)) // empty'
}
publication="$(read_publication)"
status="$(printf '%s' "$publication" | jq -r '.status // empty')"
case "$status" in
  PENDING_REVIEW|PUBLISHED) ;;
  "")
    BAIJIMU_AUTH_FILE="$auth_file" "$BAIJIMU_CLI" local-app submit "$app_id" "$version" --json       | jq -e '.errorCode == "0"' >/dev/null
    ;;
  *) echo "market publication is not retryable: $status" >&2; exit 1 ;;
esac

for _ in $(seq 1 20); do
  publication="$(read_publication)"
  status="$(printf '%s' "$publication" | jq -r '.status // empty')"
  case "$status" in
    PENDING_REVIEW|PUBLISHED)
      printf '%s\n' "$status" > "$publication_status_file"
      echo "submitted $app_id $version; status=$status"
      exit 0
      ;;
  esac
  sleep 3
done
echo "market publication status did not become visible before timeout" >&2
exit 1

