#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Runs demo-k8s-controller.sh against a gateway built from this checkout,
# with the controller's identity listed as the workspace manager of the demo
# workspace. A second identity checks that it can't change the controller's
# objects. Everything, including the CLI's configuration, lives in one
# temporary directory, and the gateway runs on its own port.
#
# Environment:
#   MANAGED_DEMO_PORT  gateway port (default: 18292)
#   Other DEMO_* variables pass through to demo-k8s-controller.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT="${MANAGED_DEMO_PORT:-18292}"
GW_NAME=managed-demo
GATEWAY_BIN="${ROOT}/target/debug/openshell-gateway"
CLI_BIN="${ROOT}/target/debug/openshell"
SUPERVISOR_IMAGE="${OPENSHELL_SUPERVISOR_IMAGE:-ghcr.io/nvidia/openshell/supervisor:0.1.6}"
SANDBOX_RUNTIME_IMAGE="${OPENSHELL_SANDBOX_RUNTIME_IMAGE:-ghcr.io/nvidia/openshell/sandbox:0.1.6}"

fail() { printf '✘ %s\n' "$*" >&2; exit 1; }
for command in cargo docker openssl lsof; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
docker info >/dev/null 2>&1 || fail "docker daemon is not reachable"
if lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port ${PORT} is in use; set MANAGED_DEMO_PORT"
fi

mkdir -p "${ROOT}/target"
STATE="$(mktemp -d "${ROOT}/target/managed-demo.XXXXXX")"
gateway_pid=
cleanup() {
  result=$?
  if [ -n "${gateway_pid}" ]; then
    kill "${gateway_pid}" 2>/dev/null || true
    wait "${gateway_pid}" 2>/dev/null || true
  fi
  if [ "${result}" -ne 0 ] && [ -f "${STATE}/gateway.log" ]; then
    echo "--- gateway log (last 40 lines) ---" >&2
    tail -40 "${STATE}/gateway.log" >&2
  fi
  rm -rf "${STATE}"
  exit "${result}"
}
trap cleanup EXIT

issue_client_cert() {
  local who=$1 subject=$2
  local dir="${STATE}/cli-${who}/openshell/gateways/${GW_NAME}"
  mkdir -p "${dir}/mtls"
  openssl ecparam -name prime256v1 -genkey -noout -out "${dir}/mtls/tls.key" 2>/dev/null
  openssl req -new -key "${dir}/mtls/tls.key" -subj "${subject}" -out "${STATE}/${who}.csr" 2>/dev/null
  printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n' \
    >"${STATE}/client.ext"
  openssl x509 -req -in "${STATE}/${who}.csr" -CA "${STATE}/pki/ca.crt" -CAkey "${STATE}/pki/ca.key" \
    -CAcreateserial -days 1 -extfile "${STATE}/client.ext" -out "${dir}/mtls/tls.crt" 2>/dev/null
  cp "${STATE}/pki/ca.crt" "${dir}/mtls/ca.crt"
  cat >"${dir}/metadata.json" <<JSON
{"name": "${GW_NAME}", "gateway_endpoint": "https://127.0.0.1:${PORT}", "is_remote": false, "gateway_port": ${PORT}, "auth_mode": "mtls"}
JSON
}

cargo build -q -p openshell-gateway --bin openshell-gateway -p openshell-cli \
  --manifest-path "${ROOT}/Cargo.toml"
# generate-certs also copies client certificates into the CLI's configuration
# directory; isolate it so the user's real CLI configuration is never touched.
XDG_CONFIG_HOME="${STATE}/certgen-config" "${GATEWAY_BIN}" generate-certs --output-dir "${STATE}/pki" \
  --server-san 127.0.0.1 --server-san localhost --server-san host.docker.internal >/dev/null
issue_client_cert controller "/CN=openshell-k8s-controller/OU=openshell-user"
issue_client_cert other "/CN=openshell-cli/OU=openshell-user"

cat >"${STATE}/gateway.toml" <<TOML
[openshell]
version = 2

[openshell.gateway]
name = "${GW_NAME}"
compute_driver = "docker"
guest_tls_ca = "${STATE}/pki/ca.crt"

[openshell.gateway.workspace_managers]
agents = ["mtls:openshell-k8s-controller"]

[openshell.gateway.gateway_jwt]
signing_key_path = "${STATE}/pki/jwt/signing.pem"
public_key_path = "${STATE}/pki/jwt/public.pem"
kid_path = "${STATE}/pki/jwt/kid"
gateway_id = "${GW_NAME}"

[openshell.drivers.docker]
# Sandboxes reach the gateway on the host through Docker's host alias.
grpc_endpoint = "https://host.docker.internal:${PORT}"
supervisor_image = "${SUPERVISOR_IMAGE}"
sandbox_runtime_image = "${SANDBOX_RUNTIME_IMAGE}"
image_pull_policy = "if_not_present"
sandbox_label = "${GW_NAME}"
app_armor_profile = "Unconfined"
TOML

"${GATEWAY_BIN}" --config "${STATE}/gateway.toml" --port "${PORT}" --log-level info \
  --compute-driver docker \
  --tls-cert "${STATE}/pki/server/tls.crt" --tls-key "${STATE}/pki/server/tls.key" \
  --tls-client-ca "${STATE}/pki/ca.crt" --enable-mtls-auth true \
  --db-url "sqlite:${STATE}/gateway.db?mode=rwc" >"${STATE}/gateway.log" 2>&1 &
gateway_pid=$!
for _ in $(seq 1 60); do
  kill -0 "${gateway_pid}" 2>/dev/null || fail "gateway exited during startup"
  lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1 && break
  sleep 1
done
printf '✔ gateway from this checkout on 127.0.0.1:%s; workspace agents is managed by the controller\n' "${PORT}"

XDG_CONFIG_HOME="${STATE}/cli-controller" \
  DEMO_GATEWAY="${GW_NAME}" \
  DEMO_GATEWAY_URL="https://host.docker.internal:${PORT}" \
  DEMO_MTLS_DIR="${STATE}/cli-controller/openshell/gateways/${GW_NAME}/mtls" \
  DEMO_OTHER_CLI_CONFIG="${STATE}/cli-other" \
  OPENSHELL_CLI="${CLI_BIN}" \
  bash "${ROOT}/tasks/scripts/demo-k8s-controller.sh"
