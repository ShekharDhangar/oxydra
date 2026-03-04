#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SCRIPT="${ROOT_DIR}/scripts/install-release.sh"

MODE="fresh"
TAG=""
REPO="shantanugoel/oxydra"
FRESH_ROOT_BASE="${OXYDRA_FRESH_ROOT:-/tmp/oxydra-fresh-tests}"
LABEL=""
START_WEB=false
WEB_BIND="127.0.0.1:9400"
NO_PULL=false
AUTO_YES=true
UPGRADE_INSTALL_DIR=""
UPGRADE_BASE_DIR=""
TARGETS=()

usage() {
  cat <<'EOF'
Run repeatable Oxydra install tests on local and SSH targets.

Usage:
  ./scripts/test-build-install.sh [options]

Options:
  --mode <fresh|fresh-clean|upgrade>
                          fresh: isolated install for onboarding tests (default)
                          fresh-clean: remove isolated install by label
                          upgrade: normal upgrade on existing setup
  --target <local|ssh:user@host|user@host>
                          Target host; repeatable. Default: local
  --tag <tag>            Release tag (optional; defaults to latest in installer)
  --repo <owner/name>    GitHub repo to install from (default: shantanugoel/oxydra)
  --label <name>         Label for isolated fresh install directory
                          (required for --mode fresh-clean)
  --fresh-root <path>    Base dir for fresh installs (default: /tmp/oxydra-fresh-tests)
  --start-web            Start onboarding web configurator after fresh install
  --web-bind <addr>      Web bind address when --start-web is used
                          (default: 127.0.0.1:9400)
  --no-pull              Pass --no-pull to installer
  --interactive          Do not auto-pass --yes to installer
  --install-dir <path>   Override install dir for --mode upgrade
  --base-dir <path>      Override base dir for --mode upgrade
  -h, --help             Show help

Examples:
  ./scripts/test-build-install.sh --mode fresh --tag v0.2.3 --target local --target ssh:pi@raspberrypi.local
  ./scripts/test-build-install.sh --mode upgrade --tag v0.2.3 --target local --target pi@raspberrypi.local
  ./scripts/test-build-install.sh --mode fresh-clean --label v0.2.3-20260304-044500 --target local --target pi@raspberrypi.local
EOF
}

log() {
  printf '[oxydra-build-test] %s\n' "$*"
}

fail() {
  printf '[oxydra-build-test] Error: %s\n' "$*" >&2
  exit 1
}

sanitize_label() {
  printf '%s' "$1" | tr -c '[:alnum:]._-' '-'
}

quote_args() {
  local out="" arg
  for arg in "$@"; do
    out="${out} $(printf '%q' "$arg")"
  done
  printf '%s' "${out# }"
}

run_remote_command() {
  local host="$1"
  shift
  ssh "$host" "$(quote_args "$@")"
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

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="${2:?Missing value for --mode}"
      shift 2
      ;;
    --target)
      TARGETS+=("${2:?Missing value for --target}")
      shift 2
      ;;
    --tag)
      TAG="${2:?Missing value for --tag}"
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
    --install-dir)
      UPGRADE_INSTALL_DIR="${2:?Missing value for --install-dir}"
      shift 2
      ;;
    --base-dir)
      UPGRADE_BASE_DIR="${2:?Missing value for --base-dir}"
      shift 2
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
  *)
    fail "--mode must be one of: fresh, fresh-clean, upgrade"
    ;;
esac

if [[ "$START_WEB" == "true" && "$MODE" != "fresh" ]]; then
  fail "--start-web is only valid with --mode fresh"
fi

if [[ "${#TARGETS[@]}" -eq 0 ]]; then
  TARGETS=("local")
fi

if [[ ! -f "$INSTALL_SCRIPT" ]]; then
  fail "installer script not found: ${INSTALL_SCRIPT}"
fi

if [[ "$MODE" == "upgrade" ]]; then
  if [[ -n "$LABEL" ]]; then
    fail "--label is not used in --mode upgrade"
  fi
else
  if [[ -z "$LABEL" && "$MODE" == "fresh-clean" ]]; then
    fail "--label is required for --mode fresh-clean"
  fi
  if [[ -z "$LABEL" ]]; then
    base_label="${TAG:-latest}"
    base_label="$(sanitize_label "${base_label#v}")"
    LABEL="${base_label}-$(date +%Y%m%d-%H%M%S)"
  else
    LABEL="$(sanitize_label "$LABEL")"
  fi
fi

if [[ "$MODE" != "upgrade" ]]; then
  log "Fresh label: ${LABEL}"
fi

for target in "${TARGETS[@]}"; do
  target_kind="ssh"
  target_host="$target"
  if [[ "$target" == "local" ]]; then
    target_kind="local"
    target_host=""
  elif [[ "$target" == ssh:* ]]; then
    target_host="${target#ssh:}"
  fi

  [[ "$target_kind" == "local" || -n "$target_host" ]] || fail "invalid target: ${target}"

  log "Target: ${target}"

  if [[ "$MODE" == "fresh-clean" ]]; then
    fresh_base="${FRESH_ROOT_BASE}/${LABEL}"
    if [[ "$target_kind" == "local" ]]; then
      rm -rf "$fresh_base"
    else
      run_remote_command "$target_host" rm -rf "$fresh_base"
    fi
    log "Removed fresh test install directory: ${fresh_base}"
    continue
  fi

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

  if [[ "$MODE" == "fresh" ]]; then
    fresh_base="${FRESH_ROOT_BASE}/${LABEL}"
    fresh_bin="${fresh_base}/bin"
    fresh_workspace="${fresh_base}/workspace"
    fresh_backup="${fresh_base}/backups"
    fresh_runner_config="${fresh_workspace}/.oxydra/runner.toml"

    install_args+=(--install-dir "$fresh_bin" --base-dir "$fresh_workspace" --backup-dir "$fresh_backup")
  else
    if [[ -n "$UPGRADE_INSTALL_DIR" ]]; then
      install_args+=(--install-dir "$UPGRADE_INSTALL_DIR")
    fi
    if [[ -n "$UPGRADE_BASE_DIR" ]]; then
      install_args+=(--base-dir "$UPGRADE_BASE_DIR")
    fi
  fi

  if [[ "$target_kind" == "local" ]]; then
    "$INSTALL_SCRIPT" "${install_args[@]}"
  else
    run_remote_installer "$target_host" "${install_args[@]}"
  fi

  if [[ "$MODE" != "fresh" ]]; then
    continue
  fi

  web_cmd="$(quote_args "${fresh_bin}/runner" --config "$fresh_runner_config" web --bind "$WEB_BIND")"
  start_cmd="$(quote_args "${fresh_bin}/runner" --config "$fresh_runner_config" --user alice start)"
  cleanup_cmd="$(quote_args rm -rf "$fresh_base")"

  if [[ "$target_kind" == "local" ]]; then
    log "Fresh install path: ${fresh_base}"
    log "Start runner daemon: ${start_cmd}"
    log "Open onboarding wizard: ${web_cmd}"
    log "Discard this fresh install: ${cleanup_cmd}"
    if [[ "$START_WEB" == "true" ]]; then
      "${fresh_bin}/runner" --config "$fresh_runner_config" web --bind "$WEB_BIND"
    fi
  else
    log "Fresh install path on ${target_host}: ${fresh_base}"
    log "Start runner daemon on ${target_host}: ssh ${target_host} ${start_cmd}"
    log "Open onboarding wizard on ${target_host}: ssh ${target_host} ${web_cmd}"
    log "Discard this fresh install on ${target_host}: ssh ${target_host} ${cleanup_cmd}"
    if [[ "$START_WEB" == "true" ]]; then
      log "Use SSH port-forward in another terminal: ssh -L 9400:${WEB_BIND%:*}:${WEB_BIND##*:} ${target_host}"
      run_remote_command "$target_host" "${fresh_bin}/runner" --config "$fresh_runner_config" web --bind "$WEB_BIND"
    fi
  fi
done

log "Done."
