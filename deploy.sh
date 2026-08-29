#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SITE_DIR="$ROOT_DIR/crumb.elixpo"
WRANGLER_CONFIG="$SITE_DIR/wrangler.toml"
WORKER_NAME="crumb-elixpo"
D1_DATABASE="crumb-elixpo"
KV_TITLE="crumb-elixpo-KV"
DRY_RUN=false

log() { printf '\033[32m▸\033[0m %s\n' "$1"; }
fail() { printf '\033[31m✗\033[0m %s\n' "$1" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Usage: ./deploy.sh COMMAND [--dry-run]

Commands:
  provision   Create the D1 database and KV namespace if absent
  secrets     Upload application secrets from the SOPS-encrypted root .env
  migrate     Apply D1 migrations remotely
  build       Install dependencies and build the Cloudflare Pages bundle
  deploy      Build and deploy the OpenNext Worker
  all         Provision, migrate, deploy, and upload secrets

The first OpenNext deployment creates the Worker service automatically.
USAGE
}

run() {
  if $DRY_RUN; then
    printf '[dry-run]'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

run_site() {
  if $DRY_RUN; then
    printf '[dry-run] cd %q &&' "$SITE_DIR"
    printf ' %q' "$@"
    printf '\n'
  else
    (cd "$SITE_DIR" && "$@")
  fi
}

require_tools() {
  if $DRY_RUN; then
    [ -f "$WRANGLER_CONFIG" ] || fail "Missing $WRANGLER_CONFIG"
    return
  fi
  command -v node >/dev/null 2>&1 || fail "Node.js is required."
  command -v npm >/dev/null 2>&1 || fail "npm is required."
  [ -f "$WRANGLER_CONFIG" ] || fail "Missing $WRANGLER_CONFIG"
}

load_cloudflare_auth() {
  if [ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [[ "${CLOUDFLARE_API_TOKEN}" != ENC\[* ]]; then
    return
  fi
  command -v sops >/dev/null 2>&1 || fail "sops is required to decrypt the root .env."
  [ -f "$ROOT_DIR/.env" ] || fail "Missing SOPS-encrypted root .env."
  local decrypted token="" account_id=""
  decrypted="$(sops decrypt "$ROOT_DIR/.env")"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      CLOUDFLARE_API_TOKEN=*) token="${line#*=}" ;;
      CLOUDFLARE_ACCOUNT_ID=*) account_id="${line#*=}" ;;
    esac
  done <<< "$decrypted"
  [ -n "$token" ] || fail "CLOUDFLARE_API_TOKEN is missing from the decrypted .env."
  export CLOUDFLARE_API_TOKEN="$token"
  if [ -n "$account_id" ]; then export CLOUDFLARE_ACCOUNT_ID="$account_id"; fi
}

json_match() {
  local field="$1" value="$2" result="$3"
  node -e '
    let input = "";
    process.stdin.on("data", chunk => input += chunk);
    process.stdin.on("end", () => {
      const parsed = JSON.parse(input);
      const rows = Array.isArray(parsed) ? parsed : parsed.result || [];
      const row = rows.find(item => item[process.argv[1]] === process.argv[2]);
      if (row?.[process.argv[3]]) process.stdout.write(String(row[process.argv[3]]));
    });
  ' "$field" "$value" "$result"
}

d1_id() {
  run_site npx wrangler d1 list --json --config "$WRANGLER_CONFIG" | json_match name "$D1_DATABASE" uuid
}

kv_id() {
  run_site npx wrangler kv namespace list --config "$WRANGLER_CONFIG" | json_match title "$KV_TITLE" id
}

replace_resource_ids() {
  local database_id="$1" namespace_id="$2"
  DATABASE_ID="$database_id" NAMESPACE_ID="$namespace_id" CONFIG_PATH="$WRANGLER_CONFIG" node -e '
    const fs = require("node:fs");
    const path = process.env.CONFIG_PATH;
    let value = fs.readFileSync(path, "utf8");
    value = value.replace(/database_id = "[^"]+"/, `database_id = "${process.env.DATABASE_ID}"`);
    value = value.replace(
      /(\[\[kv_namespaces\]\]\s*binding = "KV"\s*)id = "[^"]+"/,
      `$1id = "${process.env.NAMESPACE_ID}"`,
    );
    fs.writeFileSync(path, value);
  '
}

provision() {
  require_tools
  if $DRY_RUN; then
    run_site npx wrangler d1 create "$D1_DATABASE" --location apac
    run_site npx wrangler kv namespace create "$KV_TITLE" --config "$WRANGLER_CONFIG"
    return
  fi
  load_cloudflare_auth

  local database_id namespace_id
  database_id="$(d1_id)"
  if [ -z "$database_id" ]; then
    log "Creating D1 database $D1_DATABASE..."
    run_site npx wrangler d1 create "$D1_DATABASE" --location apac
    database_id="$(d1_id)"
  fi
  [ -n "$database_id" ] || fail "Could not resolve the D1 database ID."

  namespace_id="$(kv_id)"
  if [ -z "$namespace_id" ]; then
    log "Creating KV namespace $KV_TITLE..."
    run_site npx wrangler kv namespace create "$KV_TITLE" --config "$WRANGLER_CONFIG"
    namespace_id="$(kv_id)"
  fi
  [ -n "$namespace_id" ] || fail "Could not resolve the KV namespace ID."

  replace_resource_ids "$database_id" "$namespace_id"
  log "Cloudflare resources are ready and wrangler.toml is bound."
}

upload_secrets() {
  require_tools
  if $DRY_RUN; then
    for key in NEXT_PUBLIC_ELIXPO_CLIENT_ID NEXT_PUBLIC_ELIXPO_CLIENT_ID_CLI ELIXPO_CLIENT_SECRET ELIXPO_ACCOUNTS_WEBHOOK_SECRET POLLINATIONS_APP_KEY CONNECTOR_ENCRYPTION_KEY; do
      printf '[dry-run] sops decrypt .env | npx wrangler secret put %q --name %q\n' "$key" "$WORKER_NAME"
    done
    return
  fi
  load_cloudflare_auth
  local decrypted local_values="" key value
  decrypted="$(sops decrypt "$ROOT_DIR/.env")"
  if [ -f "$SITE_DIR/.env.local" ]; then
    local_values="$(<"$SITE_DIR/.env.local")"
  fi
  for key in NEXT_PUBLIC_ELIXPO_CLIENT_ID NEXT_PUBLIC_ELIXPO_CLIENT_ID_CLI ELIXPO_CLIENT_SECRET ELIXPO_ACCOUNTS_WEBHOOK_SECRET POLLINATIONS_APP_KEY CONNECTOR_ENCRYPTION_KEY; do
    value=""
    while IFS= read -r line || [ -n "$line" ]; do
      if [[ "$line" == "$key="* ]]; then value="${line#*=}"; break; fi
    done <<< "$local_values"
    while IFS= read -r line || [ -n "$line" ]; do
      if [ -z "$value" ] && [[ "$line" == "$key="* ]]; then value="${line#*=}"; break; fi
    done <<< "$decrypted"
    [ -n "$value" ] || fail "$key is missing from the decrypted .env."
    printf '%s' "$value" | run_site npx wrangler secret put "$key" \
      --name "$WORKER_NAME" --config "$WRANGLER_CONFIG" >/dev/null
  done
  log "Application secrets uploaded."
}

migrate() {
  require_tools
  if ! $DRY_RUN; then load_cloudflare_auth; fi
  run_site npx wrangler d1 migrations apply "$D1_DATABASE" \
    --remote --config "$WRANGLER_CONFIG"
}

build() {
  require_tools
  if [ -f "$SITE_DIR/package-lock.json" ]; then
    run_site npm ci
  else
    run_site npm install
  fi
  run_site npm run cloudflare:build
}

deploy() {
  require_tools
  if ! $DRY_RUN; then load_cloudflare_auth; fi
  run_site npm run deploy
}

[ "${1:-}" ] || { usage; exit 1; }
command="$1"
shift
if [ "${1:-}" = "--dry-run" ]; then DRY_RUN=true; shift; fi
[ $# -eq 0 ] || fail "Unexpected argument: $1"

case "$command" in
  provision) provision ;;
  secrets) upload_secrets ;;
  migrate) migrate ;;
  build) build ;;
  deploy) deploy ;;
  all) provision; migrate; deploy; upload_secrets ;;
  -h|--help|help) usage ;;
  *) fail "Unknown command: $command" ;;
esac
