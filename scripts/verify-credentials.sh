#!/usr/bin/env bash
# =============================================================================
# Prove that each monitored database accepts the credential the daemon will use.
# =============================================================================
# The daemon does not run inside the containers' network: it runs on the host and
# connects over TCP to 127.0.0.1:<port>. That is a different path from
# `docker exec ... psql`, which reaches PostgreSQL over its unix socket and can
# succeed with credentials that fail over TCP, or on a database whose port was never
# published.
#
# This reproduces the daemon's path exactly: same host, same port, same user, same
# password from the same file.
#
# It reports four distinct outcomes, because they need different fixes:
#   SIN VARIABLE    the configuration names a variable the env file does not set
#   PUERTO CERRADO  nothing is listening: the container's port is not published
#   FALLO           the connection was made and refused, or the query failed
#   OK              connected and counted the tables in the public schema
#
# Usage: scripts/verify-credentials.sh
#   ENV_FILE  override the env file (default /opt/guardianrs/.env)
#
# Exit codes: 0 all reachable, 1 at least one problem, 2 cannot read the env file.

set -uo pipefail

ENV_FILE="${ENV_FILE:-/opt/guardianrs/.env}"

if [ ! -r "$ENV_FILE" ]; then
    printf 'error: cannot read %s\n' "$ENV_FILE" >&2
    exit 2
fi

command -v psql >/dev/null || { printf 'error: psql is not installed\n' >&2; exit 2; }

read_var() {
    grep -E "^$1=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2-
}

port_open() {
    timeout 3 bash -c "echo > /dev/tcp/$1/$2" 2>/dev/null
}

# name|host|port|user|database|password_env|legacy_env
#
# `legacy_env` is read only when `password_env` is unset, so the migration to the
# per-project names can land without leaving the auditor unverified in the meantime.
TARGETS=(
    "auditor|127.0.0.1|5442|Gallinal2218|auditor_familiar|GUARDIAN_PG_AUDITOR_PASSWORD|GUARDIAN_PG_PASSWORD"
    "pedidos|127.0.0.1|5445|barraca_user|barraca|GUARDIAN_PG_PEDIDOS_PASSWORD|"
    "ecommerce|127.0.0.1|5446|web2r_prod_user|web_2r_prod|GUARDIAN_PG_ECOMMERCE_PASSWORD|"
)

# Counting tables rather than selecting 1: it proves catalog access and answers
# whether the connection landed on the database anyone meant.
QUERY="select count(*) from information_schema.tables where table_schema='public'"

failed=0

for target in "${TARGETS[@]}"; do
    IFS='|' read -r name host port user database password_env legacy_env <<< "$target"

    password="$(read_var "$password_env")"
    used="$password_env"

    if [ -z "$password" ] && [ -n "$legacy_env" ]; then
        password="$(read_var "$legacy_env")"
        [ -n "$password" ] && used="$legacy_env (nombre anterior)"
    fi

    if [ -z "$password" ]; then
        printf '%-11s SIN VARIABLE   %s\n' "$name" "$password_env"
        failed=1
        continue
    fi

    if ! port_open "$host" "$port"; then
        printf '%-11s PUERTO CERRADO %s:%s\n' "$name" "$host" "$port"
        failed=1
        continue
    fi

    # -w never prompts: a wrong password must fail, not wait for a human.
    output="$(PGCONNECT_TIMEOUT=5 PGPASSWORD="$password" timeout 15 \
        psql -w -h "$host" -p "$port" -U "$user" -d "$database" -tAc "$QUERY" 2>&1)"
    status=$?

    if [ "$status" -eq 0 ] && [ -n "$output" ]; then
        printf '%-11s OK             %s tablas en %s via %s\n' \
            "$name" "$output" "$database" "$used"
    else
        printf '%-11s FALLO          %s\n' \
            "$name" "$(printf '%s' "$output" | head -2 | tr '\n' ' ')"
        failed=1
    fi
done

exit "$failed"
