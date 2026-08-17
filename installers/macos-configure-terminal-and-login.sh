#!/usr/bin/env bash
set -Eeuo pipefail
export PATH="$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin"

verify_sha256() {
  local archive="$1"
  local expected="$2"
  local actual
  actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
  if [ "$actual" != "$expected" ]; then
    echo "安装制品 SHA256 不匹配：$(basename "$archive")" >&2
    return 1
  fi
}

install_app() {
  local dmg="$1"
  local expected_sha256="$2"
  local mount_dir source_app app_path
  verify_sha256 "$dmg" "$expected_sha256"

  mount_dir="$(mktemp -d "${TMPDIR:-/tmp}/codex-dmg.XXXXXX")"
  cleanup_mount() {
    if mount | grep -Fq " on $mount_dir "; then
      hdiutil detach "$mount_dir" -quiet || true
    fi
    rm -rf "$mount_dir"
  }
  trap cleanup_mount EXIT

  hdiutil attach "$dmg" -mountpoint "$mount_dir" -nobrowse -quiet
  if [ -d "$mount_dir/ChatGPT.app" ]; then
    source_app="$mount_dir/ChatGPT.app"
  elif [ -d "$mount_dir/Codex.app" ]; then
    source_app="$mount_dir/Codex.app"
  else
    source_app="$(find "$mount_dir" -maxdepth 1 -name '*.app' -type d | head -n 1)"
  fi
  if [ -z "${source_app:-}" ] || [ ! -d "$source_app" ]; then
    echo "DMG 中未找到受支持的 ChatGPT 桌面应用包" >&2
    return 1
  fi

  app_path="/Applications/$(basename "$source_app")"
  ditto "$source_app" "$app_path"
  hdiutil detach "$mount_dir" -quiet
  rm -rf "$mount_dir"
  trap - EXIT
  xattr -dr com.apple.quarantine "$app_path" 2>/dev/null || true
  test -d "$app_path"
  printf '%s\n' "$app_path"
}

action="${1:-}"
case "$action" in
  install-app)
    [ "$#" -eq 3 ] || { echo "install-app 参数无效" >&2; exit 2; }
    install_app "$2" "$3"
    ;;
  *)
    echo "不支持的 macOS 原生安装动作：${action:-<empty>}" >&2
    exit 2
    ;;
esac
