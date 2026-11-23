#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Live test of the gateway's experimental per-workspace workspace_managers setting,
# applied here to the default workspace.
#
# Runs a gateway built from this checkout on its own port, with mTLS user
# authentication and three client identities:
#   openshell-k8s-controller  the listed workspace manager
#   openshell-cli             an ordinary user
#   agent-executor            a second manager: a service that creates a
#                             sandbox per conversation with CreateSandbox
#   oncall (OU=operator)      an operator, for emergency stop and delete
# It checks that only managers provision, that everyone can still read,
# that the operator can delete in an emergency, that the gateway refuses to
# start when the list changes while sandboxes exist, and that sessions
# created from a template family resolve to its latest revision and can't be
# changed by the other manager, and that only the manager that published a
# workload template can delete it. Everything, including the CLI's configuration, lives
# in one temporary directory.
#
# Environment:
#   MANAGERS_TEST_PORT   gateway port (default: 18291)
#   DEMO_PAUSE=1         pause between steps for a live walkthrough
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT="${MANAGERS_TEST_PORT:-18291}"
GW_NAME=managers-test
GATEWAY_BIN="${ROOT}/target/debug/openshell-gateway"
CLI_BIN="${ROOT}/target/debug/openshell"
SUPERVISOR_IMAGE="${OPENSHELL_SUPERVISOR_IMAGE:-ghcr.io/nvidia/openshell/supervisor:0.1.6}"
SANDBOX_RUNTIME_IMAGE="${OPENSHELL_SANDBOX_RUNTIME_IMAGE:-ghcr.io/nvidia/openshell/sandbox:0.1.6}"

bold=$'\033[1m'; green=$'\033[32m'; red=$'\033[31m'; dim=$'\033[2m'; reset=$'\033[0m'
[ -t 1 ] || { bold=; green=; red=; dim=; reset=; }
step() { printf '\n%s==> %s%s\n' "${bold}" "$*" "${reset}"; pause; }
pass() { printf '%s✔ %s%s\n' "${green}" "$*" "${reset}"; }
fail() { printf '%s✘ %s%s\n' "${red}" "$*" "${reset}" >&2; exit 1; }
pause() { if [ "${DEMO_PAUSE:-0}" = "1" ]; then read -r -p "   [Enter to continue]" </dev/tty; fi; }

for command in cargo docker openssl lsof; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
docker info >/dev/null 2>&1 || fail "docker daemon is not reachable"
if lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port ${PORT} is in use; set MANAGERS_TEST_PORT"
fi

mkdir -p "${ROOT}/target"
STATE="$(mktemp -d "${ROOT}/target/managers-test.XXXXXX")"
GATEWAY_LOG="${STATE}/gateway.log"
gateway_pid=
create_pid=
completed=0

stop_gateway() {
  if [ -n "${gateway_pid}" ]; then
    kill "${gateway_pid}" 2>/dev/null || true
    wait "${gateway_pid}" 2>/dev/null || true
    gateway_pid=
  fi
}

cleanup() {
  result=$?
  trap - EXIT INT TERM
  # Some shell errors exit with status 0; only a run that reached the end passes.
  [ "${completed}" = "1" ] || [ "${result}" -ne 0 ] || result=1
  [ -n "${create_pid}" ] && kill "${create_pid}" 2>/dev/null
  # Remove any sandbox the test created, as the manager, while the gateway runs.
  if [ -n "${gateway_pid}" ]; then
    as controller sandbox delete managed-1 >/dev/null 2>&1 || true
    as executor sandbox delete session-1 >/dev/null 2>&1 || true
    sleep 2
  fi
  stop_gateway
  if [ "${result}" -ne 0 ] && [ -f "${GATEWAY_LOG}" ]; then
    echo "--- gateway log (last 40 lines) ---" >&2
    tail -40 "${GATEWAY_LOG}" >&2
  fi
  rm -rf "${STATE}"
  [ "${result}" -eq 0 ] && printf '\n%sPASS: workspace manager checks behaved as configured.%s\n' "${green}" "${reset}"
  exit "${result}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# The CLI wraps long errors across lines; join them before matching.
denied() { printf '%s' "$1" | tr '\n' ' ' | tr -s ' │' ' ' | grep -qi "managed by another identity"; }

# Run the CLI as one identity, with its own isolated configuration.
as() {
  local who=$1
  shift
  XDG_CONFIG_HOME="${STATE}/cli-${who}" "${CLI_BIN}" -g "${GW_NAME}" "$@"
}

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
  cat >"${dir}/metadata.json" <<EOF
{"name": "${GW_NAME}", "gateway_endpoint": "https://127.0.0.1:${PORT}", "is_remote": false, "gateway_port": ${PORT}, "auth_mode": "mtls"}
EOF
}

write_config() {
  local managers=$1
  cat >"${STATE}/gateway.toml" <<EOF
[openshell]
version = 2

[openshell.gateway]
name = "${GW_NAME}"
compute_driver = "docker"
guest_tls_ca = "${STATE}/pki/ca.crt"

[openshell.gateway.workspace_managers]
default = ${managers}

[openshell.gateway.gateway_jwt]
signing_key_path = "${STATE}/pki/jwt/signing.pem"
public_key_path = "${STATE}/pki/jwt/public.pem"
kid_path = "${STATE}/pki/jwt/kid"
gateway_id = "${GW_NAME}"

[openshell.drivers.docker]
supervisor_image = "${SUPERVISOR_IMAGE}"
sandbox_runtime_image = "${SANDBOX_RUNTIME_IMAGE}"
image_pull_policy = "if_not_present"
sandbox_label = "${GW_NAME}"
app_armor_profile = "Unconfined"
EOF
}

# Start the gateway; returns nonzero if it exits before listening.
start_gateway() {
  : >"${GATEWAY_LOG}"
  "${GATEWAY_BIN}" --config "${STATE}/gateway.toml" --port "${PORT}" --log-level info \
    --compute-driver docker \
    --tls-cert "${STATE}/pki/server/tls.crt" --tls-key "${STATE}/pki/server/tls.key" \
    --tls-client-ca "${STATE}/pki/ca.crt" --enable-mtls-auth true --enable-operator-auth true \
    --db-url "sqlite:${STATE}/gateway.db?mode=rwc" >>"${GATEWAY_LOG}" 2>&1 &
  gateway_pid=$!
  for _ in $(seq 1 60); do
    if ! kill -0 "${gateway_pid}" 2>/dev/null; then
      wait "${gateway_pid}" 2>/dev/null || true
      gateway_pid=
      return 1
    fi
    lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "gateway did not start listening on ${PORT}"
}

# ---------------------------------------------------------------------------
step "Set up: build the gateway and CLI, issue three client identities"
cargo build -q -p openshell-gateway --bin openshell-gateway -p openshell-cli \
  --manifest-path "${ROOT}/Cargo.toml"
# generate-certs also copies client certificates into the CLI's configuration
# directory; isolate it so the user's real CLI configuration is never touched.
XDG_CONFIG_HOME="${STATE}/certgen-config" "${GATEWAY_BIN}" generate-certs --output-dir "${STATE}/pki" \
  --server-san 127.0.0.1 --server-san localhost >/dev/null
issue_client_cert controller "/CN=openshell-k8s-controller/OU=openshell-user"
issue_client_cert cli "/CN=openshell-cli/OU=openshell-user"
issue_client_cert operator "/CN=oncall/OU=operator"
issue_client_cert executor "/CN=agent-executor/OU=openshell-user"
MANAGERS='["mtls:openshell-k8s-controller", "mtls:agent-executor"]'
write_config "${MANAGERS}"
sed -n '/^\[openshell.gateway\]/,/^$/p' "${STATE}/gateway.toml"
start_gateway || fail "gateway exited during startup"
pass "gateway listening on 127.0.0.1:${PORT}; workspace managers: the controller and agent-executor"

step "1. A user who isn't a manager can't create a sandbox"
if output="$(as cli sandbox create --name cli-made --no-tty -- true 2>&1)"; then
  fail "a non-manager created a sandbox"
fi
denied "${output}" || fail "unexpected error: ${output}"
pass "create rejected for mtls:openshell-cli"

step "2. The manager can create one"
# Create waits for the sandbox to start; only the record matters here.
as controller sandbox create --name managed-1 --no-tty -- sleep 600 >"${STATE}/create.log" 2>&1 &
create_pid=$!
for _ in $(seq 1 30); do
  as controller sandbox get managed-1 >/dev/null 2>&1 && break
  sleep 1
done
as controller sandbox get managed-1 >"${STATE}/get.txt" 2>&1 || fail "the manager's create did not persist"
kill "${create_pid}" 2>/dev/null; wait "${create_pid}" 2>/dev/null || true; create_pid=
grep -E "workspace-manager|workspace-managers" "${STATE}/get.txt" | sed 's/^ */  /'
grep -q "internal.openshell.ai/workspace-manager: mtls:openshell-k8s-controller" "${STATE}/get.txt" \
  || fail "the sandbox does not record its manager"
pass "managed-1 created and records mtls:openshell-k8s-controller as its manager"

step "3. Other users can still see it, but can't change it"
as cli sandbox list | grep -q managed-1 || fail "a non-manager can't list sandboxes"
pass "mtls:openshell-cli lists managed-1"
for action in delete stop; do
  if output="$(as cli sandbox "${action}" managed-1 2>&1)"; then
    fail "a non-manager could ${action} the sandbox"
  fi
  denied "${output}" || fail "unexpected ${action} error: ${output}"
  pass "${action} rejected for mtls:openshell-cli"
done

step "4. The gateway refuses to start if the managers list changes while sandboxes exist"
stop_gateway
write_config '["mtls:openshell-k8s-controller", "mtls:agent-executor", "mtls:someone-else"]'
if start_gateway; then
  fail "the gateway started with a changed managers list"
fi
grep -A4 "changed from" "${GATEWAY_LOG}" | tr '\n' ' ' | tr -s ' │' ' ' | cut -c1-260; echo
pass "startup refused with the changed list"
write_config "${MANAGERS}"
start_gateway || fail "the gateway did not restart with the original list"
pass "the gateway starts again with the original list"

step "5. An operator can delete it in an emergency"
as operator sandbox delete managed-1 >/dev/null || fail "the operator's emergency delete failed"
for _ in $(seq 1 30); do
  as cli sandbox get managed-1 >/dev/null 2>&1 || break
  sleep 1
done
as cli sandbox get managed-1 >/dev/null 2>&1 && fail "managed-1 still exists after the emergency delete"
pass "operator oncall deleted managed-1"

step "6. A session service creates a session from a template family"
# The Kubernetes controller publishes each OpenShellSandboxTemplate generation
# as an immutable revision with these labels; publish two by hand here.
for generation in 1 2; do
  as controller sandbox template create "agent-r${generation}" \
    --label openshell.ai/template-family=agent \
    --label "openshell.ai/template-generation=${generation}" >/dev/null \
    || fail "could not publish template revision agent-r${generation}"
done
pass "published revisions agent-r1 and agent-r2 of template family agent"
if output="$(as cli sandbox template create cli-template 2>&1)"; then
  fail "a non-manager published a template"
fi
denied "${output}" || fail "unexpected template create error: ${output}"
pass "template create rejected for mtls:openshell-cli"
if output="$(as executor sandbox template delete agent-r1 2>&1)"; then
  fail "agent-executor deleted the controller's template"
fi
denied "${output}" || fail "unexpected template delete error: ${output}"
pass "agent-executor can't delete the controller's template"
as executor sandbox create --name session-1 --template agent --no-tty -- sleep 600 \
  >"${STATE}/session.log" 2>&1 &
create_pid=$!
for _ in $(seq 1 30); do
  as executor sandbox get session-1 >/dev/null 2>&1 && break
  sleep 1
done
as executor sandbox get session-1 >"${STATE}/session.txt" 2>&1 || fail "the session was not created"
kill "${create_pid}" 2>/dev/null; wait "${create_pid}" 2>/dev/null || true; create_pid=
grep -E "Workload template|workspace-manager:" "${STATE}/session.txt" | sed 's/^ */  /'
grep -q "agent-r2" "${STATE}/session.txt" || fail "the session did not resolve to the latest revision"
grep -q "internal.openshell.ai/workspace-manager: mtls:agent-executor" "${STATE}/session.txt" \
  || fail "the session does not record agent-executor as its manager"
pass "session-1 resolved family agent to revision agent-r2 and records agent-executor"
if output="$(as controller sandbox delete session-1 2>&1)"; then
  fail "the controller could delete the session service's session"
fi
denied "${output}" || fail "unexpected delete error: ${output}"
pass "the controller can't delete the session service's session"
as executor sandbox delete session-1 >/dev/null || fail "the session service could not end its session"
for _ in $(seq 1 30); do
  as cli sandbox get session-1 >/dev/null 2>&1 || break
  sleep 1
done
for generation in 1 2; do
  as controller sandbox template delete "agent-r${generation}" >/dev/null \
    || fail "the controller could not delete its template agent-r${generation}"
done
pass "agent-executor ended session-1"
completed=1
