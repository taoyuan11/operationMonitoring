#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./deploy.sh deploy <compose-file>
  ./deploy.sh update <compose-file>
  ./deploy.sh --help

Examples:
  ./deploy.sh deploy docker-compose.with-db.yml
  ./deploy.sh deploy docker-compose.yml
  ./deploy.sh update docker-compose.with-db.yml
  ./deploy.sh update docker-compose.yml

The compose file may be an absolute path or a path relative to the project root.

Environment:
  OM_DEPLOY_BACKEND_TIMEOUT_SECONDS
    Maximum time to wait for database migrations and backend health (default: 1800).
  OM_COMPOSE_BUILD_PARALLELISM
    Maximum number of Compose services built at once (default: 1).
USAGE
}

die() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

validate_compose_file() {
  [ -f "$COMPOSE_FILE" ] || die "Compose file not found: $COMPOSE_FILE"
}

ensure_environment_file() {
  if [ -f "$ROOT/.env" ]; then
    chmod 600 "$ROOT/.env"
    return
  fi

  [ -f "$ROOT/.env.example" ] || die "Neither .env nor .env.example exists in $ROOT"
  umask 077
  cp "$ROOT/.env.example" "$ROOT/.env"
  chmod 600 "$ROOT/.env"
  printf '%s\n' \
    'Created .env from .env.example with permissions 600.' \
    'Edit .env and replace the example passwords and connection settings, then run this command again.'
  exit 1
}

semver_is_greater() {
  local left=$1
  local right=$2
  local left_major left_minor left_patch
  local right_major right_minor right_patch

  IFS=. read -r left_major left_minor left_patch <<< "$left"
  IFS=. read -r right_major right_minor right_patch <<< "$right"

  if ((10#$left_major != 10#$right_major)); then
    ((10#$left_major > 10#$right_major))
  elif ((10#$left_minor != 10#$right_minor)); then
    ((10#$left_minor > 10#$right_minor))
  else
    ((10#$left_patch > 10#$right_patch))
  fi
}

backend_version_from_file() {
  local version
  version=$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"[[:space:]]*$/\1/p' "$1")
  [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die 'Unable to determine a stable backend version from backend/Cargo.toml'
  printf '%s\n' "$version"
}

backend_version() {
  backend_version_from_file "$ROOT/backend/Cargo.toml"
}

backend_version_at_tag() {
  local tag=$1 manifest version
  manifest=$(git show "refs/tags/$tag:backend/Cargo.toml") \
    || die "Unable to read backend/Cargo.toml from tag $tag"
  version=$(printf '%s\n' "$manifest" | sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"[[:space:]]*$/\1/p')
  [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die "Unable to determine a stable backend version from tag $tag"
  printf '%s\n' "$version"
}

require_forward_backend_upgrade() {
  local current_version=$1
  local target_version=$2
  if ! semver_is_greater "$target_version" "$current_version"; then
    die "Refusing non-forward backend update from $current_version to $target_version; backend version rollback is unsupported."
  fi
}

latest_remote_tag() {
  local remote_tags ref tag latest=

  if ! remote_tags=$(git ls-remote --tags --refs origin); then
    die 'Unable to query tags from the origin remote'
  fi

  while IFS=$'\t' read -r _ ref; do
    tag=${ref#refs/tags/}
    if [[ $tag =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
      && { [ -z "$latest" ] || semver_is_greater "$tag" "$latest"; }; then
      latest=$tag
    fi
  done <<< "$remote_tags"

  [ -n "$latest" ] || die 'No stable SemVer tags (for example, 1.2.3) found on origin'
  printf '%s\n' "$latest"
}

validate_compose() {
  docker compose -f "$COMPOSE_FILE" config --quiet
}

backend_start_timeout_seconds() {
  local timeout=${OM_DEPLOY_BACKEND_TIMEOUT_SECONDS:-1800}
  [[ $timeout =~ ^[1-9][0-9]*$ ]] && ((10#$timeout >= 30)) \
    || die 'OM_DEPLOY_BACKEND_TIMEOUT_SECONDS must be an integer of at least 30 seconds'
  printf '%s\n' "$timeout"
}

compose_build_parallelism() {
  local parallelism=${OM_COMPOSE_BUILD_PARALLELISM:-1}
  [[ $parallelism =~ ^[1-9][0-9]*$ ]] \
    || die 'OM_COMPOSE_BUILD_PARALLELISM must be a positive integer'
  printf '%s\n' "$parallelism"
}

compose_has_service() {
  local expected=$1 service
  while IFS= read -r service; do
    [ "$service" = "$expected" ] && return 0
  done < <(docker compose -f "$COMPOSE_FILE" config --services)
  return 1
}

print_backend_diagnostics() {
  local container_id

  printf '\nBackend startup diagnostics:\n' >&2
  docker compose -f "$COMPOSE_FILE" ps --all >&2 || true
  docker compose -f "$COMPOSE_FILE" logs --no-color --tail=200 backend >&2 || true

  container_id=$(docker compose -f "$COMPOSE_FILE" ps --all -q backend 2>/dev/null || true)
  if [ -n "$container_id" ]; then
    docker inspect --format \
      '{{.State.Status}} restart={{.RestartCount}} health={{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
      "$container_id" >&2 || true
    docker inspect --format \
      '{{range .State.Health.Log}}{{println .End "exit=" .ExitCode .Output}}{{end}}' \
      "$container_id" >&2 || true
  fi

  if compose_has_service postgres; then
    printf '\nPostgreSQL startup diagnostics:\n' >&2
    docker compose -f "$COMPOSE_FILE" logs --no-color --tail=100 postgres >&2 || true
  fi
}

wait_for_backend_health() {
  local timeout start now last_report container_id inspection runtime health restart_count initial_restart_count
  timeout=$(backend_start_timeout_seconds)

  container_id=''
  for _ in {1..30}; do
    container_id=$(docker compose -f "$COMPOSE_FILE" ps --all -q backend 2>/dev/null || true)
    [ -n "$container_id" ] && break
    sleep 1
  done
  if [ -z "$container_id" ]; then
    printf '%s\n' 'Backend container was not created.' >&2
    return 1
  fi

  initial_restart_count=$(docker inspect --format '{{.RestartCount}}' "$container_id" 2>/dev/null || printf '0')
  [[ $initial_restart_count =~ ^[0-9]+$ ]] || initial_restart_count=0

  start=$(date +%s)
  last_report=$start
  printf 'Waiting up to %ss for backend health checks and database migrations...\n' "$timeout"
  while :; do
    inspection=$(docker inspect --format \
      '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}} {{.RestartCount}}' \
      "$container_id" 2>/dev/null) || {
        printf '%s\n' 'Backend container disappeared while waiting for health.' >&2
        return 1
      }
    IFS=' ' read -r runtime health restart_count <<< "$inspection"

    if [ "$health" = healthy ]; then
      printf 'Backend is healthy.\n'
      return 0
    fi
    case "$runtime" in
      exited|dead|removing|restarting|paused)
        printf 'Backend container entered state %s before becoming healthy.\n' "$runtime" >&2
        return 1
        ;;
    esac
    if [[ $restart_count =~ ^[0-9]+$ ]] && ((restart_count > initial_restart_count)); then
      printf 'Backend restarted during startup (restart count: %s -> %s).\n' \
        "$initial_restart_count" "$restart_count" >&2
      return 1
    fi

    now=$(date +%s)
    if ((now - start >= timeout)); then
      printf 'Backend did not become healthy within %ss (last health state: %s).\n' \
        "$timeout" "$health" >&2
      return 1
    fi
    if ((now - last_report >= 30)); then
      printf 'Still waiting for backend (%ss elapsed, health: %s)...\n' \
        "$((now - start))" "$health"
      last_report=$now
    fi
    sleep 5
  done
}

start_stack() {
  backend_start_timeout_seconds >/dev/null
  if ! docker compose -f "$COMPOSE_FILE" up -d --remove-orphans backend; then
    print_backend_diagnostics
    die 'Backend could not be started.'
  fi
  if ! wait_for_backend_health; then
    print_backend_diagnostics
    die 'Backend did not become healthy; the frontend was not started.'
  fi
  if ! docker compose -f "$COMPOSE_FILE" up -d --remove-orphans; then
    print_backend_diagnostics
    die 'The remaining services could not be started.'
  fi
  docker compose -f "$COMPOSE_FILE" ps
}

deploy() {
  printf 'Deploying with %s\n' "$COMPOSE_FILE"
  validate_compose
  docker compose --parallel "$(compose_build_parallelism)" -f "$COMPOSE_FILE" build
  start_stack
}

update() {
  local latest_tag previous_commit current_version target_backend_version

  require_command git
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || die "$ROOT is not a Git working tree"
  git remote get-url origin >/dev/null 2>&1 \
    || die 'Git remote "origin" is not configured'

  if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    printf '%s\n' 'Error: The Git working tree has local changes. Commit, stash, or remove them before updating.' >&2
    git status --short >&2
    exit 1
  fi

  printf '%s\n' 'Reminder: back up PostgreSQL and persistent volumes before updating.'
  latest_tag=$(latest_remote_tag)
  current_version=$(backend_version)
  previous_commit=$(git rev-parse --short HEAD)

  git fetch --force origin "refs/tags/$latest_tag:refs/tags/$latest_tag"
  target_backend_version=$(backend_version_at_tag "$latest_tag")
  require_forward_backend_upgrade "$current_version" "$target_backend_version"

  printf 'Updating backend from %s to %s (tag %s, commit %s)\n' \
    "$current_version" "$target_backend_version" "$latest_tag" "$previous_commit"
  git checkout --detach "refs/tags/$latest_tag"

  validate_compose_file
  validate_compose
  docker compose -f "$COMPOSE_FILE" pull --ignore-buildable
  docker compose --parallel "$(compose_build_parallelism)" -f "$COMPOSE_FILE" build --pull
  start_stack
}

if [ "$#" -eq 1 ] && { [ "$1" = --help ] || [ "$1" = -h ]; }; then
  usage
  exit 0
fi

if [ "$#" -ne 2 ]; then
  usage >&2
  exit 2
fi

ACTION=$1
COMPOSE_ARGUMENT=$2

case "$ACTION" in
  deploy|update) ;;
  *)
    printf 'Error: unsupported action: %s\n' "$ACTION" >&2
    usage >&2
    exit 2
    ;;
esac

ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$ROOT"

case "$COMPOSE_ARGUMENT" in
  /*) COMPOSE_FILE=$COMPOSE_ARGUMENT ;;
  *) COMPOSE_FILE=$ROOT/$COMPOSE_ARGUMENT ;;
esac

require_command docker
docker compose version >/dev/null 2>&1 \
  || die 'Docker Compose v2 is required (the "docker compose" command)'
validate_compose_file
ensure_environment_file

case "$ACTION" in
  deploy) deploy ;;
  update) update ;;
esac
