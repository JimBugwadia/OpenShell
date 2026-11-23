#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Installs the openshell-k8s-controller Helm chart in a throwaway kind cluster
# and tests it against a gateway built from this checkout:
#   1. The chart installs the CRDs and, with replicaCount=2, two replicas;
#      exactly one holds the leader Lease.
#   2. A template and a sandbox become Ready through the in-cluster controller.
#   3. Deleting the leader pod hands the Lease to another replica, which
#      resumes without creating a duplicate sandbox.
#   4. Rotating the controller's client certificate Secret takes effect without
#      a restart: a certificate for an identity that isn't the workspace manager
#      is refused, and rotating back restores access.
#   5. Deleting the resources removes them from the gateway, and uninstalling
#      the release keeps the CRDs.
#
# Docker must be running. build-k8s-controller-image.sh builds the image.
#
# Environment:
#   HELM_TEST_PORT        gateway port (default: 18293)
#   HELM_TEST_CLUSTER     kind cluster name (default: openshell-controller-helm)
#   K8S_CONTROLLER_BINARY prebuilt linux controller binary to package
#   HELM_TEST_KEEP=1      keep the cluster and gateway state afterward
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT="${HELM_TEST_PORT:-18293}"
CLUSTER="${HELM_TEST_CLUSTER:-openshell-controller-helm}"
NAMESPACE=agents
WORKSPACE=agents
RELEASE=controller
FULLNAME="${RELEASE}-openshell-k8s-controller"
GW_NAME=helm-test
IMAGE_TAG=helm-test
IMAGE="openshell/k8s-controller:${IMAGE_TAG}"
TEMPLATE_IMAGE="${HELM_TEST_IMAGE:-nvcr.io/nvidia/base/ubuntu:24.04}"
SUPERVISOR_IMAGE="${OPENSHELL_SUPERVISOR_IMAGE:-ghcr.io/nvidia/openshell/supervisor:0.1.6}"
SANDBOX_RUNTIME_IMAGE="${OPENSHELL_SANDBOX_RUNTIME_IMAGE:-ghcr.io/nvidia/openshell/sandbox:0.1.6}"
GATEWAY_BIN="${ROOT}/target/debug/openshell-gateway"
CLI_BIN="${ROOT}/target/debug/openshell"

step() { printf '\n==> %s\n' "$*"; }
pass() { printf '✔ %s\n' "$*"; }
fail() { printf '✘ %s\n' "$*" >&2; exit 1; }

for command in cargo docker kind kubectl helm openssl lsof; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
docker info >/dev/null 2>&1 || fail "docker daemon is not reachable"
lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1 && fail "port ${PORT} is in use; set HELM_TEST_PORT"
kind get clusters 2>/dev/null | grep -Fxq "${CLUSTER}" && fail "kind cluster ${CLUSTER} already exists"

mkdir -p "${ROOT}/target"
STATE="$(mktemp -d "${ROOT}/target/helm-test.XXXXXX")"
KCFG="${STATE}/kubeconfig"
gateway_pid='' created_cluster=0
kctl() { kubectl --kubeconfig "${KCFG}" "$@"; }
os() { XDG_CONFIG_HOME="${STATE}/cli-controller" "${CLI_BIN}" -g "${GW_NAME}" "$@"; }

cleanup() {
  result=$?
  trap - EXIT INT TERM
  step "Cleaning up"
  if [ "${result}" -ne 0 ] && [ "${created_cluster}" = 1 ]; then
    echo "--- controller logs ---" >&2
    kctl logs -n "${NAMESPACE}" -l app.kubernetes.io/instance="${RELEASE}" --prefix --tail=40 >&2 || true
  fi
  if [ "${result}" -ne 0 ] && [ "${created_cluster}" = 1 ]; then
    kctl get pods -n "${NAMESPACE}" >&2 || true
  fi
  # Remove any gateway sandbox the test left behind before stopping the gateway.
  if [ -n "${gateway_pid}" ]; then
    for name in $(os sandbox list --workspace "${WORKSPACE}" 2>/dev/null | awk 'NR > 1 {print $1}'); do
      os sandbox delete "${name}" --workspace "${WORKSPACE}" >/dev/null 2>&1 || true
    done
    kill "${gateway_pid}" 2>/dev/null || true
    wait "${gateway_pid}" 2>/dev/null || true
  fi
  if [ "${result}" -ne 0 ] && [ -f "${STATE}/gateway.log" ]; then
    echo "--- gateway log (last 30 lines) ---" >&2
    tail -30 "${STATE}/gateway.log" >&2
  fi
  if [ "${HELM_TEST_KEEP:-0}" = 1 ]; then
    echo "Kept cluster ${CLUSTER} and ${STATE}"
  else
    [ "${created_cluster}" = 1 ] && kind delete cluster --name "${CLUSTER}" >/dev/null 2>&1
    rm -rf "${STATE}"
  fi
  [ "${result}" -eq 0 ] && printf '\nPASS: controller Helm chart test completed.\n'
  exit "${result}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

issue_client_cert() {
  local out=$1 subject=$2
  mkdir -p "${out}"
  openssl ecparam -name prime256v1 -genkey -noout -out "${out}/tls.key" 2>/dev/null
  openssl req -new -key "${out}/tls.key" -subj "${subject}" -out "${out}/req.csr" 2>/dev/null
  printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n' >"${out}/ext"
  openssl x509 -req -in "${out}/req.csr" -CA "${STATE}/pki/ca.crt" -CAkey "${STATE}/pki/ca.key" \
    -CAcreateserial -days 1 -extfile "${out}/ext" -out "${out}/tls.crt" 2>/dev/null
  cp "${STATE}/pki/ca.crt" "${out}/ca.crt"
}

wait_for() {
  local what=$1 seconds=$2
  shift 2
  for _ in $(seq "${seconds}"); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "timed out after ${seconds}s waiting for ${what}"
}

lease_holder() {
  kctl get lease "${FULLNAME}" -n "${NAMESPACE}" -o 'jsonpath={.spec.holderIdentity}' 2>/dev/null
}

set_tls_secret() {
  kctl create secret generic gateway-tls -n "${NAMESPACE}" \
    --from-file=ca.crt="$1/ca.crt" --from-file=tls.crt="$1/tls.crt" --from-file=tls.key="$1/tls.key" \
    --dry-run=client -o yaml | kctl apply -f - >/dev/null
}

# ---------------------------------------------------------------------------
step "Build the gateway, the CLI, and the controller image"
cargo build -q --locked -p openshell-gateway --bin openshell-gateway -p openshell-cli \
  --manifest-path "${ROOT}/Cargo.toml"
IMAGE_TAG="${IMAGE_TAG}" bash "${ROOT}/tasks/scripts/build-k8s-controller-image.sh" >/dev/null
pass "built ${IMAGE}"

step "Start a gateway whose workspace ${WORKSPACE} is managed by the controller identity"
XDG_CONFIG_HOME="${STATE}/certgen-config" "${GATEWAY_BIN}" generate-certs --output-dir "${STATE}/pki" \
  --server-san 127.0.0.1 --server-san localhost --server-san host.docker.internal >/dev/null
CLI_MTLS="${STATE}/cli-controller/openshell/gateways/${GW_NAME}/mtls"
issue_client_cert "${CLI_MTLS}" "/CN=openshell-k8s-controller/OU=openshell-user"
cat >"${STATE}/cli-controller/openshell/gateways/${GW_NAME}/metadata.json" <<JSON
{"name": "${GW_NAME}", "gateway_endpoint": "https://127.0.0.1:${PORT}", "is_remote": false, "gateway_port": ${PORT}, "auth_mode": "mtls"}
JSON
# The pods get their own certificates, so rotating them never touches the CLI's.
issue_client_cert "${STATE}/controller-1" "/CN=openshell-k8s-controller/OU=openshell-user"
issue_client_cert "${STATE}/controller-2" "/CN=openshell-k8s-controller/OU=openshell-user"
issue_client_cert "${STATE}/other" "/CN=openshell-cli/OU=openshell-user"
cat >"${STATE}/gateway.toml" <<TOML
[openshell]
version = 2

[openshell.gateway]
name = "${GW_NAME}"
compute_driver = "docker"
guest_tls_ca = "${STATE}/pki/ca.crt"

[openshell.gateway.workspace_managers]
${WORKSPACE} = ["mtls:openshell-k8s-controller"]

[openshell.gateway.gateway_jwt]
signing_key_path = "${STATE}/pki/jwt/signing.pem"
public_key_path = "${STATE}/pki/jwt/public.pem"
kid_path = "${STATE}/pki/jwt/kid"
gateway_id = "${GW_NAME}"

[openshell.drivers.docker]
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
wait_for "the gateway to listen" 60 lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN
os workspace create --name "${WORKSPACE}" >/dev/null
pass "gateway on port ${PORT}"

step "1. Install the chart with two replicas"
created_cluster=1
kind create cluster --name "${CLUSTER}" --kubeconfig "${KCFG}" --wait 120s >/dev/null 2>&1
kind load docker-image "${IMAGE}" --name "${CLUSTER}" >/dev/null
kctl create namespace "${NAMESPACE}" >/dev/null
set_tls_secret "${STATE}/controller-1"
helm --kubeconfig "${KCFG}" install "${RELEASE}" "${ROOT}/deploy/helm/openshell-k8s-controller" \
  -n "${NAMESPACE}" --wait --timeout 120s \
  --set image.repository=openshell/k8s-controller --set image.tag="${IMAGE_TAG}" \
  --set image.pullPolicy=Never \
  --set gateway.url="https://host.docker.internal:${PORT}" \
  --set gateway.workspace="${WORKSPACE}" \
  --set gateway.tls.secretName=gateway-tls \
  --set replicaCount=2 --set credentialReloadSeconds=5 >/dev/null
kctl wait --for=condition=Established --timeout=60s crd/openshellsandboxes.openshell.ai \
  crd/openshellsandboxtemplates.openshell.ai crd/openshellproviders.openshell.ai >/dev/null
has_leader() { [ -n "$(lease_holder)" ]; }
wait_for "a Lease holder" 60 has_leader
leader="$(lease_holder)"
pods="$(kctl get pods -n "${NAMESPACE}" -l app.kubernetes.io/instance="${RELEASE}" -o name | sed 's|pod/||')"
[ "$(echo "${pods}" | wc -l | tr -d ' ')" = 2 ] || fail "expected two controller pods: ${pods}"
echo "${pods}" | grep -Fxq "${leader}" || fail "Lease holder ${leader} isn't a controller pod"
acquired=0
for pod in ${pods}; do
  kctl logs -n "${NAMESPACE}" "${pod}" | grep -q "acquired Lease" && acquired=$((acquired + 1))
done
[ "${acquired}" = 1 ] || fail "expected one replica to hold the Lease, found ${acquired}"
standby="$(echo "${pods}" | grep -Fxv "${leader}")"
pass "${leader} leads; ${standby} waits"

step "2. A template and a sandbox become Ready"
cat <<EOF | kctl apply -f - >/dev/null
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: agent
  namespace: ${NAMESPACE}
spec:
  image: ${TEMPLATE_IMAGE}
  environment:
    AGENT_RELEASE: "1"
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: ${NAMESPACE}
spec:
  templateRef:
    name: agent
EOF
kctl wait --for=condition=Ready --timeout=60s ossbxt/agent -n "${NAMESPACE}" >/dev/null
kctl wait --for=condition=Ready --timeout=300s ossbx/support-agent -n "${NAMESPACE}" >/dev/null
sandbox_id="$(kctl get ossbx support-agent -n "${NAMESPACE}" -o 'jsonpath={.status.sandboxId}')"
[ -n "${sandbox_id}" ] || fail "the sandbox has no gateway ID"
attempts="$(kctl get ossbx support-agent -n "${NAMESPACE}" -o 'jsonpath={.status.journal.createAttempts}')"
pass "sandbox ${sandbox_id} is Ready"

step "3. Delete the leader; another replica takes over"
old_leader="${leader}"
kctl delete pod "${old_leader}" -n "${NAMESPACE}" --wait=false >/dev/null
# The leader releases the Lease on shutdown, so another replica takes it
# without waiting for it to expire: the standby or the Deployment's
# replacement pod, whichever claims it first.
new_leader() {
  local holder
  holder="$(lease_holder)"
  [ -n "${holder}" ] && [ "${holder}" != "${old_leader}" ] \
    && kctl get pod "${holder}" -n "${NAMESPACE}" >/dev/null 2>&1
}
wait_for "another replica to take the Lease" 30 new_leader
leader="$(lease_holder)"
wait_for "${leader} to log the takeover" 30 \
  sh -c "kubectl --kubeconfig '${KCFG}' logs -n '${NAMESPACE}' '${leader}' | grep -q 'acquired Lease'"
# A change after the takeover proves the new leader reconciles.
kctl patch ossbxt agent -n "${NAMESPACE}" --type=merge -p '{"spec":{"environment":{"AGENT_RELEASE":"2"}}}' >/dev/null
kctl wait --for=jsonpath='{.status.currentRevision}'=agent-r2 --timeout=60s ossbxt/agent -n "${NAMESPACE}" >/dev/null
[ "$(kctl get ossbx support-agent -n "${NAMESPACE}" -o 'jsonpath={.status.sandboxId}')" = "${sandbox_id}" ] \
  || fail "the sandbox ID changed after the takeover"
[ "$(kctl get ossbx support-agent -n "${NAMESPACE}" -o 'jsonpath={.status.journal.createAttempts}')" = "${attempts}" ] \
  || fail "the new leader created the sandbox again"
[ "$(os sandbox list --workspace "${WORKSPACE}" | awk 'NR > 1' | wc -l | tr -d ' ')" = 1 ] \
  || fail "the gateway has more than one sandbox"
pass "${leader} took over from ${old_leader}, published agent-r2, and kept sandbox ${sandbox_id}"

step "4. Rotate the controller's client certificate"
reloads() { kctl logs -n "${NAMESPACE}" "${leader}" | grep -c "reconnected to the gateway with new credentials" || true; }
reloaded() { [ "$(reloads)" -gt "${before}" ]; }
# An identity that isn't the workspace manager: the gateway refuses its changes.
before="$(reloads)"
set_tls_secret "${STATE}/other"
wait_for "the controller to load the new certificate" 150 reloaded
kctl patch ossbxt agent -n "${NAMESPACE}" --type=merge -p '{"spec":{"environment":{"AGENT_RELEASE":"3"}}}' >/dev/null
sleep 15
[ "$(kctl get ossbxt agent -n "${NAMESPACE}" -o 'jsonpath={.status.currentRevision}')" = agent-r2 ] \
  || fail "a revision was published with the rotated-out identity"
pass "after rotation to another identity, the gateway refuses the controller's changes"
before="$(reloads)"
set_tls_secret "${STATE}/controller-2"
wait_for "the controller to load the new certificate" 150 reloaded
# The refused publish waits for a recheck; a new change reconciles at once.
kctl patch ossbxt agent -n "${NAMESPACE}" --type=merge -p '{"spec":{"environment":{"AGENT_RELEASE":"4"}}}' >/dev/null
kctl wait --for=jsonpath='{.status.currentRevision}'=agent-r4 --timeout=60s ossbxt/agent -n "${NAMESPACE}" >/dev/null
kctl get pod "${leader}" -n "${NAMESPACE}" -o 'jsonpath={.status.containerStatuses[0].restartCount}' | grep -qx 0 \
  || fail "the controller restarted"
pass "after rotation to a new manager certificate, agent-r4 is published without a restart"

step "5. Delete the resources and uninstall the release"
kctl delete ossbx support-agent -n "${NAMESPACE}" --timeout=120s >/dev/null
kctl delete ossbxt agent -n "${NAMESPACE}" --timeout=120s >/dev/null
[ "$(os sandbox list --workspace "${WORKSPACE}" | awk 'NR > 1' | wc -l | tr -d ' ')" = 0 ] \
  || fail "the gateway sandbox remains"
[ "$(os sandbox template list --workspace "${WORKSPACE}" | awk 'NR > 1' | wc -l | tr -d ' ')" = 0 ] \
  || fail "template revisions remain"
helm --kubeconfig "${KCFG}" uninstall "${RELEASE}" -n "${NAMESPACE}" --wait >/dev/null
kctl get crd openshellsandboxes.openshell.ai >/dev/null || fail "uninstall deleted the CRDs"
pass "gateway objects removed; the CRDs remain after uninstall"
