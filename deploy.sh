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
    return
  fi

  [ -f "$ROOT/.env.example" ] || die "Neither .env nor .env.example exists in $ROOT"
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

deploy() {
  printf 'Deploying with %s\n' "$COMPOSE_FILE"
  validate_compose
  docker compose -f "$COMPOSE_FILE" up -d --build
  docker compose -f "$COMPOSE_FILE" ps
}

update() {
  local latest_tag previous_commit

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
  previous_commit=$(git rev-parse --short HEAD)

  printf 'Updating from commit %s to tag %s\n' "$previous_commit" "$latest_tag"
  git fetch --force origin "refs/tags/$latest_tag:refs/tags/$latest_tag"
  git checkout --detach "refs/tags/$latest_tag"

  validate_compose_file
  validate_compose
  docker compose -f "$COMPOSE_FILE" pull --ignore-buildable
  docker compose -f "$COMPOSE_FILE" build --pull
  docker compose -f "$COMPOSE_FILE" up -d --remove-orphans
  docker compose -f "$COMPOSE_FILE" ps
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
