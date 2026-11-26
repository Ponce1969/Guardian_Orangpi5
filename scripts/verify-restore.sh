#!/usr/bin/env bash
# =============================================================================
# Prove that a backup can actually be restored.
# =============================================================================
# A dump that passes `pg_restore --list` is proven readable, not proven
# restorable. The difference only shows up the day it is needed, which is the
# worst possible moment to discover it, so this exercises the full path: create a
# throwaway database, restore the newest dump into it, count real rows, drop it.
#
# It never touches the production database. The target name is generated per run
# and checked against a protected name before anything is created.
#
# The dump is mounted into a one-off container rather than read from the host,
# because the backup directory is 0700 and owned by the service user. That is a
# deliberate reminder: membership of the docker group is root-equivalent, since
# the daemon does the mount as root.
#
# Usage:
#   scripts/verify-restore.sh                  # newest dump, auto-detected
#   scripts/verify-restore.sh /path/to.dump    # a specific one
#
# Exit codes: 0 restored with rows, 1 failed, 2 refused to run.

set -euo pipefail

CONTAINER="${CONTAINER:-auditor_familiar_db}"
PROTECTED_DB="${PROTECTED_DB:-auditor_familiar}"
PASSWORD_ENV="${PASSWORD_ENV:-GUARDIAN_PG_AUDITOR_PASSWORD}"
ENV_FILE="${ENV_FILE:-/opt/guardianrs/.env}"
DUMP_DIR="${DUMP_DIR:-/var/backups/guardianrs/postgres}"

die() { printf 'error: %s\n' "$1" >&2; exit 1; }
say() { printf '\n== %s\n' "$1"; }

[ -r "$ENV_FILE" ] || die "cannot read $ENV_FILE (needs the PostgreSQL password)"
command -v docker >/dev/null || die "docker is not available"

strip_outer_quotes() {
    local val="$1"
    if [[ ${#val} -ge 2 ]]; then
        if [[ "${val:0:1}" == '"' && "${val: -1}" == '"' ]]; then
            val="${val:1:${#val}-2}"
        elif [[ "${val:0:1}" == "'" && "${val: -1}" == "'" ]]; then
            val="${val:1:${#val}-2}"
        fi
    fi
    printf '%s' "$val"
}

read_env_var() {
    local raw
    raw="$(grep -E "^$1=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2- | tr -d '\r' || true)"
    strip_outer_quotes "$raw"
}

PGPASSWORD="$(read_env_var "$PASSWORD_ENV")"
if [ -z "$PGPASSWORD" ] && [ "$PASSWORD_ENV" = "GUARDIAN_PG_AUDITOR_PASSWORD" ]; then
    PGPASSWORD="$(read_env_var "GUARDIAN_PG_PASSWORD")"
fi
[ -n "$PGPASSWORD" ] || die "$PASSWORD_ENV is empty in $ENV_FILE"

IMAGE="$(docker inspect "$CONTAINER" --format '{{.Config.Image}}')"
NETWORK="$(docker inspect "$CONTAINER" --format '{{range $name, $_ := .NetworkSettings.Networks}}{{println $name}}{{end}}' | head -1)"
if [ -z "${PG_USER:-}" ]; then
    PG_USER="$(docker inspect "$CONTAINER" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n 's/^POSTGRES_USER=//p' | head -1)"
    PG_USER="${PG_USER:-postgres}"
fi

[ -n "$IMAGE" ] || die "cannot read the image of $CONTAINER"
[ -n "$NETWORK" ] || die "cannot read the network of $CONTAINER"

# --- Which dump -------------------------------------------------------------
DUMP="${1:-}"

if [ -z "$DUMP" ]; then
    say "Locating the newest dump"
    case "$DUMP_DIR" in
        /*) ;;
        *) DUMP_DIR="$(pwd -P)/$DUMP_DIR" ;;
    esac
    DUMP="$(docker run --rm -e DUMP_DIR="$DUMP_DIR" -e PROTECTED_DB="$PROTECTED_DB" -v "$DUMP_DIR:$DUMP_DIR:ro" --entrypoint sh "$IMAGE" \
        -c 'test -d "$DUMP_DIR" && { ls -1t "$DUMP_DIR/guardianrs-${PROTECTED_DB}-"*.dump 2>/dev/null | head -1 || true; }')" \
        || die "cannot list $DUMP_DIR"
    [ -n "$DUMP" ] || die "no guardianrs-${PROTECTED_DB}-*.dump file in $DUMP_DIR"
else
    case "$DUMP" in
        /*) ;;
        *) DUMP="$(pwd -P)/$DUMP" ;;
    esac
    if command -v realpath >/dev/null 2>&1; then
        DUMP="$(realpath -m "$DUMP" 2>/dev/null || printf '%s' "$DUMP")"
    fi
fi

DUMP_DIR_ARG="$(dirname "$DUMP")"
[ -n "$DUMP_DIR_ARG" ] && [[ "$DUMP_DIR_ARG" == /* ]] || die "invalid dump directory for $DUMP"

[ -f "$DUMP" ] && [ -s "$DUMP" ] || [ -n "$(docker run --rm -e DUMP_PATH="$DUMP" -v "$DUMP_DIR_ARG:$DUMP_DIR_ARG:ro" --entrypoint sh "$IMAGE" \
    -c 'test -f "$DUMP_PATH" && test -s "$DUMP_PATH" && echo present' 2>/dev/null)" ] \
    || die "$DUMP does not exist or is empty"

say "Dump: $DUMP"
docker run --rm -e DUMP_PATH="$DUMP" -v "$DUMP_DIR_ARG:$DUMP_DIR_ARG:ro" --entrypoint sh "$IMAGE" \
    -c 'ls -l "$DUMP_PATH"' || true

# --- Target -----------------------------------------------------------------
TEST_DB="auditor_restore_test_$(date +%Y%m%d%H%M%S)"

# The one mistake this script must make impossible.
[ "$TEST_DB" != "$PROTECTED_DB" ] || { echo "refusing to restore into $PROTECTED_DB" >&2; exit 2; }

psql_as() {
    local database="$1"; shift
    PGPASSWORD="$PGPASSWORD" docker exec -i -e PGPASSWORD "$CONTAINER" \
        psql -U "$PG_USER" -d "$database" -v ON_ERROR_STOP=1 "$@"
}

CREATED=0
cleanup() {
    local status=$?
    trap - EXIT

    if [ "$CREATED" = "1" ]; then
        say "Dropping $TEST_DB"
        # FORCE needs PostgreSQL 13+; a leftover test database is not worth
        # failing the whole check over.
        psql_as postgres -c "DROP DATABASE IF EXISTS \"$TEST_DB\" WITH (FORCE)" >/dev/null 2>&1 \
            || psql_as postgres -c "DROP DATABASE IF EXISTS \"$TEST_DB\"" >/dev/null 2>&1 \
            || printf 'warning: could not drop %s; drop it by hand\n' "$TEST_DB" >&2
    fi

    exit "$status"
}
trap cleanup EXIT

say "Creating $TEST_DB"
psql_as postgres -c "CREATE DATABASE \"$TEST_DB\"" >/dev/null
CREATED=1

# --- Restore ----------------------------------------------------------------
say "Restoring"
# --no-owner and --no-privileges: the roles the dump names may not exist in a
# fresh database, and the question here is whether the data comes back, not
# whether the ownership graph does.
if PGPASSWORD="$PGPASSWORD" docker run --rm --network "$NETWORK" \
    -v "$DUMP_DIR_ARG:$DUMP_DIR_ARG:ro" -e PGPASSWORD --entrypoint pg_restore "$IMAGE" \
    -h "$CONTAINER" -U "$PG_USER" -d "$TEST_DB" \
    --no-owner --no-privileges "$DUMP"; then
    echo "pg_restore exited 0"
else
    echo "pg_restore reported errors (see above)" >&2
    exit 1
fi

# --- Control ----------------------------------------------------------------
say "Counting rows in the largest tables"
# Exact counts rather than pg_stat_user_tables: statistics are not collected for
# a database that was created a second ago, and an empty report would look like a
# successful restore of an empty dump.
ROWS="$(psql_as "$TEST_DB" -tA -c "
    SELECT relname, (xpath('/row/cnt/text()', query_to_xml(
               format('SELECT count(*) AS cnt FROM %I.%I', schemaname, relname),
               false, true, '')))[1]::text::bigint
    FROM pg_stat_user_tables
    ORDER BY 2 DESC
    LIMIT 10;")"

printf '%s\n' "$ROWS"

TOTAL="$(printf '%s\n' "$ROWS" | awk -F'|' '{sum += $2} END {print sum + 0}')"
TABLES="$(printf '%s\n' "$ROWS" | grep -c '|' || true)"

say "Result: $TABLES tables, $TOTAL rows"

# An empty restore exits 0. Only the row count distinguishes "restored the data"
# from "recreated the schema".
[ "$TOTAL" -gt 0 ] || die "the restored database is empty: the dump has no rows"

echo "restore verified"
