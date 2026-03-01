#!/usr/bin/env bash
set -euo pipefail

REPO="shantanugoel/oxydra"
TAG=""
INSTALL_DIR="${OXYDRA_INSTALL_DIR:-$HOME/.local/bin}"
SYSTEM_INSTALL=false
BASE_DIR="."
SKIP_CONFIG=false
OVERWRITE_CONFIG=false
FORCE=false
AUTO_YES=false
NO_PULL=false
DRY_RUN=false
BACKUP_ROOT_OVERRIDE=""

SCRIPT_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)"
fi

PLATFORM=""
ARCHIVE=""
DOWNLOAD_URL=""
TMP_DIR=""
CONFIG_ROOT=""
RUNNER_CONFIG=""
RUNNER_BIN=""
CURRENT_RUNNER_BIN=""
CURRENT_VERSION="unknown"
CURRENT_VERSION_NORMALIZED=""
TARGET_VERSION_NORMALIZED=""
BACKUP_ROOT=""
BACKUP_PATH=""
ROLLBACK_READY=false
ROLLBACK_IN_PROGRESS=false
RUNNER_CONFIG_EXISTED_BEFORE=false
DAEMON_WAS_RUNNING=false
ACTIVE_USERS=()
STOPPED_USERS=()

binaries=(runner oxydra-vm shell-daemon oxydra-tui)

usage() {
  cat <<'EOF'
Install Oxydra binaries from GitHub Releases.

Usage:
  install-release.sh [options]

Options:
  --tag <tag>            Install a specific release tag (for example: v0.3.0)
                         If omitted, installs the latest release.
  --repo <owner/name>    GitHub repository (default: shantanugoel/oxydra)
  --install-dir <path>   Target directory for binaries (default: ~/.local/bin)
  --system               Install to /usr/local/bin (uses sudo when needed)
  --base-dir <path>      Base directory where .oxydra config templates are written
                         (default: current directory)
  --skip-config          Install binaries only (skip config initialization/updates)
  --overwrite-config     Replace existing .oxydra template files if present
  --force                Reinstall even when target tag matches current version
  --yes, -y              Non-interactive mode; answer yes to prompts
  --no-pull              Skip Docker guest image pre-pull
  --backup-dir <path>    Backup root directory (default: ~/.local/share/oxydra/backups)
  --dry-run              Print planned actions without making changes
  -h, --help             Show help

Examples:
  install-release.sh
  install-release.sh --tag v0.3.0
  install-release.sh --tag v0.3.0 --system
  install-release.sh --tag v0.3.0 --base-dir /path/to/workspace
  install-release.sh --tag v0.3.0 --yes
  install-release.sh --tag v0.3.0 --dry-run
EOF
}

log() {
  printf '[oxydra-install] %s\n' "$*"
}

warn() {
  printf '[oxydra-install] Warning: %s\n' "$*" >&2
}

confirm_default_yes() {
  local prompt="$1"
  if [[ "$AUTO_YES" == "true" ]]; then
    log "${prompt} [auto-yes]"
    return 0
  fi

  if [[ ! -t 0 ]]; then
    return 1
  fi

  local reply
  printf '[oxydra-install] %s [Y/n] ' "$prompt" >&2
  read -r reply || return 1
  local lowered
  lowered="$(printf '%s' "$reply" | tr '[:upper:]' '[:lower:]')"
  case "$lowered" in
    ""|y|yes) return 0 ;;
    *) return 1 ;;
  esac
}

normalize_version() {
  local raw="$1"
  raw="${raw#v}"
  if printf '%s\n' "$raw" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    printf '%s' "$raw"
    return 0
  fi
  local extracted
  extracted="$(printf '%s\n' "$raw" | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1 || true)"
  [[ -n "$extracted" ]] || return 1
  printf '%s' "$extracted"
}

compare_versions() {
  local a="$1"
  local b="$2"
  local -a av bv
  local IFS=.
  read -r -a av <<<"$a"
  read -r -a bv <<<"$b"

  local i ai bi
  for i in 0 1 2; do
    ai="${av[$i]:-0}"
    bi="${bv[$i]:-0}"
    if (( ai > bi )); then
      printf '1'
      return 0
    fi
    if (( ai < bi )); then
      printf '%s' '-1'
      return 0
    fi
  done
  printf '0'
}

resolve_latest_tag() {
  local response
  local api_url="https://api.github.com/repos/${REPO}/releases/latest"

  if ! response="$(curl -fsSL \
    -H 'Accept: application/vnd.github+json' \
    -H 'User-Agent: oxydra-install-script' \
    "$api_url")"; then
    fail "failed to query latest release from ${api_url}. Check network access or pass --tag explicitly."
  fi

  local latest
  latest="$(printf '%s\n' "$response" | sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [[ -n "$latest" ]] || fail "could not determine latest release tag from GitHub API response"
  printf '%s' "$latest"
}

detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64)
          printf '%s' "macos-arm64"
          ;;
        x86_64|amd64)
          fail "macOS x86_64 release artifacts are not published. Build from source instead."
          ;;
        *)
          fail "unsupported macOS architecture: ${arch}"
          ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64)
          printf '%s' "linux-amd64"
          ;;
        aarch64|arm64)
          printf '%s' "linux-arm64"
          ;;
        *)
          fail "unsupported Linux architecture: ${arch}"
          ;;
      esac
      ;;
    *)
      fail "unsupported OS: ${os}. Supported: macOS (arm64), Linux (amd64/arm64)."
      ;;
  esac
}

install_binary_local() {
  local source="$1"
  local destination="$2"

  if command -v install >/dev/null 2>&1; then
    install -m 0755 "$source" "$destination"
  else
    cp "$source" "$destination"
    chmod 0755 "$destination"
  fi
}

install_binary_system() {
  local source="$1"
  local destination="$2"

  if command -v install >/dev/null 2>&1; then
    sudo install -m 0755 "$source" "$destination"
  else
    sudo cp "$source" "$destination"
    sudo chmod 0755 "$destination"
  fi
}

should_use_sudo_for_install() {
  [[ "$SYSTEM_INSTALL" == "true" && "$(id -u)" -ne 0 ]]
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return 0
  fi
  fail "sha256sum or shasum is required for checksum verification"
}

copy_or_download_config_template_impl() {
  local source_name="$1"
  local destination="$2"
  local force_write="$3"
  local archive_source="${TMP_DIR}/examples/config/${source_name}"
  local local_source="${SCRIPT_DIR}/../examples/config/${source_name}"

  if [[ -f "$destination" && "$force_write" != "true" && "$OVERWRITE_CONFIG" != "true" ]]; then
    log "Config exists, leaving unchanged: ${destination}"
    return 1
  fi

  mkdir -p "$(dirname "$destination")"

  # Prefer config bundled in the release archive.
  if [[ -f "$archive_source" ]]; then
    cp "$archive_source" "$destination"
    log "Copied template: ${destination}"
    return 0
  fi

  # Fall back to local repo checkout (when running from source tree).
  if [[ -n "$SCRIPT_DIR" && -f "$local_source" ]]; then
    cp "$local_source" "$destination"
    log "Copied template: ${destination}"
    return 0
  fi

  # Last resort: download from GitHub.
  local source_url="https://raw.githubusercontent.com/${REPO}/${TAG}/examples/config/${source_name}"
  if ! curl -fL --retry 3 -o "$destination" "$source_url"; then
    fail "failed to fetch config template ${source_name} from ${source_url}"
  fi
  log "Downloaded template: ${destination}"
  return 0
}

copy_or_download_config_template() {
  copy_or_download_config_template_impl "$1" "$2" "false"
}

copy_or_download_config_template_force() {
  copy_or_download_config_template_impl "$1" "$2" "true"
}

patch_runner_template_defaults() {
  local runner_config="$1"
  local patched="${runner_config}.patched"
  [[ -f "$runner_config" ]] || return

  awk -v tag="$TAG" '
    /^[[:space:]]*workspace_root[[:space:]]*=/ {
      print "workspace_root = \"workspaces\""
      next
    }
    /^[[:space:]]*oxydra_vm[[:space:]]*=/ {
      print "oxydra_vm = \"ghcr.io/shantanugoel/oxydra-vm:" tag "\""
      next
    }
    /^[[:space:]]*shell_vm[[:space:]]*=/ {
      print "shell_vm  = \"ghcr.io/shantanugoel/shell-vm:" tag "\""
      next
    }
    { print }
  ' "$runner_config" > "$patched"

  mv "$patched" "$runner_config"
}

extract_image_ref_from_runner_config() {
  local field="$1"
  local config_path="$2"
  sed -nE "s/^[[:space:]]*${field}[[:space:]]*=[[:space:]]*\"([^\"]*)\".*/\1/p" "$config_path" | head -n 1
}

extract_tag_from_image_ref() {
  local image_ref="$1"
  if [[ "$image_ref" == *:* ]]; then
    printf '%s' "${image_ref##*:}"
  else
    printf '%s' "latest"
  fi
}

extract_image_tag_from_runner_config() {
  local field="$1"
  local config_path="$2"
  local image_ref
  image_ref="$(extract_image_ref_from_runner_config "$field" "$config_path" || true)"
  if [[ -z "$image_ref" ]]; then
    printf '%s' ""
    return 0
  fi
  extract_tag_from_image_ref "$image_ref"
}

update_runner_guest_image_tags() {
  local runner_config="$1"
  local patched="${runner_config}.patched"
  [[ -f "$runner_config" ]] || return

  local old_oxydra old_shell new_oxydra new_shell
  old_oxydra="$(extract_image_tag_from_runner_config "oxydra_vm" "$runner_config" || true)"
  old_shell="$(extract_image_tag_from_runner_config "shell_vm" "$runner_config" || true)"

  awk -v tag="$TAG" '
    function update_line(line,    eq,right,q1,rest,q2,image,new_image,pre,tail) {
      eq = index(line, "=")
      if (eq == 0) {
        return line
      }

      right = substr(line, eq + 1)
      q1 = index(right, "\"")
      if (q1 == 0) {
        return line
      }

      rest = substr(right, q1 + 1)
      q2 = index(rest, "\"")
      if (q2 == 0) {
        return line
      }

      image = substr(rest, 1, q2 - 1)
      if (match(image, /:[^:]*$/)) {
        new_image = substr(image, 1, RSTART) tag
      } else {
        new_image = image ":" tag
      }

      pre = substr(right, 1, q1)
      tail = substr(rest, q2 + 1)
      return substr(line, 1, eq) pre new_image "\"" tail
    }

    {
      if ($0 ~ /^[[:space:]]*oxydra_vm[[:space:]]*=/) {
        print update_line($0)
        next
      }
      if ($0 ~ /^[[:space:]]*shell_vm[[:space:]]*=/) {
        print update_line($0)
        next
      }
      print
    }
  ' "$runner_config" > "$patched"

  mv "$patched" "$runner_config"

  new_oxydra="$(extract_image_tag_from_runner_config "oxydra_vm" "$runner_config" || true)"
  new_shell="$(extract_image_tag_from_runner_config "shell_vm" "$runner_config" || true)"

  if [[ -n "$new_oxydra" && "$new_oxydra" != "$old_oxydra" ]]; then
    log "Updated guest_images.oxydra_vm tag: ${old_oxydra:-<none>} -> ${new_oxydra}"
  fi
  if [[ -n "$new_shell" && "$new_shell" != "$old_shell" ]]; then
    log "Updated guest_images.shell_vm tag: ${old_shell:-<none>} -> ${new_shell}"
  fi
}

cleanup_old_new_templates() {
  local config_root="$1"
  local current_tag="$2"
  local file path keep_path

  for file in runner.toml agent.toml runner-user.toml; do
    keep_path="${config_root}/${file}.${current_tag}.new"
    for path in "${config_root}/${file}.v"*.new; do
      [[ -e "$path" ]] || continue
      if [[ "$path" != "$keep_path" ]]; then
        rm -f "$path"
      fi
    done
  done
}

save_versioned_templates_for_diff() {
  local config_root="$1"
  local runner_new="${config_root}/runner.toml.${TAG}.new"
  local agent_new="${config_root}/agent.toml.${TAG}.new"
  local user_new="${config_root}/runner-user.toml.${TAG}.new"

  copy_or_download_config_template_force "runner.toml" "$runner_new"
  patch_runner_template_defaults "$runner_new"
  copy_or_download_config_template_force "agent.toml" "$agent_new"
  copy_or_download_config_template_force "runner-user.toml" "$user_new"

  cleanup_old_new_templates "$config_root" "$TAG"

  log "New config template saved: ${runner_new}"
  log "New config template saved: ${agent_new}"
  log "New config template saved: ${user_new}"
  log "Review changes with:"
  log "  diff ${config_root}/runner.toml ${runner_new}"
  log "  diff ${config_root}/agent.toml ${agent_new}"
}

initialize_config_templates() {
  local base_dir="$1"
  local config_root="${base_dir}/.oxydra"
  local users_dir="${config_root}/users"
  local runner_config="${config_root}/runner.toml"

  RUNNER_CONFIG_EXISTED_BEFORE=false
  if [[ -f "$runner_config" ]]; then
    RUNNER_CONFIG_EXISTED_BEFORE=true
  fi

  mkdir -p "$users_dir"

  copy_or_download_config_template "agent.toml" "${config_root}/agent.toml" || true
  copy_or_download_config_template "runner.toml" "$runner_config" || true
  copy_or_download_config_template "runner-user.toml" "${users_dir}/alice.toml" || true

  if [[ -f "$runner_config" ]]; then
    if [[ "$RUNNER_CONFIG_EXISTED_BEFORE" == "true" && "$OVERWRITE_CONFIG" != "true" ]]; then
      update_runner_guest_image_tags "$runner_config"
      save_versioned_templates_for_diff "$config_root"
    else
      patch_runner_template_defaults "$runner_config"
    fi
  fi

  cat <<EOF
[oxydra-install] Config templates are ready in ${config_root}
[oxydra-install] Update these values before first run:
  1) ${config_root}/runner.toml
     - set default_tier = "container" (or "process" / "micro_vm")
     - verify [guest_images] tags match ${TAG}
  2) On Linux, ensure Docker is running and your user is in the docker group:
       sudo systemctl enable --now docker
       sudo usermod -aG docker \$USER && newgrp docker
      The guest images are public on ghcr.io and pull without authentication.
      If you see a 404 "manifest unknown" error, verify the tag in runner.toml
      includes the "v" prefix (e.g. ${TAG}, not ${TAG#v}).
  3) ${config_root}/agent.toml
     - set [selection].provider and [selection].model
     - ensure matching [providers.registry.<name>] api_key_env is correct
  4) Export your provider API key environment variable:
     OPENAI_API_KEY or ANTHROPIC_API_KEY or GEMINI_API_KEY
EOF
}

parse_runner_users() {
  local config_path="$1"
  sed -nE 's/^[[:space:]]*\[users\.([^]]+)\][[:space:]]*$/\1/p' "$config_path"
}

resolve_workspace_root_abs() {
  local config_path="$1"
  local config_dir workspace_root
  config_dir="$(cd "$(dirname "$config_path")" && pwd)"
  workspace_root="$(sed -nE 's/^[[:space:]]*workspace_root[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$config_path" | head -n 1)"
  if [[ -z "$workspace_root" ]]; then
    workspace_root="workspaces"
  fi

  if [[ "$workspace_root" == /* ]]; then
    printf '%s' "$workspace_root"
  else
    printf '%s' "${config_dir}/${workspace_root}"
  fi
}

control_socket_path_for_user() {
  local config_path="$1"
  local user_id="$2"
  local workspace_root_abs
  workspace_root_abs="$(resolve_workspace_root_abs "$config_path")"
  printf '%s' "${workspace_root_abs}/${user_id}/ipc/runner-control.sock"
}

collect_active_runner_users() {
  ACTIVE_USERS=()
  [[ -f "$RUNNER_CONFIG" ]] || return

  local users user socket_path is_active workspace_root_abs existing already_present
  workspace_root_abs="$(resolve_workspace_root_abs "$RUNNER_CONFIG")"
  users="$(parse_runner_users "$RUNNER_CONFIG" || true)"
  while IFS= read -r user; do
    [[ -n "$user" ]] || continue
    is_active=false
    socket_path="${workspace_root_abs}/${user}/ipc/runner-control.sock"

    if [[ -x "$CURRENT_RUNNER_BIN" ]] && "$CURRENT_RUNNER_BIN" --config "$RUNNER_CONFIG" --user "$user" status >/dev/null 2>&1; then
      is_active=true
    fi

    if [[ "$is_active" != "true" && -S "$socket_path" ]]; then
      is_active=true
    fi

    if [[ "$is_active" == "true" ]]; then
      ACTIVE_USERS+=("$user")
    fi
  done <<<"$users"

  for socket_path in "${workspace_root_abs}"/*/ipc/runner-control.sock; do
    [[ -S "$socket_path" ]] || continue
    user="$(basename "$(dirname "$(dirname "$socket_path")")")"
    already_present=false
    for existing in "${ACTIVE_USERS[@]}"; do
      if [[ "$existing" == "$user" ]]; then
        already_present=true
        break
      fi
    done
    if [[ "$already_present" != "true" ]]; then
      ACTIVE_USERS+=("$user")
    fi
  done
}

wait_for_socket_removal() {
  local socket_path="$1"
  local timeout_secs="$2"
  local elapsed=0
  while [[ -e "$socket_path" && "$elapsed" -lt "$timeout_secs" ]]; do
    sleep 1
    elapsed=$((elapsed + 1))
  done
  [[ ! -e "$socket_path" ]]
}

stop_active_runner_daemons() {
  STOPPED_USERS=()
  if [[ "${#ACTIVE_USERS[@]}" -eq 0 ]]; then
    return
  fi

  DAEMON_WAS_RUNNING=true
  log "Runner daemon is currently active for user(s): ${ACTIVE_USERS[*]}"

  if [[ "$DRY_RUN" == "true" ]]; then
    log "Dry-run: would stop active runner daemon(s) before upgrade."
    STOPPED_USERS=("${ACTIVE_USERS[@]}")
    return
  fi

  if [[ ! -x "$CURRENT_RUNNER_BIN" ]]; then
    fail "could not stop running daemon. Stop it manually before upgrading."
  fi

  if ! confirm_default_yes "It must be stopped before upgrading. Stop it now?"; then
    fail "could not stop running daemon. Stop it manually before upgrading."
  fi

  local user socket_path
  for user in "${ACTIVE_USERS[@]}"; do
    socket_path="$(control_socket_path_for_user "$RUNNER_CONFIG" "$user")"
    if ! "$CURRENT_RUNNER_BIN" --config "$RUNNER_CONFIG" --user "$user" stop >/dev/null 2>&1; then
      if [[ ! -e "$socket_path" ]]; then
        STOPPED_USERS+=("$user")
        continue
      fi
      fail "could not stop running daemon. Stop it manually before upgrading."
    fi

    if ! wait_for_socket_removal "$socket_path" 15; then
      fail "could not stop running daemon. Stop it manually before upgrading."
    fi

    STOPPED_USERS+=("$user")
  done
}

copy_existing_binary_to_backup() {
  local source="$1"
  local destination="$2"
  [[ -f "$source" ]] || return

  if should_use_sudo_for_install; then
    sudo cp "$source" "$destination"
  else
    cp "$source" "$destination"
  fi
}

rotate_backups() {
  local backup_root="$1"
  local count=0
  local path
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    count=$((count + 1))
    if [[ "$count" -gt 3 ]]; then
      rm -rf "$path"
    fi
  done < <(ls -1dt "${backup_root}"/* 2>/dev/null || true)
}

create_backup() {
  local has_state=false
  local binary
  for binary in "${binaries[@]}"; do
    if [[ -f "${INSTALL_DIR}/${binary}" ]]; then
      has_state=true
      break
    fi
  done
  if [[ -d "$CONFIG_ROOT" ]]; then
    has_state=true
  fi

  if [[ "$has_state" != "true" ]]; then
    return
  fi

  if [[ "$BACKUP_ROOT" != /* ]]; then
    BACKUP_ROOT="${BASE_DIR}/${BACKUP_ROOT}"
  fi
  mkdir -p "$BACKUP_ROOT"

  local timestamp version_label
  timestamp="$(date +%Y%m%d-%H%M%S)"
  version_label="$CURRENT_VERSION"
  if [[ -z "$version_label" || "$version_label" == "unknown" ]]; then
    version_label="unknown"
  fi

  BACKUP_PATH="${BACKUP_ROOT}/${version_label}-${timestamp}"
  mkdir -p "${BACKUP_PATH}/binaries"

  for binary in "${binaries[@]}"; do
    copy_existing_binary_to_backup "${INSTALL_DIR}/${binary}" "${BACKUP_PATH}/binaries/${binary}"
  done

  if [[ -d "$CONFIG_ROOT" ]]; then
    mkdir -p "${BACKUP_PATH}/config"
    cp -R "$CONFIG_ROOT" "${BACKUP_PATH}/config/"
  fi

  rotate_backups "$BACKUP_ROOT"
  ROLLBACK_READY=true
  log "Backup created: ${BACKUP_PATH}"
}

restore_from_backup() {
  if [[ -z "$BACKUP_PATH" || ! -d "$BACKUP_PATH" ]]; then
    return 1
  fi

  ROLLBACK_IN_PROGRESS=true
  local restore_ok=true
  local binary source destination

  if [[ "$SYSTEM_INSTALL" == "true" ]]; then
    if [[ "$(id -u)" -eq 0 ]]; then
      mkdir -p "$INSTALL_DIR" || restore_ok=false
    else
      if ! command -v sudo >/dev/null 2>&1; then
        restore_ok=false
      else
        sudo mkdir -p "$INSTALL_DIR" || restore_ok=false
      fi
    fi
  else
    mkdir -p "$INSTALL_DIR" || restore_ok=false
  fi

  if [[ -d "${BACKUP_PATH}/binaries" ]]; then
    for binary in "${binaries[@]}"; do
      source="${BACKUP_PATH}/binaries/${binary}"
      destination="${INSTALL_DIR}/${binary}"
      [[ -f "$source" ]] || continue
      if [[ "$SYSTEM_INSTALL" == "true" ]]; then
        if [[ "$(id -u)" -eq 0 ]]; then
          install_binary_local "$source" "$destination" || restore_ok=false
        else
          install_binary_system "$source" "$destination" || restore_ok=false
        fi
      else
        install_binary_local "$source" "$destination" || restore_ok=false
      fi
    done
  fi

  if [[ -d "${BACKUP_PATH}/config/.oxydra" ]]; then
    rm -rf "$CONFIG_ROOT" || restore_ok=false
    mkdir -p "$(dirname "$CONFIG_ROOT")" || restore_ok=false
    cp -R "${BACKUP_PATH}/config/.oxydra" "$CONFIG_ROOT" || restore_ok=false
  fi

  ROLLBACK_IN_PROGRESS=false
  [[ "$restore_ok" == "true" ]]
}

maybe_offer_rollback() {
  if [[ "$ROLLBACK_READY" != "true" || "$ROLLBACK_IN_PROGRESS" == "true" ]]; then
    return
  fi

  if ! confirm_default_yes "Installation failed. Restore from backup?"; then
    return
  fi

  log "Restoring from backup: ${BACKUP_PATH}"
  if restore_from_backup; then
    log "Rollback complete."
  else
    warn "rollback failed; restore manually from ${BACKUP_PATH}"
  fi
}

fail() {
  local message="$*"
  maybe_offer_rollback
  printf '[oxydra-install] Error: %s\n' "$message" >&2
  exit 1
}

pre_pull_images() {
  if [[ "$NO_PULL" == "true" ]]; then
    log "Skipping Docker image pre-pull (--no-pull)"
    return
  fi

  if ! command -v docker >/dev/null 2>&1; then
    log "Docker is not available; skipping image pre-pull."
    return
  fi

  local oxydra_ref shell_ref
  oxydra_ref=""
  shell_ref=""
  if [[ -f "$RUNNER_CONFIG" ]]; then
    oxydra_ref="$(extract_image_ref_from_runner_config "oxydra_vm" "$RUNNER_CONFIG" || true)"
    shell_ref="$(extract_image_ref_from_runner_config "shell_vm" "$RUNNER_CONFIG" || true)"
  fi
  [[ -n "$oxydra_ref" ]] || oxydra_ref="ghcr.io/shantanugoel/oxydra-vm:${TAG}"
  [[ -n "$shell_ref" ]] || shell_ref="ghcr.io/shantanugoel/shell-vm:${TAG}"

  if [[ "$(uname -s)" == "Darwin" ]]; then
    log "Pre-pulling guest images (Docker Desktop must be running)..."
  else
    log "Pre-pulling guest images for ${TAG}..."
  fi

  if ! docker pull "$oxydra_ref"; then
    warn "failed to pre-pull ${oxydra_ref}; continuing."
  fi
  if ! docker pull "$shell_ref"; then
    warn "failed to pre-pull ${shell_ref}; continuing."
  fi
}

restart_stopped_daemons() {
  if [[ "${#STOPPED_USERS[@]}" -eq 0 ]]; then
    return
  fi
  if [[ "$DRY_RUN" == "true" ]]; then
    log "Dry-run: would offer daemon restart for user(s): ${STOPPED_USERS[*]}"
    return
  fi
  if [[ ! -x "$RUNNER_BIN" ]]; then
    return
  fi
  if ! confirm_default_yes "Restart the runner daemon for user(s): ${STOPPED_USERS[*]}?"; then
    return
  fi

  local user restart_log pid
  for user in "${STOPPED_USERS[@]}"; do
    restart_log="${CONFIG_ROOT}/restart-${user}.log"
    nohup "$RUNNER_BIN" --config "$RUNNER_CONFIG" --user "$user" start >"$restart_log" 2>&1 &
    pid=$!
    sleep 1
    if kill -0 "$pid" >/dev/null 2>&1; then
      log "Restarted runner for ${user} (pid: ${pid}, log: ${restart_log})"
    else
      warn "runner restart for ${user} exited quickly; inspect ${restart_log}"
    fi
  done
}

print_dry_run_plan() {
  log "Dry-run mode enabled. No changes will be made."
  log "Would download: ${DOWNLOAD_URL}"
  log "Would download: https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS"
  log "Would verify SHA256 for ${ARCHIVE}"

  if [[ "${#ACTIVE_USERS[@]}" -gt 0 ]]; then
    log "Would stop active runner daemon user(s): ${ACTIVE_USERS[*]}"
  fi

  local has_state=false
  local binary
  for binary in "${binaries[@]}"; do
    if [[ -f "${INSTALL_DIR}/${binary}" ]]; then
      has_state=true
      break
    fi
  done
  if [[ -d "$CONFIG_ROOT" ]]; then
    has_state=true
  fi
  if [[ "$has_state" == "true" ]]; then
    local version_label timestamp planned_backup_path
    version_label="$CURRENT_VERSION"
    if [[ -z "$version_label" || "$version_label" == "unknown" ]]; then
      version_label="unknown"
    fi
    timestamp="$(date +%Y%m%d-%H%M%S)"
    planned_backup_path="${BACKUP_ROOT}/${version_label}-${timestamp}"
    log "Would back up existing binaries/config to: ${planned_backup_path}"
  fi

  log "Would install binaries to ${INSTALL_DIR}"
  if [[ "$SKIP_CONFIG" == "true" ]]; then
    log "Would skip config updates (--skip-config)"
  else
    if [[ -f "$RUNNER_CONFIG" && "$OVERWRITE_CONFIG" != "true" ]]; then
      log "Would update [guest_images] tags in existing ${RUNNER_CONFIG} to ${TAG}"
      log "Would save new templates as ${CONFIG_ROOT}/*.${TAG}.new"
    else
      log "Would initialize config templates under ${CONFIG_ROOT}"
    fi
  fi

  if [[ "$NO_PULL" == "true" ]]; then
    log "Would skip Docker pre-pull (--no-pull)"
  else
    log "Would pre-pull guest Docker images (best-effort)"
  fi

  if [[ "${#STOPPED_USERS[@]}" -gt 0 ]]; then
    log "Would offer daemon restart for user(s): ${STOPPED_USERS[*]}"
  fi

  log "Dry-run complete."
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      TAG="${2:?Missing value for --tag}"
      shift 2
      ;;
    --repo)
      REPO="${2:?Missing value for --repo}"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="${2:?Missing value for --install-dir}"
      shift 2
      ;;
    --system)
      SYSTEM_INSTALL=true
      shift
      ;;
    --base-dir)
      BASE_DIR="${2:?Missing value for --base-dir}"
      shift 2
      ;;
    --skip-config)
      SKIP_CONFIG=true
      shift
      ;;
    --overwrite-config)
      OVERWRITE_CONFIG=true
      shift
      ;;
    --force)
      FORCE=true
      shift
      ;;
    --yes|-y)
      AUTO_YES=true
      shift
      ;;
    --no-pull)
      NO_PULL=true
      shift
      ;;
    --backup-dir)
      BACKUP_ROOT_OVERRIDE="${2:?Missing value for --backup-dir}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ "$SYSTEM_INSTALL" == "true" ]]; then
  INSTALL_DIR="/usr/local/bin"
fi

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"
command -v awk >/dev/null 2>&1 || fail "awk is required"
command -v sed >/dev/null 2>&1 || fail "sed is required"

[[ -n "$TAG" ]] || TAG="$(resolve_latest_tag)"
PLATFORM="$(detect_platform)"

if ! mkdir -p "$BASE_DIR"; then
  fail "failed to create base directory: ${BASE_DIR}"
fi
BASE_DIR="$(cd "$BASE_DIR" && pwd)"

CONFIG_ROOT="${BASE_DIR}/.oxydra"
RUNNER_CONFIG="${CONFIG_ROOT}/runner.toml"
CURRENT_RUNNER_BIN="${INSTALL_DIR}/runner"
RUNNER_BIN="${INSTALL_DIR}/runner"

TARGET_VERSION_NORMALIZED="$(normalize_version "$TAG" || true)"
if [[ -x "$CURRENT_RUNNER_BIN" ]]; then
  local_version_line="$("$CURRENT_RUNNER_BIN" --version 2>/dev/null | head -n 1 || true)"
  CURRENT_VERSION_NORMALIZED="$(normalize_version "$local_version_line" || true)"
  if [[ -n "$CURRENT_VERSION_NORMALIZED" ]]; then
    CURRENT_VERSION="v${CURRENT_VERSION_NORMALIZED}"
  fi
fi

if [[ -z "$BACKUP_ROOT_OVERRIDE" ]]; then
  BACKUP_ROOT="${HOME}/.local/share/oxydra/backups"
else
  BACKUP_ROOT="$BACKUP_ROOT_OVERRIDE"
fi

if [[ -n "$CURRENT_VERSION_NORMALIZED" && -n "$TARGET_VERSION_NORMALIZED" ]]; then
  if [[ "$CURRENT_VERSION_NORMALIZED" == "$TARGET_VERSION_NORMALIZED" && "$FORCE" != "true" ]]; then
    log "Already at v${CURRENT_VERSION_NORMALIZED}. Use --force to reinstall."
    exit 0
  fi

  if [[ "$CURRENT_VERSION_NORMALIZED" == "$TARGET_VERSION_NORMALIZED" ]]; then
    log "Reinstalling v${TARGET_VERSION_NORMALIZED} (--force)."
  else
    cmp="$(compare_versions "$TARGET_VERSION_NORMALIZED" "$CURRENT_VERSION_NORMALIZED")"
    if [[ "$cmp" == "-1" ]]; then
      warn "downgrading from v${CURRENT_VERSION_NORMALIZED} -> v${TARGET_VERSION_NORMALIZED}"
    else
      log "Upgrading from v${CURRENT_VERSION_NORMALIZED} -> v${TARGET_VERSION_NORMALIZED}"
    fi
  fi
else
  log "Installing Oxydra ${TAG} (${PLATFORM})"
fi

ARCHIVE="oxydra-${TAG}-${PLATFORM}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE}"

if [[ -f "$RUNNER_CONFIG" ]]; then
  collect_active_runner_users
  stop_active_runner_daemons
fi

if [[ "$DRY_RUN" == "true" ]]; then
  print_dry_run_plan
  exit 0
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

log "Downloading ${DOWNLOAD_URL}"
if ! curl -fL --retry 3 -o "${TMP_DIR}/${ARCHIVE}" "$DOWNLOAD_URL"; then
  fail "download failed. Verify the tag/repository and that release assets are published for ${PLATFORM}."
fi

SHA_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS"
log "Downloading ${SHA_URL}"
if ! curl -fL --retry 3 -o "${TMP_DIR}/SHA256SUMS" "$SHA_URL"; then
  fail "failed to download SHA256SUMS for ${TAG}"
fi

expected_sha="$(awk -v name="${ARCHIVE}" '{ file = $NF; sub(/^\*/, "", file); if (file == name) { print $1; exit } }' "${TMP_DIR}/SHA256SUMS")"
[[ -n "$expected_sha" ]] || fail "could not find checksum entry for ${ARCHIVE} in SHA256SUMS"

actual_sha="$(sha256_file "${TMP_DIR}/${ARCHIVE}")"
if [[ "$expected_sha" != "$actual_sha" ]]; then
  fail "checksum verification failed for ${ARCHIVE}"
fi
log "Checksum verified: ${ARCHIVE}"

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"

for binary in "${binaries[@]}"; do
  [[ -f "${TMP_DIR}/${binary}" ]] || fail "archive missing binary: ${binary}"
done

create_backup

if [[ "$SYSTEM_INSTALL" == "true" ]]; then
  if [[ "$(id -u)" -eq 0 ]]; then
    mkdir -p "$INSTALL_DIR"
    for binary in "${binaries[@]}"; do
      install_binary_local "${TMP_DIR}/${binary}" "${INSTALL_DIR}/${binary}"
    done
  else
    command -v sudo >/dev/null 2>&1 || fail "--system requires sudo (or run as root)"
    sudo mkdir -p "$INSTALL_DIR"
    for binary in "${binaries[@]}"; do
      install_binary_system "${TMP_DIR}/${binary}" "${INSTALL_DIR}/${binary}"
    done
  fi
else
  mkdir -p "$INSTALL_DIR"
  for binary in "${binaries[@]}"; do
    install_binary_local "${TMP_DIR}/${binary}" "${INSTALL_DIR}/${binary}"
  done
fi

log "Installed binaries to ${INSTALL_DIR}:"
for binary in "${binaries[@]}"; do
  log "  - ${binary}"
done

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  cat <<EOF
[oxydra-install] ${INSTALL_DIR} is not in PATH.
[oxydra-install] PATH was not modified automatically.
[oxydra-install] If you used curl|bash, environment changes cannot persist to the parent shell.
[oxydra-install] Add it with:
  export PATH="${INSTALL_DIR}:\$PATH"
EOF
fi

if [[ "$SKIP_CONFIG" != "true" ]]; then
  initialize_config_templates "$BASE_DIR"
else
  log "Skipping config initialization and update (--skip-config)"
fi

pre_pull_images
restart_stopped_daemons

if [[ -n "$TARGET_VERSION_NORMALIZED" && -n "$CURRENT_VERSION_NORMALIZED" ]]; then
  log "Done. Upgraded v${CURRENT_VERSION_NORMALIZED} -> v${TARGET_VERSION_NORMALIZED}"
else
  log "Done. Installed tag: ${TAG}"
fi
log "Binaries: ${INSTALL_DIR}/{runner,oxydra-vm,shell-daemon,oxydra-tui}"
if [[ "$SKIP_CONFIG" == "true" ]]; then
  log "Config: skipped (--skip-config)"
else
  log "Config: ${CONFIG_ROOT}/"
fi
if [[ -n "$BACKUP_PATH" ]]; then
  log "Backup: ${BACKUP_PATH}"
fi
log "Workspace base directory: ${BASE_DIR}"
log "Start the runner with:"
log "  \"${RUNNER_BIN}\" --config \"${RUNNER_CONFIG}\" --user alice start"
log "Connect TUI with:"
log "  \"${RUNNER_BIN}\" --tui --config \"${RUNNER_CONFIG}\" --user alice"
