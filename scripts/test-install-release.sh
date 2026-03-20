#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SCRIPT="${ROOT_DIR}/scripts/install-release.sh"

PASS_COUNT=0
FAIL_COUNT=0
TOTAL_COUNT=0

ORIGINAL_PATH="$PATH"

detect_host_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "${os}:${arch}" in
    Darwin:arm64|Darwin:aarch64) printf '%s' "macos-arm64" ;;
    Linux:x86_64|Linux:amd64) printf '%s' "linux-amd64" ;;
    Linux:aarch64|Linux:arm64) printf '%s' "linux-arm64" ;;
    *)
      echo "Unsupported host platform for installer tests: ${os} ${arch}" >&2
      return 1
      ;;
  esac
}

HOST_PLATFORM="$(detect_host_platform)"

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
  echo "Missing sha256sum/shasum on PATH" >&2
  return 1
}

assert_contains() {
  local value="$1"
  local needle="$2"
  if [[ "$value" != *"$needle"* ]]; then
    echo "Assertion failed: expected output to contain: $needle" >&2
    return 1
  fi
}

assert_not_contains() {
  local value="$1"
  local needle="$2"
  if [[ "$value" == *"$needle"* ]]; then
    echo "Assertion failed: expected output to NOT contain: $needle" >&2
    return 1
  fi
}

assert_equals() {
  local expected="$1"
  local actual="$2"
  if [[ "$expected" != "$actual" ]]; then
    echo "Assertion failed: expected [$expected], got [$actual]" >&2
    return 1
  fi
}

assert_file_exists() {
  local path="$1"
  if [[ ! -e "$path" ]]; then
    echo "Assertion failed: expected file to exist: $path" >&2
    return 1
  fi
}

assert_file_not_exists() {
  local path="$1"
  if [[ -e "$path" ]]; then
    echo "Assertion failed: expected file to NOT exist: $path" >&2
    return 1
  fi
}

assert_executable() {
  local path="$1"
  if [[ ! -x "$path" ]]; then
    echo "Assertion failed: expected executable file: $path" >&2
    return 1
  fi
}

assert_symlink_target() {
  local path="$1"
  local expected_target="$2"
  if [[ ! -L "$path" ]]; then
    echo "Assertion failed: expected symlink: $path" >&2
    return 1
  fi
  local actual_target
  actual_target="$(readlink "$path")"
  if [[ "$actual_target" != "$expected_target" ]]; then
    echo "Assertion failed: expected symlink $path -> $expected_target, got $actual_target" >&2
    return 1
  fi
}

assert_file_contains_literal() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "Assertion failed: expected file $file to contain: $needle" >&2
    return 1
  fi
}

assert_file_empty() {
  local file="$1"
  if [[ -s "$file" ]]; then
    echo "Assertion failed: expected file to be empty: $file" >&2
    return 1
  fi
}

cleanup_case() {
  if [[ "${KEEP_INSTALLER_TEST_ARTIFACTS:-0}" == "1" ]]; then
    echo "Keeping test artifacts at: ${CASE_ROOT}" >&2
    return
  fi
  rm -rf "${CASE_ROOT}"
}

write_runner_stub() {
  local destination="$1"
  local version="$2"
  local binary_name="${3:-oxydra}"
  cat >"$destination" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "--version" ]]; then
  echo "${binary_name} ${version}"
  exit 0
fi

config=""
user=""
command=""
while [[ \$# -gt 0 ]]; do
  case "\$1" in
    --config)
      config="\$2"
      shift 2
      ;;
    --user)
      user="\$2"
      shift 2
      ;;
    start|stop|status)
      command="\$1"
      shift
      break
      ;;
    *)
      shift
      ;;
  esac
done

if [[ -z "\$command" ]]; then
  exit 0
fi

workspace_root="workspaces"
if [[ -n "\$config" && -f "\$config" ]]; then
  parsed_root="\$(sed -nE 's/^[[:space:]]*workspace_root[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "\$config" | head -n 1)"
  if [[ -n "\$parsed_root" ]]; then
    workspace_root="\$parsed_root"
  fi
  config_dir="\$(cd "\$(dirname "\$config")" && pwd)"
  if [[ "\$workspace_root" != /* ]]; then
    workspace_root="\${config_dir}/\${workspace_root}"
  fi
fi

if [[ -z "\$user" ]]; then
  user="alice"
fi

socket_path="\${workspace_root}/\${user}/ipc/runner-control.sock"
case "\$command" in
  status)
    [[ -e "\$socket_path" ]] && exit 0 || exit 1
    ;;
  stop)
    rm -f "\$socket_path"
    exit 0
    ;;
  start)
    mkdir -p "\$(dirname "\$socket_path")"
    : > "\$socket_path"
    if [[ -n "\${RUNNER_START_LOG:-}" ]]; then
      echo "\$user" >> "\${RUNNER_START_LOG}"
    fi
    exit 0
    ;;
esac
EOF
  chmod +x "$destination"
}

write_binary_stub() {
  local destination="$1"
  local name="$2"
  cat >"$destination" <<EOF
#!/usr/bin/env bash
set -euo pipefail
echo "${name}"
EOF
  chmod +x "$destination"
}

write_mock_commands() {
  local mock_bin="$1"
  mkdir -p "$mock_bin"

  cat >"${mock_bin}/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

output_path=""
url=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -o)
      output_path="$2"
      shift 2
      ;;
    --retry|-H|--user-agent)
      shift 2
      ;;
    -f|-L|-s|-S|--fail|--location)
      shift
      ;;
    http://*|https://*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

if [[ -n "${MOCK_CURL_LOG:-}" ]]; then
  printf '%s\n' "$url" >> "${MOCK_CURL_LOG}"
fi

if [[ "$url" == *"/releases/latest" ]]; then
  printf '{\n  "tag_name": "%s"\n}\n' "${MOCK_LATEST_TAG:-v0.0.0}"
  exit 0
fi

if [[ "$url" =~ /releases/download/([^/]+)/([^/?]+)$ ]]; then
  tag="${BASH_REMATCH[1]}"
  file="${BASH_REMATCH[2]}"
  source_path="${MOCK_RELEASES_DIR}/${tag}/${file}"
  [[ -f "$source_path" ]] || exit 22
  if [[ -n "$output_path" ]]; then
    cp "$source_path" "$output_path"
  else
    cat "$source_path"
  fi
  exit 0
fi

if [[ "$url" =~ /examples/config/([^/?]+)$ ]]; then
  file="${BASH_REMATCH[1]}"
  source_path="${MOCK_RAW_CONFIG_DIR}/${file}"
  [[ -f "$source_path" ]] || exit 22
  if [[ -n "$output_path" ]]; then
    cp "$source_path" "$output_path"
  else
    cat "$source_path"
  fi
  exit 0
fi

exit 22
EOF
  chmod +x "${mock_bin}/curl"

  cat >"${mock_bin}/install" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

mode="0755"
if [[ "${1:-}" == "-m" ]]; then
  mode="$2"
  shift 2
fi

src="$1"
dst="$2"

if [[ "${MOCK_INSTALL_FAIL_ONCE:-0}" == "1" ]]; then
  marker="${MOCK_INSTALL_FAIL_MARKER:?MOCK_INSTALL_FAIL_MARKER must be set when MOCK_INSTALL_FAIL_ONCE=1}"
  target_binary="${MOCK_INSTALL_FAIL_ON_BINARY:-}"
  should_fail=false
  if [[ -z "$target_binary" ]]; then
    should_fail=true
  elif [[ "$(basename "$dst")" == "$target_binary" ]]; then
    should_fail=true
  fi
  if [[ "$should_fail" == "true" && ! -f "$marker" ]]; then
    : > "$marker"
    echo "mock install: intentional one-time failure" >&2
    exit 1
  fi
fi

cp "$src" "$dst"
chmod "$mode" "$dst"
EOF
  chmod +x "${mock_bin}/install"

  cat >"${mock_bin}/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${MOCK_DOCKER_LOG:-}" ]]; then
  printf '%s\n' "$*" >> "${MOCK_DOCKER_LOG}"
fi
if [[ "${MOCK_DOCKER_FAIL:-0}" == "1" ]]; then
  exit 1
fi
exit 0
EOF
  chmod +x "${mock_bin}/docker"
}

setup_case() {
  CASE_ROOT="$(mktemp -d)"
  trap cleanup_case EXIT

  RELEASES_DIR="${CASE_ROOT}/releases"
  RAW_CONFIG_DIR="${CASE_ROOT}/raw-config"
  MOCK_BIN="${CASE_ROOT}/mock-bin"
  WORKSPACE="${CASE_ROOT}/workspace"
  INSTALL_DIR="${CASE_ROOT}/install-bin"
  BACKUP_DIR="${CASE_ROOT}/backups"
  LOG_DIR="${CASE_ROOT}/logs"

  mkdir -p "$RELEASES_DIR" "$RAW_CONFIG_DIR" "$MOCK_BIN" "$WORKSPACE" "$INSTALL_DIR" "$LOG_DIR"
  : > "${LOG_DIR}/curl.log"
  : > "${LOG_DIR}/docker.log"
  : > "${LOG_DIR}/runner-start.log"

  write_mock_commands "$MOCK_BIN"

  export PATH="${MOCK_BIN}:${ORIGINAL_PATH}"
  export MOCK_RELEASES_DIR="$RELEASES_DIR"
  export MOCK_RAW_CONFIG_DIR="$RAW_CONFIG_DIR"
  export MOCK_CURL_LOG="${LOG_DIR}/curl.log"
  export MOCK_DOCKER_LOG="${LOG_DIR}/docker.log"
  export MOCK_INSTALL_FAIL_MARKER="${LOG_DIR}/install-failed-once.marker"
  export RUNNER_START_LOG="${LOG_DIR}/runner-start.log"
  unset MOCK_INSTALL_FAIL_ONCE MOCK_DOCKER_FAIL MOCK_INSTALL_FAIL_ON_BINARY MOCK_LATEST_TAG
}

create_release_fixture() {
  local tag="$1"
  local version="$2"
  local release_dir="${RELEASES_DIR}/${tag}"
  local payload_dir="${CASE_ROOT}/payload-${tag}"
  local archive_name="oxydra-${tag}-${HOST_PLATFORM}.tar.gz"

  mkdir -p "${release_dir}" "${payload_dir}" "${payload_dir}/examples/config" "${payload_dir}/examples/config/users"

  write_runner_stub "${payload_dir}/oxydra" "$version"
  write_binary_stub "${payload_dir}/oxydra-vm" "oxydra-vm ${version}"
  write_binary_stub "${payload_dir}/oxydra-shelld" "oxydra-shelld ${version}"
  write_binary_stub "${payload_dir}/oxydra-tui" "oxydra-tui ${version}"

  cat > "${payload_dir}/examples/config/runner.toml" <<'EOF'
config_version = "1.0.1"
workspace_root = "workspaces"
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:v0.0.0"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:v0.0.0"

[users.alice]
config_path = "users/alice.toml"
EOF

  cat > "${payload_dir}/examples/config/agent.toml" <<'EOF'
config_version = "1.0.0"
[memory]
enabled = true
[selection]
provider = "openai"
model = "gpt-4o-mini"
EOF

  cat > "${payload_dir}/examples/config/runner-user.toml" <<'EOF'
config_version = "1.0.1"
EOF

  cp "${payload_dir}/examples/config/runner.toml" "${RAW_CONFIG_DIR}/runner.toml"
  cp "${payload_dir}/examples/config/agent.toml" "${RAW_CONFIG_DIR}/agent.toml"
  cp "${payload_dir}/examples/config/runner-user.toml" "${RAW_CONFIG_DIR}/runner-user.toml"

  tar -czf "${release_dir}/${archive_name}" -C "$payload_dir" .
  local checksum
  checksum="$(sha256_file "${release_dir}/${archive_name}")"
  printf '%s  %s\n' "$checksum" "dist/${archive_name}" > "${release_dir}/SHA256SUMS"
}

setup_existing_install() {
  local version="$1"
  mkdir -p "$INSTALL_DIR"
  write_runner_stub "${INSTALL_DIR}/oxydra" "$version"
  write_binary_stub "${INSTALL_DIR}/oxydra-vm" "old-oxydra-vm ${version}"
  write_binary_stub "${INSTALL_DIR}/oxydra-shelld" "old-oxydra-shelld ${version}"
  write_binary_stub "${INSTALL_DIR}/oxydra-tui" "old-oxydra-tui ${version}"
}

setup_existing_legacy_install() {
  local version="$1"
  mkdir -p "$INSTALL_DIR"
  write_runner_stub "${INSTALL_DIR}/runner" "$version" "runner"
  write_binary_stub "${INSTALL_DIR}/oxydra-vm" "old-oxydra-vm ${version}"
  write_binary_stub "${INSTALL_DIR}/shell-daemon" "old-shell-daemon ${version}"
  write_binary_stub "${INSTALL_DIR}/oxydra-tui" "old-oxydra-tui ${version}"
}

setup_existing_config() {
  mkdir -p "${WORKSPACE}/.oxydra/users"
  cat > "${WORKSPACE}/.oxydra/runner.toml" <<'EOF'
config_version = "1.0.1"
workspace_root = "workspaces"
default_tier = "container"

[guest_images]
oxydra_vm = "registry.example.com/acme/oxydra-vm:old-custom" # keep comment
shell_vm  = "docker.io/acme/shell-vm:legacy-old"

[users.alice]
config_path = "users/alice.toml"
EOF
  cat > "${WORKSPACE}/.oxydra/agent.toml" <<'EOF'
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
EOF
  cat > "${WORKSPACE}/.oxydra/users/alice.toml" <<'EOF'
config_version = "1.0.1"
EOF
}

latest_backup_dir() {
  ls -1dt "${BACKUP_DIR}"/* 2>/dev/null | head -n 1
}

run_installer_capture() {
  local expected_status="$1"
  shift

  local output status
  set +e
  output="$("${INSTALL_SCRIPT}" "$@" 2>&1)"
  status=$?
  set -e

  if [[ "$status" -ne "$expected_status" ]]; then
    echo "Unexpected installer exit status: got ${status}, expected ${expected_status}" >&2
    echo "--- installer output ---" >&2
    echo "$output" >&2
    return 1
  fi

  printf '%s' "$output"
}

test_fresh_install_path() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Checksum verified: oxydra-v2.0.0-${HOST_PLATFORM}.tar.gz"
  assert_executable "${INSTALL_DIR}/oxydra"
  assert_executable "${INSTALL_DIR}/oxydra-vm"
  assert_executable "${INSTALL_DIR}/oxydra-shelld"
  assert_executable "${INSTALL_DIR}/oxydra-tui"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:v2.0.0"'
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'shell_vm  = "ghcr.io/shantanugoel/shell-vm:v2.0.0"'
  assert_file_not_exists "$BACKUP_DIR"
  assert_contains "$output" "Update these values before first run"
}

test_upgrade_updates_tags_and_creates_backups() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  mkdir -p "${WORKSPACE}/.oxydra/workspaces/alice/ipc"
  : > "${WORKSPACE}/.oxydra/workspaces/alice/ipc/runner-control.sock"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Runner daemon is currently active for user(s): alice"
  assert_contains "$output" "Stop it now? [auto-yes]"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "registry.example.com/acme/oxydra-vm:v2.0.0" # keep comment'
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'shell_vm  = "docker.io/acme/shell-vm:v2.0.0"'
  assert_file_exists "${WORKSPACE}/.oxydra/runner.toml.v2.0.0.new"
  assert_file_exists "${WORKSPACE}/.oxydra/agent.toml.v2.0.0.new"
  assert_file_exists "${WORKSPACE}/.oxydra/runner-user.toml.v2.0.0.new"
  assert_file_contains_literal "${RUNNER_START_LOG}" "alice"

  local backup_path
  backup_path="$(latest_backup_dir)"
  [[ -n "$backup_path" ]] || {
    echo "Assertion failed: expected backup directory to exist" >&2
    return 1
  }

  assert_file_exists "${backup_path}/binaries/oxydra"
  assert_file_contains_literal "${backup_path}/config/.oxydra/runner.toml" 'oxydra_vm = "registry.example.com/acme/oxydra-vm:old-custom" # keep comment'
  assert_not_contains "$output" "Update these values before first run"
  assert_contains "$output" "Config updated in"
}

test_legacy_upgrade_creates_compat_symlinks_and_backups() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_legacy_install "1.0.0"
  setup_existing_config

  mkdir -p "${WORKSPACE}/.oxydra/workspaces/alice/ipc"
  : > "${WORKSPACE}/.oxydra/workspaces/alice/ipc/runner-control.sock"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Runner daemon is currently active for user(s): alice"
  assert_contains "$output" "'runner' has been renamed to 'oxydra'. A compatibility symlink was created."
  assert_contains "$output" "'shell-daemon' has been renamed to 'oxydra-shelld'. A compatibility symlink was created."
  assert_executable "${INSTALL_DIR}/oxydra"
  assert_executable "${INSTALL_DIR}/oxydra-shelld"
  assert_symlink_target "${INSTALL_DIR}/runner" "oxydra"
  assert_symlink_target "${INSTALL_DIR}/shell-daemon" "oxydra-shelld"
  assert_file_contains_literal "${RUNNER_START_LOG}" "alice"

  local backup_path
  backup_path="$(latest_backup_dir)"
  [[ -n "$backup_path" ]] || {
    echo "Assertion failed: expected legacy backup directory to exist" >&2
    return 1
  }

  assert_file_exists "${backup_path}/binaries/runner"
  assert_file_exists "${backup_path}/binaries/shell-daemon"
  assert_contains "$("${INSTALL_DIR}/runner" --version)" "2.0.0"
  assert_contains "$("${INSTALL_DIR}/shell-daemon")" "oxydra-shelld 2.0.0"
}

test_dry_run_keeps_state_unchanged() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  local runner_before version_before output runner_after version_after
  runner_before="$(cat "${WORKSPACE}/.oxydra/runner.toml")"
  version_before="$("${INSTALL_DIR}/oxydra" --version)"

  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --dry-run)"

  assert_contains "$output" "Dry-run mode enabled. No changes will be made."
  runner_after="$(cat "${WORKSPACE}/.oxydra/runner.toml")"
  version_after="$("${INSTALL_DIR}/oxydra" --version)"
  assert_equals "$runner_before" "$runner_after"
  assert_equals "$version_before" "$version_after"
  assert_file_not_exists "${WORKSPACE}/.oxydra/runner.toml.v2.0.0.new"
  assert_file_not_exists "$BACKUP_DIR"
}

test_same_version_guard_and_force_mode() {
  setup_case
  setup_existing_install "2.0.0"

  : > "${LOG_DIR}/curl.log"
  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR")"

  assert_contains "$output" "Already at v2.0.0. Use --force to reinstall."
  assert_file_empty "${LOG_DIR}/curl.log"

  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --force \
    --yes \
    --dry-run)"
  assert_contains "$output" "Reinstalling v2.0.0 (--force)."
  assert_contains "$output" "Dry-run mode enabled. No changes will be made."
}

test_checksum_mismatch_aborts_upgrade() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  local archive_name="oxydra-v2.0.0-${HOST_PLATFORM}.tar.gz"
  printf '%s  %s\n' "deadbeef" "$archive_name" > "${RELEASES_DIR}/v2.0.0/SHA256SUMS"

  local output
  output="$(run_installer_capture 1 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "checksum verification failed"
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "1.0.0"
  assert_file_not_exists "$BACKUP_DIR"
}

test_rollback_restores_after_failed_install_step() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  export MOCK_INSTALL_FAIL_ONCE=1

  local output
  output="$(run_installer_capture 1 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Installation failed. Restore from backup? [auto-yes]"
  assert_contains "$output" "Rollback complete."
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "1.0.0"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "registry.example.com/acme/oxydra-vm:old-custom" # keep comment'

  local backup_path
  backup_path="$(latest_backup_dir)"
  [[ -n "$backup_path" ]] || {
    echo "Assertion failed: expected rollback backup directory to exist" >&2
    return 1
  }
}

test_legacy_upgrade_rollback_restores_legacy_binaries() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_legacy_install "1.0.0"
  setup_existing_config

  export MOCK_INSTALL_FAIL_ONCE=1
  export MOCK_INSTALL_FAIL_ON_BINARY=oxydra-shelld

  local output
  output="$(run_installer_capture 1 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Installation failed. Restore from backup? [auto-yes]"
  assert_contains "$output" "Rollback complete."
  assert_contains "$("${INSTALL_DIR}/runner" --version)" "1.0.0"
  assert_contains "$("${INSTALL_DIR}/oxydra-vm")" "old-oxydra-vm 1.0.0"
  assert_contains "$("${INSTALL_DIR}/shell-daemon")" "old-shell-daemon 1.0.0"
  assert_file_not_exists "${INSTALL_DIR}/oxydra"
  assert_file_not_exists "${INSTALL_DIR}/oxydra-shelld"
}

test_docker_prepull_runs_when_not_disabled() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes)"

  assert_not_contains "$output" "Skipping Docker image pre-pull (--no-pull)"
  assert_file_contains_literal "${LOG_DIR}/docker.log" 'pull ghcr.io/shantanugoel/oxydra-vm:v2.0.0'
  assert_file_contains_literal "${LOG_DIR}/docker.log" 'pull ghcr.io/shantanugoel/shell-vm:v2.0.0'
}

test_no_pull_flag_skips_docker_prepull() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Skipping Docker image pre-pull (--no-pull)"
  assert_file_empty "${LOG_DIR}/docker.log"
}

# ---------------------------------------------------------------------------
# Additional helpers for new test cases
# ---------------------------------------------------------------------------

setup_existing_config_multi_user() {
  mkdir -p "${WORKSPACE}/.oxydra/users"
  cat > "${WORKSPACE}/.oxydra/runner.toml" <<'EOF'
config_version = "1.0.1"
workspace_root = "workspaces"
default_tier = "container"

[guest_images]
oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:v1.0.0"
shell_vm  = "ghcr.io/shantanugoel/shell-vm:v1.0.0"

[users.alice]
config_path = "users/alice.toml"

[users.bob]
config_path = "users/bob.toml"
EOF
  cat > "${WORKSPACE}/.oxydra/agent.toml" <<'EOF'
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
EOF
  cat > "${WORKSPACE}/.oxydra/users/alice.toml" <<'EOF'
config_version = "1.0.1"
EOF
  cat > "${WORKSPACE}/.oxydra/users/bob.toml" <<'EOF'
config_version = "1.0.1"
EOF
}

# ---------------------------------------------------------------------------
# New test cases
# ---------------------------------------------------------------------------

test_downgrade_prints_warning() {
  setup_case
  create_release_fixture "v1.0.0" "1.0.0"
  setup_existing_install "2.0.0"
  setup_existing_config

  local output
  output="$(run_installer_capture 0 \
    --tag "v1.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Warning: downgrading from v2.0.0 -> v1.0.0"
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "1.0.0"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "registry.example.com/acme/oxydra-vm:v1.0.0" # keep comment'
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'shell_vm  = "docker.io/acme/shell-vm:v1.0.0"'
}

test_skip_config_omits_config_directory() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull \
    --skip-config)"

  assert_executable "${INSTALL_DIR}/oxydra"
  assert_executable "${INSTALL_DIR}/oxydra-vm"
  assert_file_not_exists "${WORKSPACE}/.oxydra"
  assert_contains "$output" "Skipping config initialization and update (--skip-config)"
  assert_not_contains "$output" "Update these values before first run"
}

test_overwrite_config_replaces_existing() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull \
    --overwrite-config)"

  # Config should be replaced with template defaults (ghcr.io, not custom registry)
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "ghcr.io/shantanugoel/oxydra-vm:v2.0.0"'
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'shell_vm  = "ghcr.io/shantanugoel/shell-vm:v2.0.0"'
  # No .new templates since config was overwritten (fresh template path)
  assert_file_not_exists "${WORKSPACE}/.oxydra/runner.toml.v2.0.0.new"
}

test_rollback_after_partial_binary_install() {
  # Fail on a LATER binary (oxydra-shelld) so runner + oxydra-vm are already
  # overwritten with new versions. Rollback must restore ALL of them.
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  export MOCK_INSTALL_FAIL_ONCE=1
  export MOCK_INSTALL_FAIL_ON_BINARY=oxydra-shelld

  local output
  output="$(run_installer_capture 1 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Installation failed. Restore from backup? [auto-yes]"
  assert_contains "$output" "Rollback complete."
  # All binaries must be restored to old version, including runner which was
  # successfully overwritten before oxydra-shelld failed.
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "1.0.0"
  assert_contains "$("${INSTALL_DIR}/oxydra-vm")" "old-oxydra-vm 1.0.0"
  assert_contains "$("${INSTALL_DIR}/oxydra-shelld")" "old-oxydra-shelld 1.0.0"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "registry.example.com/acme/oxydra-vm:old-custom" # keep comment'
}

test_latest_tag_resolution_without_explicit_tag() {
  setup_case
  create_release_fixture "v3.0.0" "3.0.0"
  export MOCK_LATEST_TAG="v3.0.0"

  local output
  output="$(run_installer_capture 0 \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "Installing Oxydra v3.0.0"
  assert_executable "${INSTALL_DIR}/oxydra"
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "3.0.0"
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" ':v3.0.0"'
}

test_backup_rotation_keeps_three() {
  setup_case
  create_release_fixture "v1.1.0" "1.1.0"
  create_release_fixture "v1.2.0" "1.2.0"
  create_release_fixture "v1.3.0" "1.3.0"
  create_release_fixture "v1.4.0" "1.4.0"

  # Install sequence: 1.0.0 -> 1.1.0 -> 1.2.0 -> 1.3.0 -> 1.4.0
  # Creates 4 backups; rotation should prune the oldest, keeping 3.
  setup_existing_install "1.0.0"
  run_installer_capture 0 --tag "v1.1.0" --base-dir "$WORKSPACE" --install-dir "$INSTALL_DIR" --backup-dir "$BACKUP_DIR" --yes --no-pull --skip-config >/dev/null
  run_installer_capture 0 --tag "v1.2.0" --base-dir "$WORKSPACE" --install-dir "$INSTALL_DIR" --backup-dir "$BACKUP_DIR" --yes --no-pull --skip-config >/dev/null
  run_installer_capture 0 --tag "v1.3.0" --base-dir "$WORKSPACE" --install-dir "$INSTALL_DIR" --backup-dir "$BACKUP_DIR" --yes --no-pull --skip-config >/dev/null
  run_installer_capture 0 --tag "v1.4.0" --base-dir "$WORKSPACE" --install-dir "$INSTALL_DIR" --backup-dir "$BACKUP_DIR" --yes --no-pull --skip-config >/dev/null

  local backup_count
  backup_count="$(find "$BACKUP_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')"
  assert_equals "3" "$backup_count"
}

test_multi_user_daemon_stop_and_restart() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config_multi_user

  # Create socket files for both users so they are detected as active
  mkdir -p "${WORKSPACE}/.oxydra/workspaces/alice/ipc"
  : > "${WORKSPACE}/.oxydra/workspaces/alice/ipc/runner-control.sock"
  mkdir -p "${WORKSPACE}/.oxydra/workspaces/bob/ipc"
  : > "${WORKSPACE}/.oxydra/workspaces/bob/ipc/runner-control.sock"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_contains "$output" "alice"
  assert_contains "$output" "bob"
  assert_contains "$output" "Stop it now? [auto-yes]"
  assert_contains "$output" "Restart the runner daemon for user(s):"
  # Verify both users were restarted
  assert_file_contains_literal "${RUNNER_START_LOG}" "alice"
  assert_file_contains_literal "${RUNNER_START_LOG}" "bob"
}

test_upgrade_preserves_custom_registry_with_port() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"

  mkdir -p "${WORKSPACE}/.oxydra/users"
  cat > "${WORKSPACE}/.oxydra/runner.toml" <<'EOF'
config_version = "1.0.1"
workspace_root = "workspaces"
default_tier = "container"

[guest_images]
oxydra_vm = "myregistry.local:5000/custom/oxydra-vm:v1.0.0"
shell_vm  = "myregistry.local:5000/custom/shell-vm:v1.0.0"

[users.alice]
config_path = "users/alice.toml"
EOF
  cat > "${WORKSPACE}/.oxydra/agent.toml" <<'EOF'
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
EOF
  cat > "${WORKSPACE}/.oxydra/users/alice.toml" <<'EOF'
config_version = "1.0.1"
EOF

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  # Port-based registry must be preserved; only the tag after last colon changes
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'oxydra_vm = "myregistry.local:5000/custom/oxydra-vm:v2.0.0"'
  assert_file_contains_literal "${WORKSPACE}/.oxydra/runner.toml" 'shell_vm  = "myregistry.local:5000/custom/shell-vm:v2.0.0"'
}

test_noninteractive_stdin_aborts_when_daemon_running() {
  # When running via curl|bash (stdin not a terminal) without --yes,
  # the script must abort cleanly instead of hanging or silently proceeding.
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"
  setup_existing_install "1.0.0"
  setup_existing_config

  mkdir -p "${WORKSPACE}/.oxydra/workspaces/alice/ipc"
  : > "${WORKSPACE}/.oxydra/workspaces/alice/ipc/runner-control.sock"

  # Redirect stdin from /dev/null to simulate piped (non-interactive) mode.
  # Do NOT pass --yes. The script should refuse to stop the daemon.
  local output status
  status=0
  output="$("${INSTALL_SCRIPT}" \
    --tag "v2.0.0" \
    --base-dir "$WORKSPACE" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --no-pull < /dev/null 2>&1)" || status=$?

  [[ "$status" -ne 0 ]] || {
    echo "Assertion failed: expected non-zero exit for non-interactive with daemon" >&2
    return 1
  }
  assert_contains "$output" "could not stop running daemon"
  # Binaries must remain untouched
  assert_contains "$("${INSTALL_DIR}/oxydra" --version)" "1.0.0"
}

test_base_dir_auto_created() {
  setup_case
  create_release_fixture "v2.0.0" "2.0.0"

  local new_base="${CASE_ROOT}/deep/nested/project"

  local output
  output="$(run_installer_capture 0 \
    --tag "v2.0.0" \
    --base-dir "$new_base" \
    --install-dir "$INSTALL_DIR" \
    --backup-dir "$BACKUP_DIR" \
    --yes \
    --no-pull)"

  assert_file_exists "${new_base}/.oxydra/runner.toml"
  assert_file_exists "${new_base}/.oxydra/agent.toml"
  assert_executable "${INSTALL_DIR}/oxydra"
}

run_case() {
  local name="$1"
  TOTAL_COUNT=$((TOTAL_COUNT + 1))
  printf '==> %s\n' "$name"

  local status=0
  set +e
  ( set -euo pipefail; "$name" )
  status=$?
  set -e

  if [[ "$status" -eq 0 ]]; then
    PASS_COUNT=$((PASS_COUNT + 1))
    printf 'PASS: %s\n\n' "$name"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    printf 'FAIL: %s\n\n' "$name"
  fi
}

main() {
  [[ -x "$INSTALL_SCRIPT" ]] || {
    echo "Installer script is missing or not executable: $INSTALL_SCRIPT" >&2
    exit 1
  }

  run_case test_fresh_install_path
  run_case test_upgrade_updates_tags_and_creates_backups
  run_case test_legacy_upgrade_creates_compat_symlinks_and_backups
  run_case test_dry_run_keeps_state_unchanged
  run_case test_same_version_guard_and_force_mode
  run_case test_checksum_mismatch_aborts_upgrade
  run_case test_rollback_restores_after_failed_install_step
  run_case test_legacy_upgrade_rollback_restores_legacy_binaries
  run_case test_docker_prepull_runs_when_not_disabled
  run_case test_no_pull_flag_skips_docker_prepull
  run_case test_downgrade_prints_warning
  run_case test_skip_config_omits_config_directory
  run_case test_overwrite_config_replaces_existing
  run_case test_rollback_after_partial_binary_install
  run_case test_latest_tag_resolution_without_explicit_tag
  run_case test_backup_rotation_keeps_three
  run_case test_multi_user_daemon_stop_and_restart
  run_case test_upgrade_preserves_custom_registry_with_port
  run_case test_noninteractive_stdin_aborts_when_daemon_running
  run_case test_base_dir_auto_created

  printf 'Installer test summary: %d passed, %d failed, %d total\n' "$PASS_COUNT" "$FAIL_COUNT" "$TOTAL_COUNT"
  if [[ "$FAIL_COUNT" -ne 0 ]]; then
    exit 1
  fi
}

main "$@"
