#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
INSTALL_SCRIPT="${ROOT_DIR}/scripts/install-release.sh"
BUILD_SCRIPT_REL="scripts/build-release-assets.sh"

MODE="fresh"
SOURCE="tag"
TAG=""
COMMIT=""
REPO="shantanugoel/oxydra"

FRESH_ROOT_BASE="${OXYDRA_FRESH_ROOT:-/tmp/oxydra-fresh-tests}"
LABEL=""
START_WEB=false
WEB_BIND="127.0.0.1:9400"

NO_PULL=false
AUTO_YES=true
UPGRADE_INSTALL_DIR=""
UPGRADE_BASE_DIR=""

ENV_SOURCE_PATH="${SCRIPT_DIR}/.env"
ENV_SOURCE_EXPLICIT=false
ENV_OVERRIDES=()

PUSH_IMAGES=false
IMAGE_NAMESPACE="${IMAGE_NAMESPACE:-}"
SKIP_DOCKER_IMAGES=false
SSH_IMAGE_LOAD=true
WORKTREE_ROOT="${OXYDRA_WORKTREE_ROOT:-/tmp/oxydra-build-worktrees}"
KEEP_WORKTREE=false

TARGETS=()
TARGET_KIND_LIST=()
TARGET_HOST_LIST=()
TARGET_NAME_LIST=()
TARGET_PLATFORM_LIST=()
TARGET_HOME_LIST=()

BINARIES=(runner oxydra-vm shell-daemon oxydra-tui)

SOURCE_CHECKOUT=""
WORKTREE_DIR=""
BUILD_LABEL=""

usage() {
  cat <<'USAGE_EOF'
Run repeatable Oxydra build/install tests on local and SSH targets.

Usage:
  ./scripts/test-build-install.sh [options]

Core options:
  --mode <fresh|fresh-clean|upgrade>
                          fresh: isolated install under /tmp (default)
                          fresh-clean: remove isolated install by label
                          upgrade: replace existing install/config on each target
  --source <tag|local|commit>
                          tag: use GitHub Releases via install-release.sh
                          local: build current checkout and install that build
                          commit: create temp worktree for --commit and build that
                          (default: tag)
  --tag <tag>            Release tag (used by --source tag)
  --commit <rev>         Commit/branch/tag (required by --source commit)
  --target <local|ssh:user@host|user@host>
                          Target host; repeatable. Default: local

Install behavior:
  --repo <owner/name>    GitHub repo for --source tag (default: shantanugoel/oxydra)
  --label <name>         Fresh install label (required for fresh-clean)
  --fresh-root <path>    Base dir for fresh installs (default: /tmp/oxydra-fresh-tests)
  --install-dir <path>   Override install dir for upgrade mode
  --base-dir <path>      Override base dir for upgrade mode
  --start-web            Start web configurator after fresh setup
  --web-bind <addr>      Bind address for --start-web (default: 127.0.0.1:9400)
  --no-pull              Pass --no-pull to install-release.sh (tag source only)
  --interactive          Do not auto-pass --yes (tag source only)

Env handling:
  --env-file <path>      Local env file (default: scripts/.env if present)
  --no-env-file          Disable env file loading

Docker image behavior for local/commit source:
  --push-images          Push built images to GHCR (instead of SSH docker load)
  --image-namespace <n>  GHCR namespace for --push-images (default: repo owner)
  --skip-docker-images   Do not build/update guest image refs
  --no-ssh-image-load    Don't docker-load local images over SSH (requires --push-images)

Worktree behavior:
  --keep-worktree        Keep temporary worktree for --source commit

  -h, --help             Show help

Examples:
  # Release tag flow
  ./scripts/test-build-install.sh --mode fresh --source tag --tag v0.2.3 \
    --target local --target ssh:pi@raspberrypi.local

  # Current checkout flow
  ./scripts/test-build-install.sh --mode fresh --source local \
    --target local --target ssh:pi@raspberrypi.local

  # Commit flow
  ./scripts/test-build-install.sh --mode upgrade --source commit --commit a1b2c3d \
    --target local --target ssh:pi@raspberrypi.local
USAGE_EOF
}

log() {
  printf '[oxydra-build-test] %s\n' "$*"
}

warn() {
  printf '[oxydra-build-test] Warning: %s\n' "$*" >&2
}

fail() {
  printf '[oxydra-build-test] Error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "$WORKTREE_DIR" && -d "$WORKTREE_DIR" && "$KEEP_WORKTREE" != "true" ]]; then
    git -C "$ROOT_DIR" worktree remove --force "$WORKTREE_DIR" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

sanitize_label() {
  printf '%s' "$1" | tr -c '[:alnum:]._-' '-'
}

trim_space() {
  printf '%s' "$1" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

contains_item() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    [[ "$item" == "$needle" ]] && return 0
  done
  return 1
}

append_unique() {
  local value="$1"
  shift
  if contains_item "$value" "$@"; then
    return 1
  fi
  return 0
}

upsert_env_override() {
  local entry="$1"
  local key="${entry%%=*}"
  local i existing existing_key
  for i in "${!ENV_OVERRIDES[@]}"; do
    existing="${ENV_OVERRIDES[$i]}"
    existing_key="${existing%%=*}"
    if [[ "$existing_key" == "$key" ]]; then
      ENV_OVERRIDES[$i]="$entry"
      return
    fi
  done
  ENV_OVERRIDES+=("$entry")
}

load_env_overrides() {
  local source_path="$1"
  local raw line key value
  local line_no=0

  while IFS= read -r raw || [[ -n "$raw" ]]; do
    line_no=$((line_no + 1))
    line="$(trim_space "$raw")"
    [[ -z "$line" || "${line:0:1}" == "#" ]] && continue

    if [[ "$line" == export[[:space:]]* ]]; then
      line="$(trim_space "${line#export}")"
    fi

    if [[ "$line" != *=* ]]; then
      fail "invalid env entry in ${source_path}:${line_no} (expected KEY=VALUE)"
    fi

    key="${line%%=*}"
    value="${line#*=}"
    key="$(trim_space "$key")"
    if [[ -z "$key" || ! "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
      fail "invalid env key in ${source_path}:${line_no}: ${key}"
    fi

    upsert_env_override "${key}=${value}"
  done < "$source_path"
}

write_env_overrides_file() {
  local destination="$1"
  local entry
  mkdir -p "$(dirname "$destination")"
  : > "$destination"
  for entry in "${ENV_OVERRIDES[@]}"; do
    printf '%s\n' "$entry" >> "$destination"
  done
}

quote_args() {
  local out="" arg
  for arg in "$@"; do
    out="${out} $(printf '%q' "$arg")"
  done
  printf '%s' "${out# }"
}

join_csv() {
  local out="" item
  for item in "$@"; do
    if [[ -z "$out" ]]; then
      out="$item"
    else
      out="${out},${item}"
    fi
  done
  printf '%s' "$out"
}

run_remote_command() {
  local host="$1"
  shift
  ssh "$host" "$(quote_args "$@")"
}

copy_file_to_remote() {
  local host="$1"
  local source_file="$2"
  local destination="$3"
  local mode="$4"
  run_remote_command "$host" mkdir -p "$(dirname "$destination")"
  ssh "$host" "cat > $(printf '%q' "$destination")" < "$source_file"
  run_remote_command "$host" chmod "$mode" "$destination"
}

run_remote_installer() {
  local host="$1"
  shift

  local remote_installer="/tmp/oxydra-install-release-${USER:-user}-$$.sh"
  local command status

  ssh "$host" "cat > $(printf '%q' "$remote_installer") && chmod +x $(printf '%q' "$remote_installer")" < "$INSTALL_SCRIPT"

  command="$(quote_args "$remote_installer" "$@")"
  set +e
  ssh "$host" "$command"
  status=$?
  set -e

  ssh "$host" "$(quote_args rm -f "$remote_installer")" >/dev/null 2>&1 || true
  return "$status"
}

write_runner_generic_wrapper_script() {
  local destination="$1"
  local runner_bin="$2"
  local runner_config="$3"
  local env_file="$4"

  cat >"$destination" <<WRAPPER_EOF
#!/usr/bin/env bash
set -euo pipefail

ENV_FILE=$(printf '%q' "$env_file")
RUNNER_BIN=$(printf '%q' "$runner_bin")
RUNNER_CONFIG=$(printf '%q' "$runner_config")

if [[ -f "\$ENV_FILE" ]]; then
  while IFS= read -r line || [[ -n "\$line" ]]; do
    line="\$(printf '%s' "\$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    [[ -z "\$line" || "\${line:0:1}" == "#" ]] && continue
    export "\$line"
  done < "\$ENV_FILE"
  exec "\$RUNNER_BIN" --config "\$RUNNER_CONFIG" --env-file "\$ENV_FILE" "\$@"
fi

exec "\$RUNNER_BIN" --config "\$RUNNER_CONFIG" "\$@"
WRAPPER_EOF

  chmod 0755 "$destination"
}

parse_target_spec() {
  local target="$1"

  if [[ "$target" == "local" ]]; then
    PARSED_TARGET_KIND="local"
    PARSED_TARGET_HOST=""
    PARSED_TARGET_NAME="local"
    return
  fi

  PARSED_TARGET_KIND="ssh"
  if [[ "$target" == ssh:* ]]; then
    PARSED_TARGET_HOST="${target#ssh:}"
  else
    PARSED_TARGET_HOST="$target"
  fi
  [[ -n "$PARSED_TARGET_HOST" ]] || fail "invalid target: ${target}"
  PARSED_TARGET_NAME="$PARSED_TARGET_HOST"
}

platform_from_os_arch() {
  local os="$1"
  local arch="$2"

  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) printf '%s' "macos-arm64" ;;
        x86_64|amd64) fail "macOS x86_64 release artifacts are not supported" ;;
        *) fail "unsupported macOS architecture: ${arch}" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64) printf '%s' "linux-amd64" ;;
        aarch64|arm64) printf '%s' "linux-arm64" ;;
        *) fail "unsupported Linux architecture: ${arch}" ;;
      esac
      ;;
    *)
      fail "unsupported OS: ${os}"
      ;;
  esac
}

detect_target_platform() {
  local kind="$1"
  local host="$2"
  local os arch

  if [[ "$kind" == "local" ]]; then
    os="$(uname -s)"
    arch="$(uname -m)"
  else
    os="$(ssh "$host" "uname -s")"
    arch="$(ssh "$host" "uname -m")"
  fi

  platform_from_os_arch "$os" "$arch"
}

detect_target_home() {
  local kind="$1"
  local host="$2"

  if [[ "$kind" == "local" ]]; then
    printf '%s' "$HOME"
  else
    ssh "$host" "printf '%s' \"\$HOME\""
  fi
}

remote_file_exists() {
  local host="$1"
  local path="$2"
  ssh "$host" "test -f $(printf '%q' "$path")"
}

update_runner_config_images() {
  local config_file="$1"
  local oxydra_ref="$2"
  local shell_ref="$3"

  [[ -f "$config_file" ]] || return
  if [[ -z "$oxydra_ref" && -z "$shell_ref" ]]; then
    return
  fi

  local patched="${config_file}.patched"

  awk -v ox="$oxydra_ref" -v sh="$shell_ref" '
    function replace_quoted(line, value) {
      if (line ~ /"[^"]*"/) {
        sub(/"[^"]*"/, "\"" value "\"", line)
        return line
      }
      return line
    }

    BEGIN {
      in_guest = 0
      saw_guest = 0
      saw_ox = 0
      saw_sh = 0
    }

    {
      if ($0 ~ /^[[:space:]]*\[guest_images\][[:space:]]*$/) {
        in_guest = 1
        saw_guest = 1
        print
        next
      }

      if ($0 ~ /^[[:space:]]*\[[^]]+\][[:space:]]*$/ && in_guest) {
        if (ox != "" && !saw_ox) print "oxydra_vm = \"" ox "\""
        if (sh != "" && !saw_sh) print "shell_vm  = \"" sh "\""
        in_guest = 0
        print
        next
      }

      if (in_guest && ox != "" && $0 ~ /^[[:space:]]*oxydra_vm[[:space:]]*=/) {
        print replace_quoted($0, ox)
        saw_ox = 1
        next
      }

      if (in_guest && sh != "" && $0 ~ /^[[:space:]]*shell_vm[[:space:]]*=/) {
        print replace_quoted($0, sh)
        saw_sh = 1
        next
      }

      print
    }

    END {
      if (in_guest) {
        if (ox != "" && !saw_ox) print "oxydra_vm = \"" ox "\""
        if (sh != "" && !saw_sh) print "shell_vm  = \"" sh "\""
      }
      if (!saw_guest) {
        print ""
        print "[guest_images]"
        if (ox != "") print "oxydra_vm = \"" ox "\""
        if (sh != "") print "shell_vm  = \"" sh "\""
      }
    }
  ' "$config_file" > "$patched"

  mv "$patched" "$config_file"
}

create_config_tree() {
  local output_root="$1"
  local checkout_dir="$2"
  local oxydra_ref="$3"
  local shell_ref="$4"

  local template_dir="${checkout_dir}/examples/config"
  [[ -f "${template_dir}/agent.toml" ]] || fail "missing template: ${template_dir}/agent.toml"
  [[ -f "${template_dir}/runner.toml" ]] || fail "missing template: ${template_dir}/runner.toml"
  [[ -f "${template_dir}/runner-user.toml" ]] || fail "missing template: ${template_dir}/runner-user.toml"

  mkdir -p "${output_root}/users"
  cp "${template_dir}/agent.toml" "${output_root}/agent.toml"
  cp "${template_dir}/runner.toml" "${output_root}/runner.toml"
  cp "${template_dir}/runner-user.toml" "${output_root}/users/alice.toml"

  update_runner_config_images "${output_root}/runner.toml" "$oxydra_ref" "$shell_ref"
}

ensure_config_local() {
  local mode="$1"
  local base_dir="$2"
  local checkout_dir="$3"
  local oxydra_ref="$4"
  local shell_ref="$5"

  local config_root="${base_dir}/.oxydra"
  local runner_cfg="${config_root}/runner.toml"
  local tmp_root

  if [[ "$mode" == "fresh" ]]; then
    tmp_root="$(mktemp -d)"
    create_config_tree "${tmp_root}/.oxydra" "$checkout_dir" "$oxydra_ref" "$shell_ref"
    mkdir -p "$base_dir"
    rm -rf "$config_root"
    cp -R "${tmp_root}/.oxydra" "$config_root"
    rm -rf "$tmp_root"
    return
  fi

  mkdir -p "${config_root}/users"
  if [[ -f "$runner_cfg" ]]; then
    update_runner_config_images "$runner_cfg" "$oxydra_ref" "$shell_ref"
    if [[ ! -f "${config_root}/agent.toml" ]]; then
      cp "${checkout_dir}/examples/config/agent.toml" "${config_root}/agent.toml"
    fi
    if [[ ! -f "${config_root}/users/alice.toml" ]]; then
      cp "${checkout_dir}/examples/config/runner-user.toml" "${config_root}/users/alice.toml"
    fi
    return
  fi

  tmp_root="$(mktemp -d)"
  create_config_tree "${tmp_root}/.oxydra" "$checkout_dir" "$oxydra_ref" "$shell_ref"
  cp -R "${tmp_root}/.oxydra" "$config_root"
  rm -rf "$tmp_root"
}

ensure_config_remote() {
  local mode="$1"
  local host="$2"
  local base_dir="$3"
  local checkout_dir="$4"
  local oxydra_ref="$5"
  local shell_ref="$6"

  local config_root="${base_dir}/.oxydra"
  local runner_remote="${config_root}/runner.toml"
  local agent_remote="${config_root}/agent.toml"
  local user_remote="${config_root}/users/alice.toml"
  local tmp_root tmp_runner

  run_remote_command "$host" mkdir -p "${config_root}/users"

  if [[ "$mode" == "fresh" ]]; then
    tmp_root="$(mktemp -d)"
    create_config_tree "${tmp_root}/.oxydra" "$checkout_dir" "$oxydra_ref" "$shell_ref"
    run_remote_command "$host" rm -rf "$config_root"
    run_remote_command "$host" mkdir -p "${config_root}/users"
    copy_file_to_remote "$host" "${tmp_root}/.oxydra/agent.toml" "$agent_remote" 0644
    copy_file_to_remote "$host" "${tmp_root}/.oxydra/runner.toml" "$runner_remote" 0644
    copy_file_to_remote "$host" "${tmp_root}/.oxydra/users/alice.toml" "$user_remote" 0644
    rm -rf "$tmp_root"
    return
  fi

  if remote_file_exists "$host" "$runner_remote"; then
    tmp_runner="$(mktemp)"
    ssh "$host" "cat $(printf '%q' "$runner_remote")" > "$tmp_runner"
    update_runner_config_images "$tmp_runner" "$oxydra_ref" "$shell_ref"
    copy_file_to_remote "$host" "$tmp_runner" "$runner_remote" 0644
    rm -f "$tmp_runner"

    if ! remote_file_exists "$host" "$agent_remote"; then
      copy_file_to_remote "$host" "${checkout_dir}/examples/config/agent.toml" "$agent_remote" 0644
    fi
    if ! remote_file_exists "$host" "$user_remote"; then
      copy_file_to_remote "$host" "${checkout_dir}/examples/config/runner-user.toml" "$user_remote" 0644
    fi
    return
  fi

  tmp_root="$(mktemp -d)"
  create_config_tree "${tmp_root}/.oxydra" "$checkout_dir" "$oxydra_ref" "$shell_ref"
  copy_file_to_remote "$host" "${tmp_root}/.oxydra/agent.toml" "$agent_remote" 0644
  copy_file_to_remote "$host" "${tmp_root}/.oxydra/runner.toml" "$runner_remote" 0644
  copy_file_to_remote "$host" "${tmp_root}/.oxydra/users/alice.toml" "$user_remote" 0644
  rm -rf "$tmp_root"
}

install_binaries_local() {
  local archive="$1"
  local install_dir="$2"
  local tmp_dir binary

  tmp_dir="$(mktemp -d)"
  tar -xzf "$archive" -C "$tmp_dir"
  mkdir -p "$install_dir"

  for binary in "${BINARIES[@]}"; do
    [[ -f "${tmp_dir}/${binary}" ]] || fail "archive missing binary: ${binary}"
    if command -v install >/dev/null 2>&1; then
      install -m 0755 "${tmp_dir}/${binary}" "${install_dir}/${binary}"
    else
      cp "${tmp_dir}/${binary}" "${install_dir}/${binary}"
      chmod 0755 "${install_dir}/${binary}"
    fi
  done

  rm -rf "$tmp_dir"
}

install_binaries_remote() {
  local host="$1"
  local archive="$2"
  local install_dir="$3"
  local remote_archive="/tmp/oxydra-build-${BUILD_LABEL:-manual}-$$.tar.gz"

  copy_file_to_remote "$host" "$archive" "$remote_archive" 0644

  ssh "$host" "bash -s -- $(printf '%q' "$remote_archive") $(printf '%q' "$install_dir")" <<'REMOTE_INSTALL_EOF'
set -euo pipefail
archive="$1"
install_dir="$2"
tmp_dir="$(mktemp -d)"

tar -xzf "$archive" -C "$tmp_dir"
mkdir -p "$install_dir"

for binary in runner oxydra-vm shell-daemon oxydra-tui; do
  [[ -f "${tmp_dir}/${binary}" ]] || { echo "missing binary: ${binary}" >&2; exit 1; }
  if command -v install >/dev/null 2>&1; then
    install -m 0755 "${tmp_dir}/${binary}" "${install_dir}/${binary}"
  else
    cp "${tmp_dir}/${binary}" "${install_dir}/${binary}"
    chmod 0755 "${install_dir}/${binary}"
  fi
done

rm -rf "$tmp_dir" "$archive"
REMOTE_INSTALL_EOF
}

resolve_image_refs_for_platform() {
  local platform="$1"

  RESOLVED_OXYDRA_IMAGE=""
  RESOLVED_SHELL_IMAGE=""

  if [[ "$SKIP_DOCKER_IMAGES" == "true" ]]; then
    return
  fi

  if [[ "$PUSH_IMAGES" == "true" ]]; then
    RESOLVED_OXYDRA_IMAGE="ghcr.io/${IMAGE_NAMESPACE}/oxydra-vm:${BUILD_LABEL}"
    RESOLVED_SHELL_IMAGE="ghcr.io/${IMAGE_NAMESPACE}/shell-vm:${BUILD_LABEL}"
    return
  fi

  case "$platform" in
    linux-amd64)
      RESOLVED_OXYDRA_IMAGE="oxydra-vm:${BUILD_LABEL}-linux-amd64"
      RESOLVED_SHELL_IMAGE="shell-vm:${BUILD_LABEL}-linux-amd64"
      ;;
    macos-arm64|linux-arm64)
      RESOLVED_OXYDRA_IMAGE="oxydra-vm:${BUILD_LABEL}-linux-arm64"
      RESOLVED_SHELL_IMAGE="shell-vm:${BUILD_LABEL}-linux-arm64"
      ;;
    *)
      fail "unsupported platform for image refs: ${platform}"
      ;;
  esac
}

maybe_load_images_to_remote() {
  local host="$1"
  local platform="$2"

  if [[ "$SKIP_DOCKER_IMAGES" == "true" || "$PUSH_IMAGES" == "true" ]]; then
    return
  fi

  if [[ "$SSH_IMAGE_LOAD" != "true" ]]; then
    fail "remote target ${host} requires images; use --push-images or remove --no-ssh-image-load"
  fi

  resolve_image_refs_for_platform "$platform"
  [[ -n "$RESOLVED_OXYDRA_IMAGE" ]] || return

  docker image inspect "$RESOLVED_OXYDRA_IMAGE" >/dev/null 2>&1 || fail "missing local image: ${RESOLVED_OXYDRA_IMAGE}"
  docker image inspect "$RESOLVED_SHELL_IMAGE" >/dev/null 2>&1 || fail "missing local image: ${RESOLVED_SHELL_IMAGE}"

  log "Loading images onto ${host}: ${RESOLVED_OXYDRA_IMAGE}, ${RESOLVED_SHELL_IMAGE}"
  docker save "$RESOLVED_OXYDRA_IMAGE" "$RESOLVED_SHELL_IMAGE" | ssh "$host" docker load >/dev/null
}

prepare_source_checkout() {
  case "$SOURCE" in
    tag)
      [[ -z "$COMMIT" ]] || fail "--commit is only valid with --source commit"
      ;;
    local)
      [[ -z "$TAG" ]] || warn "--tag is ignored for --source local"
      [[ -z "$COMMIT" ]] || fail "--commit is only valid with --source commit"
      SOURCE_CHECKOUT="$ROOT_DIR"
      BUILD_LABEL="local"
      ;;
    commit)
      [[ -n "$COMMIT" ]] || fail "--source commit requires --commit <rev>"
      [[ -z "$TAG" ]] || warn "--tag is ignored for --source commit"

      local full short
      full="$(git -C "$ROOT_DIR" rev-parse "$COMMIT" 2>/dev/null || true)"
      [[ -n "$full" ]] || fail "could not resolve commit: ${COMMIT}"
      short="$(git -C "$ROOT_DIR" rev-parse --short=12 "$full")"

      BUILD_LABEL="$(sanitize_label "$short")"
      mkdir -p "$WORKTREE_ROOT"
      WORKTREE_DIR="$(mktemp -d "${WORKTREE_ROOT}/oxydra-${short}-XXXXXX")"
      rm -rf "$WORKTREE_DIR"
      git -C "$ROOT_DIR" worktree add --detach "$WORKTREE_DIR" "$full" >/dev/null
      SOURCE_CHECKOUT="$WORKTREE_DIR"
      log "Using worktree: ${WORKTREE_DIR} (${full})"
      ;;
    *)
      fail "--source must be one of: tag, local, commit"
      ;;
  esac
}

build_local_or_commit_assets() {
  local required_binary_platforms=("$@")
  local build_platforms=()
  local required_image_platforms=()
  local platform

  for platform in "${required_binary_platforms[@]-}"; do
    if append_unique "$platform" "${build_platforms[@]-}"; then
      build_platforms+=("$platform")
    fi
  done

  if [[ "$SKIP_DOCKER_IMAGES" != "true" ]]; then
    for platform in "${required_binary_platforms[@]-}"; do
      case "$platform" in
        linux-amd64|linux-arm64)
          if append_unique "$platform" "${required_image_platforms[@]-}"; then
            required_image_platforms+=("$platform")
          fi
          ;;
        macos-arm64)
          if append_unique "linux-arm64" "${required_image_platforms[@]-}"; then
            required_image_platforms+=("linux-arm64")
          fi
          ;;
      esac
    done

    for platform in "${required_image_platforms[@]-}"; do
      if append_unique "$platform" "${build_platforms[@]-}"; then
        build_platforms+=("$platform")
      fi
    done
  fi

  if [[ "$PUSH_IMAGES" == "true" && "$SKIP_DOCKER_IMAGES" == "true" ]]; then
    fail "--push-images cannot be combined with --skip-docker-images"
  fi

  if [[ "$PUSH_IMAGES" == "true" && -z "$IMAGE_NAMESPACE" ]]; then
    IMAGE_NAMESPACE="${REPO%%/*}"
    [[ -n "$IMAGE_NAMESPACE" ]] || fail "could not infer image namespace; pass --image-namespace"
  fi

  local platform_csv
  platform_csv="$(join_csv "${build_platforms[@]-}")"
  [[ -n "$platform_csv" ]] || fail "no platforms selected for build"

  local build_cmd=("${SOURCE_CHECKOUT}/${BUILD_SCRIPT_REL}" --tag "$BUILD_LABEL" --platforms "$platform_csv")
  if [[ "$SKIP_DOCKER_IMAGES" == "true" ]]; then
    build_cmd+=(--no-docker)
  elif [[ "$PUSH_IMAGES" == "true" ]]; then
    build_cmd+=(--push-docker --registry ghcr --image-namespace "$IMAGE_NAMESPACE")
  fi

  log "Building source=${SOURCE} label=${BUILD_LABEL} profile=debug platforms=${platform_csv}"
  (cd "$SOURCE_CHECKOUT" && OXYDRA_BUILD_PROFILE=debug "${build_cmd[@]}")

  for platform in "${required_binary_platforms[@]-}"; do
    [[ -f "${SOURCE_CHECKOUT}/dist/oxydra-${BUILD_LABEL}-${platform}.tar.gz" ]] || fail "missing built artifact for ${platform}"
  done
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="${2:?Missing value for --mode}"
      shift 2
      ;;
    --source)
      SOURCE="${2:?Missing value for --source}"
      shift 2
      ;;
    --tag)
      TAG="${2:?Missing value for --tag}"
      shift 2
      ;;
    --commit)
      COMMIT="${2:?Missing value for --commit}"
      shift 2
      ;;
    --target)
      TARGETS+=("${2:?Missing value for --target}")
      shift 2
      ;;
    --repo)
      REPO="${2:?Missing value for --repo}"
      shift 2
      ;;
    --label)
      LABEL="${2:?Missing value for --label}"
      shift 2
      ;;
    --fresh-root)
      FRESH_ROOT_BASE="${2:?Missing value for --fresh-root}"
      shift 2
      ;;
    --start-web)
      START_WEB=true
      shift
      ;;
    --web-bind)
      WEB_BIND="${2:?Missing value for --web-bind}"
      shift 2
      ;;
    --no-pull)
      NO_PULL=true
      shift
      ;;
    --interactive)
      AUTO_YES=false
      shift
      ;;
    --env-file)
      ENV_SOURCE_PATH="${2:?Missing value for --env-file}"
      ENV_SOURCE_EXPLICIT=true
      shift 2
      ;;
    --no-env-file)
      ENV_SOURCE_PATH=""
      ENV_SOURCE_EXPLICIT=true
      shift
      ;;
    --install-dir)
      UPGRADE_INSTALL_DIR="${2:?Missing value for --install-dir}"
      shift 2
      ;;
    --base-dir)
      UPGRADE_BASE_DIR="${2:?Missing value for --base-dir}"
      shift 2
      ;;
    --push-images)
      PUSH_IMAGES=true
      shift
      ;;
    --image-namespace)
      IMAGE_NAMESPACE="${2:?Missing value for --image-namespace}"
      shift 2
      ;;
    --skip-docker-images)
      SKIP_DOCKER_IMAGES=true
      shift
      ;;
    --no-ssh-image-load)
      SSH_IMAGE_LOAD=false
      shift
      ;;
    --keep-worktree)
      KEEP_WORKTREE=true
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

case "$MODE" in
  fresh|fresh-clean|upgrade) ;;
  *) fail "--mode must be one of: fresh, fresh-clean, upgrade" ;;
esac

case "$SOURCE" in
  tag|local|commit) ;;
  *) fail "--source must be one of: tag, local, commit" ;;
esac

if [[ "$START_WEB" == "true" && "$MODE" != "fresh" ]]; then
  fail "--start-web is only valid with --mode fresh"
fi

if [[ "${#TARGETS[@]}" -eq 0 ]]; then
  TARGETS=("local")
fi

if [[ -n "$ENV_SOURCE_PATH" ]]; then
  if [[ -f "$ENV_SOURCE_PATH" ]]; then
    load_env_overrides "$ENV_SOURCE_PATH"
    log "Loaded ${#ENV_OVERRIDES[@]} env override(s) from ${ENV_SOURCE_PATH}"
  elif [[ "$ENV_SOURCE_EXPLICIT" == "true" ]]; then
    fail "env file not found: ${ENV_SOURCE_PATH}"
  fi
fi

if [[ "$MODE" == "fresh-clean" ]]; then
  [[ -n "$LABEL" ]] || fail "--label is required for --mode fresh-clean"
  LABEL="$(sanitize_label "$LABEL")"

  for target in "${TARGETS[@]}"; do
    parse_target_spec "$target"
    fresh_base="${FRESH_ROOT_BASE}/${LABEL}"
    log "Target: ${target}"
    if [[ "$PARSED_TARGET_KIND" == "local" ]]; then
      rm -rf "$fresh_base"
    else
      run_remote_command "$PARSED_TARGET_HOST" rm -rf "$fresh_base"
    fi
    log "Removed fresh test install directory: ${fresh_base}"
  done

  log "Done."
  exit 0
fi

if [[ "$SOURCE" == "tag" && ! -f "$INSTALL_SCRIPT" ]]; then
  fail "missing installer script: ${INSTALL_SCRIPT}"
fi

if [[ "$SOURCE" != "tag" && ! -x "${ROOT_DIR}/${BUILD_SCRIPT_REL}" ]]; then
  fail "missing build script: ${ROOT_DIR}/${BUILD_SCRIPT_REL}"
fi

prepare_source_checkout

for target in "${TARGETS[@]}"; do
  parse_target_spec "$target"
  TARGET_KIND_LIST+=("$PARSED_TARGET_KIND")
  TARGET_HOST_LIST+=("$PARSED_TARGET_HOST")
  TARGET_NAME_LIST+=("$PARSED_TARGET_NAME")

  target_platform="$(detect_target_platform "$PARSED_TARGET_KIND" "$PARSED_TARGET_HOST")"
  target_home="$(detect_target_home "$PARSED_TARGET_KIND" "$PARSED_TARGET_HOST")"
  TARGET_PLATFORM_LIST+=("$target_platform")
  TARGET_HOME_LIST+=("$target_home")
done

if [[ "$MODE" == "fresh" ]]; then
  if [[ -z "$LABEL" ]]; then
    if [[ "$SOURCE" == "tag" ]]; then
      label_base="${TAG:-latest}"
    else
      label_base="$BUILD_LABEL"
    fi
    label_base="$(sanitize_label "${label_base#v}")"
    LABEL="${label_base}-$(date +%Y%m%d-%H%M%S)"
  else
    LABEL="$(sanitize_label "$LABEL")"
  fi
  log "Fresh label: ${LABEL}"
fi

if [[ "$SOURCE" != "tag" ]]; then
  required_binary_platforms=()
  for target_platform in "${TARGET_PLATFORM_LIST[@]-}"; do
    if append_unique "$target_platform" "${required_binary_platforms[@]-}"; then
      required_binary_platforms+=("$target_platform")
    fi
  done
  build_local_or_commit_assets "${required_binary_platforms[@]-}"
fi

index=0
for target in "${TARGETS[@]}"; do
  target_kind="${TARGET_KIND_LIST[$index]}"
  target_host="${TARGET_HOST_LIST[$index]}"
  target_name="${TARGET_NAME_LIST[$index]}"
  target_platform="${TARGET_PLATFORM_LIST[$index]}"
  target_home="${TARGET_HOME_LIST[$index]}"
  index=$((index + 1))

  log "Target: ${target}"

  if [[ "$MODE" == "fresh" ]]; then
    fresh_base="${FRESH_ROOT_BASE}/${LABEL}"
    install_dir="${fresh_base}/bin"
    base_dir="${fresh_base}/workspace"
    backup_dir="${fresh_base}/backups"
    runner_config="${base_dir}/.oxydra/runner.toml"
    runner_env_file="${fresh_base}/runner.env"
    runner_wrapper="${fresh_base}/runner-with-env.sh"
  else
    install_dir="${UPGRADE_INSTALL_DIR:-${target_home}/.local/bin}"
    base_dir="${UPGRADE_BASE_DIR:-${target_home}}"
    backup_dir=""
    runner_config="${base_dir}/.oxydra/runner.toml"
    runner_env_file="${base_dir}/.oxydra/runner.env.test-build"
    runner_wrapper="${base_dir}/.oxydra/runner-with-env.sh"
  fi

  if [[ "$SOURCE" == "tag" ]]; then
    install_args=(--repo "$REPO")
    if [[ -n "$TAG" ]]; then
      install_args+=(--tag "$TAG")
    fi
    if [[ "$AUTO_YES" == "true" ]]; then
      install_args+=(--yes)
    fi
    if [[ "$NO_PULL" == "true" ]]; then
      install_args+=(--no-pull)
    fi

    install_args+=(--install-dir "$install_dir" --base-dir "$base_dir")
    if [[ "$MODE" == "fresh" ]]; then
      install_args+=(--backup-dir "$backup_dir")
    fi

    if [[ "$target_kind" == "local" ]]; then
      "$INSTALL_SCRIPT" "${install_args[@]}"
    else
      run_remote_installer "$target_host" "${install_args[@]}"
    fi
  else
    tarball="${SOURCE_CHECKOUT}/dist/oxydra-${BUILD_LABEL}-${target_platform}.tar.gz"
    [[ -f "$tarball" ]] || fail "missing tarball for ${target_platform}: ${tarball}"

    resolve_image_refs_for_platform "$target_platform"

    if [[ "$target_kind" == "local" ]]; then
      install_binaries_local "$tarball" "$install_dir"
      ensure_config_local "$MODE" "$base_dir" "$SOURCE_CHECKOUT" "$RESOLVED_OXYDRA_IMAGE" "$RESOLVED_SHELL_IMAGE"
    else
      install_binaries_remote "$target_host" "$tarball" "$install_dir"
      maybe_load_images_to_remote "$target_host" "$target_platform"
      ensure_config_remote "$MODE" "$target_host" "$base_dir" "$SOURCE_CHECKOUT" "$RESOLVED_OXYDRA_IMAGE" "$RESOLVED_SHELL_IMAGE"
    fi
  fi

  tmp_wrapper="$(mktemp)"
  write_runner_generic_wrapper_script "$tmp_wrapper" "${install_dir}/runner" "$runner_config" "$runner_env_file"

  if [[ "$target_kind" == "local" ]]; then
    mkdir -p "$(dirname "$runner_wrapper")"
    cp "$tmp_wrapper" "$runner_wrapper"
    chmod 0755 "$runner_wrapper"
    if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
      tmp_env="$(mktemp)"
      write_env_overrides_file "$tmp_env"
      cp "$tmp_env" "$runner_env_file"
      chmod 0600 "$runner_env_file"
      rm -f "$tmp_env"
    fi
  else
    copy_file_to_remote "$target_host" "$tmp_wrapper" "$runner_wrapper" 0755
    if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
      tmp_env="$(mktemp)"
      write_env_overrides_file "$tmp_env"
      copy_file_to_remote "$target_host" "$tmp_env" "$runner_env_file" 0600
      rm -f "$tmp_env"
    fi
  fi
  rm -f "$tmp_wrapper"

  if [[ "$MODE" != "fresh" ]]; then
    if [[ "$target_kind" == "local" ]]; then
      log "Upgrade install dir: ${install_dir}"
      log "Upgrade base dir: ${base_dir}"
      log "Runner wrapper: ${runner_wrapper}"
      if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
        log "Runner env file: ${runner_env_file}"
      fi
      log "Run commands with: ${runner_wrapper} --user alice <status|start|stop|logs ...>"
    else
      log "Upgrade install dir on ${target_name}: ${install_dir}"
      log "Upgrade base dir on ${target_name}: ${base_dir}"
      log "Runner wrapper on ${target_name}: ${runner_wrapper}"
      if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
        log "Runner env file on ${target_name}: ${runner_env_file}"
      fi
      log "Run commands on ${target_name}: ssh ${target_name} ${runner_wrapper} --user alice <status|start|stop|logs ...>"
    fi
    continue
  fi

  start_cmd="$(quote_args "$runner_wrapper" --user alice start)"
  web_cmd="$(quote_args "$runner_wrapper" web --bind "$WEB_BIND")"
  cleanup_cmd="$(quote_args rm -rf "$fresh_base")"

  if [[ "$target_kind" == "local" ]]; then
    log "Fresh install path: ${fresh_base}"
    log "Runner wrapper: ${runner_wrapper}"
    if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
      log "Runner env file: ${runner_env_file}"
    fi
    log "Start runner daemon: ${start_cmd}"
    log "Open onboarding wizard: ${web_cmd}"
    log "Discard this fresh install: ${cleanup_cmd}"
    if [[ "$START_WEB" == "true" ]]; then
      "$runner_wrapper" web --bind "$WEB_BIND"
    fi
  else
    log "Fresh install path on ${target_name}: ${fresh_base}"
    log "Runner wrapper on ${target_name}: ${runner_wrapper}"
    if [[ "${#ENV_OVERRIDES[@]}" -gt 0 ]]; then
      log "Runner env file on ${target_name}: ${runner_env_file}"
    fi
    log "Start runner daemon on ${target_name}: ssh ${target_name} ${start_cmd}"
    log "Open onboarding wizard on ${target_name}: ssh ${target_name} ${web_cmd}"
    log "Discard this fresh install on ${target_name}: ssh ${target_name} ${cleanup_cmd}"
    if [[ "$START_WEB" == "true" ]]; then
      log "Use SSH port-forward in another terminal: ssh -L 9400:${WEB_BIND%:*}:${WEB_BIND##*:} ${target_name}"
      run_remote_command "$target_host" "$runner_wrapper" web --bind "$WEB_BIND"
    fi
  fi
done

if [[ "$SOURCE" != "tag" ]]; then
  if [[ "$SKIP_DOCKER_IMAGES" == "true" ]]; then
    log "Docker images: skipped (--skip-docker-images)"
  elif [[ "$PUSH_IMAGES" == "true" ]]; then
    log "Docker images pushed: ghcr.io/${IMAGE_NAMESPACE}/{oxydra-vm,shell-vm}:${BUILD_LABEL}"
  else
    log "Docker images built locally with label: ${BUILD_LABEL}"
  fi
fi

log "Done."
