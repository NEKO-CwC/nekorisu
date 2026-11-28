#!/bin/sh
# Starts a throwaway nekorisu-server (test token, test administrator password, empty data),
# runs a command with NEKORISU_E2E_SERVER* set so the @server scenarios use it, then stops it.
#
#   tests/e2e/server.sh                          # cargo test --test e2e
#   tests/e2e/server.sh <command> [args...]      # anything else, e.g. a test program copied elsewhere
#
# NEKORISU_E2E_SERVER_IMAGE: the server's container image (default nekorisu-server).
# NEKORISU_E2E_SERVER_PORTS: "<http> <grpc>" on 127.0.0.1 (default "18080 17070").
# NEKORISU_E2E_SERVER_KEEP=1: leave the server running and print what to set to use it.
set -eu

image=${NEKORISU_E2E_SERVER_IMAGE:-nekorisu-server}
set -- ${NEKORISU_E2E_SERVER_PORTS:-18080 17070} "$@"
http_port=$1 grpc_port=$2
shift 2
[ $# -gt 0 ] || set -- cargo test --test e2e

random() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }
token=$(random)
password=test-$(random)
data=$(mktemp -d)
config=$(mktemp -d)
chmod 777 "$data" "$config" # the image runs as an unprivileged user

name=nekorisu-e2e-$(random | cut -c1-8)
docker run -d --rm --name "$name" \
  -p "127.0.0.1:$http_port:8080" -p "127.0.0.1:$grpc_port:7070" \
  -e NEKORISU_DEVICE_TOKEN="$token" \
  -e NEKORISU_PUBLIC_URL="http://127.0.0.1:$http_port" \
  -e NEKORISU_OAUTH_CLIENT_METADATA_DOCUMENTS=false \
  -v "$data:/data" -v "$config:/config" \
  "$image" >/dev/null

stop() {
  docker stop "$name" >/dev/null 2>&1 || true
  rm -rf "$data" "$config"
}
if [ "${NEKORISU_E2E_SERVER_KEEP:-}" != 1 ]; then trap stop EXIT; fi

# The first start prints a one-time setup code; it sets the administrator password.
code=
for _ in $(seq 1 100); do
  code=$(docker logs "$name" 2>&1 | sed -n 's/.*Setup code: \([A-Za-z0-9-]*\).*/\1/p' | tail -1)
  [ -n "$code" ] && break
  sleep 0.2
done
[ -n "$code" ] || { docker logs "$name"; echo "the server did not start" >&2; exit 1; }
curl -fsS -o /dev/null -H 'content-type: application/json' \
  -d "{\"password\":\"$password\",\"setup_code\":\"$code\"}" "http://127.0.0.1:$http_port/api/auth/setup"

export NEKORISU_E2E_SERVER="http://127.0.0.1:$http_port"
export NEKORISU_E2E_SERVER_GRPC="http://127.0.0.1:$grpc_port"
export NEKORISU_E2E_SERVER_TOKEN="$token"
export NEKORISU_E2E_SERVER_PASSWORD="$password"
if [ "${NEKORISU_E2E_SERVER_KEEP:-}" = 1 ]; then
  echo "# server $name running; to stop it: docker stop $name && rm -rf $data $config"
  for var in NEKORISU_E2E_SERVER NEKORISU_E2E_SERVER_GRPC NEKORISU_E2E_SERVER_TOKEN NEKORISU_E2E_SERVER_PASSWORD; do
    eval "echo export $var=\$$var"
  done
  exit 0
fi
"$@"
