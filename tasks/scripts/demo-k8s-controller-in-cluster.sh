#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Part 3 of examples/kubernetes-sandbox-crd: installs everything in one kind
# cluster and checks it, following the runbook's steps:
#   1. Create a kind cluster with Cilium as the CNI, so NetworkPolicy is
#      enforced, and install the Agent Sandbox controller.
#   2. Install the OpenShell gateway with its Helm chart and register it with
#      the CLI through a port-forward.
#   3. Install the controller in its own namespace, bound to namespace agents
#      and gateway workspace agents.
#   4. Declare a provider, a template, and a sandbox; wait for all three to be
#      Ready and run a command in the sandbox.
#   5. Check that the sandbox's workload pod can't open connections directly,
#      while an ordinary pod can.
#   6. Delete namespace agents and check that the gateway sandbox, template,
#      and provider are removed and the namespace finishes deleting.
# The CLI configuration lives in a temporary directory, so your registered
# gateways don't change. The script deletes the cluster unless
# E2E_KEEP_CLUSTER=1, and exits nonzero on any failed check.
#
# Environment:
#   E2E_CLUSTER            kind cluster name (default: openshell-e2e)
#   E2E_PORT               local port for the gateway port-forward (default: 18080)
#   E2E_CHART_VERSION      gateway chart version (default: 0.0.0-dev)
#   E2E_CILIUM_VERSION     Cilium version (default: 1.18.1)
#   E2E_AGENT_SANDBOX_VERSION  Agent Sandbox release (default: v1.0.12)
#   E2E_KEEP_CLUSTER=1     keep the cluster afterward
#   DEMO_PAUSE=1           pause between steps
#   OPENSHELL_CLI          CLI binary (default: openshell)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLUSTER="${E2E_CLUSTER:-openshell-e2e}"
PORT="${E2E_PORT:-18080}"
CHART_VERSION="${E2E_CHART_VERSION:-0.0.0-dev}"
CILIUM_VERSION="${E2E_CILIUM_VERSION:-1.18.1}"
AGENT_SANDBOX_VERSION="${E2E_AGENT_SANDBOX_VERSION:-v1.0.12}"
CLI="${OPENSHELL_CLI:-openshell}"
GW=kind
NAMESPACE=agents
CONTROLLER_NAMESPACE=openshell-system
RELEASE=openshell-controller
DEPLOYMENT="${RELEASE}-openshell-k8s-controller"
export KUBECONFIG="${ROOT}/target/${CLUSTER}.kubeconfig"

bold=$'\033[1m'; green=$'\033[32m'; red=$'\033[31m'; dim=$'\033[2m'; reset=$'\033[0m'
[ -t 1 ] || { bold=; green=; red=; dim=; reset=; }
step() { printf '\n%s==> %s%s\n' "${bold}" "$*" "${reset}"; pause; }
run() { printf '%s$ %s%s\n' "${dim}" "$*" "${reset}"; "$@"; }
pass() { printf '%s✔ %s%s\n' "${green}" "$*" "${reset}"; }
fail() { printf '%s✘ %s%s\n' "${red}" "$*" "${reset}" >&2; exit 1; }
pause() { if [ "${DEMO_PAUSE:-0}" = "1" ]; then read -r -p "   [Enter to continue]" </dev/tty; fi; }
os() { "${CLI}" -g "${GW}" "$@"; }
# Run a busybox connection check and print CONNECTED or BLOCKED.
probe='nc -w 5 -z 1.1.1.1 443 && echo CONNECTED || echo BLOCKED'

for command in kind kubectl helm cilium docker "${CLI}"; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
docker info >/dev/null 2>&1 || fail "docker daemon is not reachable"
if kind get clusters 2>/dev/null | grep -Fxq "${CLUSTER}"; then
  fail "kind cluster ${CLUSTER} already exists; delete it or set E2E_CLUSTER"
fi
if lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port ${PORT} is in use; set E2E_PORT"
fi
mkdir -p "${ROOT}/target"
STATE="$(mktemp -d "${ROOT}/target/in-cluster-demo.XXXXXX")"
export XDG_CONFIG_HOME="${STATE}/config"
MTLS="${XDG_CONFIG_HOME}/openshell/gateways/${GW}/mtls"

created_cluster=0 forward_pid=''
cleanup() {
  result=$?
  trap - EXIT INT TERM
  printf '\n%s==> Cleaning up%s\n' "${bold}" "${reset}"
  if [ "${result}" -ne 0 ] && [ "${created_cluster}" = "1" ]; then
    echo "--- controller log ---" >&2
    kubectl logs -n "${CONTROLLER_NAMESPACE}" "deployment/${DEPLOYMENT}" --tail=40 >&2 2>/dev/null || true
    echo "--- gateway log ---" >&2
    kubectl logs -n openshell openshell-0 --tail=40 >&2 2>/dev/null || true
  fi
  [ -n "${forward_pid}" ] && kill "${forward_pid}" 2>/dev/null
  if [ "${created_cluster}" = "1" ]; then
    if [ "${E2E_KEEP_CLUSTER:-0}" = "1" ]; then
      echo "Kept cluster ${CLUSTER}; KUBECONFIG=${KUBECONFIG}"
      echo "CLI configuration: XDG_CONFIG_HOME=${XDG_CONFIG_HOME}"
      echo "Restart the port-forward with: kubectl -n openshell port-forward svc/openshell ${PORT}:8080"
      exit "${result}"
    fi
    kind delete cluster --name "${CLUSTER}" --kubeconfig "${KUBECONFIG}" >/dev/null 2>&1 || true
    rm -f "${KUBECONFIG}"
  fi
  rm -rf "${STATE}"
  exit "${result}"
}
trap cleanup EXIT INT TERM

step "1. Create a kind cluster with Cilium and install Agent Sandbox"
cat >"${STATE}/kind.yaml" <<'YAML'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  # Cilium replaces kind's default CNI, which doesn't enforce NetworkPolicy.
  disableDefaultCNI: true
YAML
created_cluster=1
run kind create cluster --name "${CLUSTER}" --config "${STATE}/kind.yaml" --kubeconfig "${KUBECONFIG}"
run cilium install --version "${CILIUM_VERSION}" --wait
run cilium status --wait --wait-duration 5m >/dev/null
run kubectl apply -f "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${AGENT_SANDBOX_VERSION}/sandbox.yaml"
run kubectl -n agent-sandbox-system rollout status deployment --timeout=180s
pass "cluster ${CLUSTER} runs Cilium ${CILIUM_VERSION} and Agent Sandbox ${AGENT_SANDBOX_VERSION}"

step "2. Install the gateway and register it with the CLI"
run helm install openshell oci://quay.io/ai-openshell/charts/openshell --version "${CHART_VERSION}" \
  --namespace openshell --create-namespace --wait --timeout 8m
kubectl -n openshell port-forward svc/openshell "${PORT}:8080" >"${STATE}/port-forward.log" 2>&1 &
forward_pid=$!
mkdir -p "${MTLS}"
for file in ca.crt tls.crt tls.key; do
  kubectl -n openshell get secret openshell-client-tls -o "jsonpath={.data.${file//./\\.}}" \
    | base64 -d >"${MTLS}/${file}"
done
for _ in $(seq 1 30); do
  lsof -nP -iTCP:"${PORT}" -sTCP:LISTEN >/dev/null 2>&1 && break
  sleep 1
done
# With --local, the CLI imports the client certificate of a gateway installed
# on this machine, such as one from Homebrew, over the files copied above.
# Point that import at an empty location so the chart's certificate stays.
run env OPENSHELL_LOCAL_TLS_DIR="${STATE}/no-local-tls" \
  "${CLI}" gateway add "https://127.0.0.1:${PORT}" --local --name "${GW}" >/dev/null
status="$(os status 2>&1 || true)"
grep -q "Status: Connected" <<<"${status}" || { echo "${status}" >&2; fail "the CLI can't connect to the gateway"; }
run os workspace create --name "${NAMESPACE}"
pass "gateway $(awk '/Version:/ {print $2}' <<<"${status}") is Connected through 127.0.0.1:${PORT}"

step "3. Install the controller in ${CONTROLLER_NAMESPACE}, bound to ${NAMESPACE}"
run env IMAGE_TAG=demo bash "${ROOT}/tasks/scripts/build-k8s-controller-image.sh" >"${STATE}/image-build.log" 2>&1 \
  || { tail -20 "${STATE}/image-build.log" >&2; fail "controller image build failed"; }
run kind load docker-image openshell/k8s-controller:demo --name "${CLUSTER}" >/dev/null
run kubectl create namespace "${NAMESPACE}"
run kubectl create namespace "${CONTROLLER_NAMESPACE}"
run kubectl -n "${CONTROLLER_NAMESPACE}" create secret generic gateway-tls \
  --from-file="${MTLS}/ca.crt" --from-file="${MTLS}/tls.crt" --from-file="${MTLS}/tls.key"
run helm install "${RELEASE}" "${ROOT}/deploy/helm/openshell-k8s-controller" \
  --namespace "${CONTROLLER_NAMESPACE}" --wait \
  --set watchNamespace="${NAMESPACE}" \
  --set image.repository=openshell/k8s-controller --set image.tag=demo --set image.pullPolicy=Never \
  --set gateway.url=https://openshell.openshell.svc:8080 \
  --set gateway.workspace="${NAMESPACE}" \
  --set gateway.tls.secretName=gateway-tls
# Read whole outputs before matching: with pipefail, grep -q can fail a
# pipeline by closing it early.
leader() { [[ "$(kubectl -n "${CONTROLLER_NAMESPACE}" logs "deployment/${DEPLOYMENT}" 2>/dev/null)" == *"acquired Lease"* ]]; }
for _ in $(seq 1 30); do
  leader && break
  sleep 1
done
leader || fail "the controller didn't take its Lease"
pass "controller is running in ${CONTROLLER_NAMESPACE} and watches ${NAMESPACE}"

step "4. Declare a provider, a template, and a sandbox"
run os provider profile import -f "${ROOT}/providers/github.yaml" --workspace "${NAMESPACE}" >/dev/null
run kubectl create secret generic github-token -n "${NAMESPACE}" --from-literal=token=demo-not-a-real-token
kubectl apply -f - <<YAML
apiVersion: openshell.ai/v1alpha1
kind: OpenShellProvider
metadata:
  name: github
  namespace: ${NAMESPACE}
spec:
  type: github
  credentials:
    - name: GITHUB_TOKEN
      secretKeyRef:
        name: github-token
        key: token
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: demo-agent
  namespace: ${NAMESPACE}
spec:
  image: nvcr.io/nvidia/base/ubuntu:24.04
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
    name: demo-agent
  providers: [github]
YAML
run kubectl wait --for=condition=Ready osprov/github ossbxt/demo-agent -n "${NAMESPACE}" --timeout=120s
run kubectl wait --for=condition=Ready ossbx/support-agent -n "${NAMESPACE}" --timeout=400s
run kubectl get osprov,ossbxt,ossbx -n "${NAMESPACE}"
NAME="$(kubectl get ossbx support-agent -n "${NAMESPACE}" -o jsonpath='{.status.sandboxName}')"
run kubectl get pods -n openshell
output="$(os sandbox exec "${NAME}" --workspace "${NAMESPACE}" --no-tty -- echo "hello from ${NAME}")"
[ "${output}" = "hello from ${NAME}" ] || fail "sandbox exec returned: ${output}"
pass "sandbox ${NAME} is Ready, runs as pods in namespace openshell, and runs commands"

step "5. Check that NetworkPolicy is enforced"
run kubectl -n openshell get networkpolicy
workload="$(kubectl -n openshell get pod -l openshell.ai/boundary-role=workload -o jsonpath='{.items[0].metadata.name}')"
[ -n "${workload}" ] || fail "no workload pod found"
# An ephemeral container shares the workload pod's network, and so its policy.
run kubectl -n openshell debug "${workload}" -q --image=busybox:1.36 --profile=general \
  --container=netcheck -- sh -c "${probe}" >/dev/null 2>&1
for _ in $(seq 1 60); do
  sandboxed="$(kubectl -n openshell logs "${workload}" -c netcheck 2>/dev/null || true)"
  [ -n "${sandboxed}" ] && break
  sleep 1
done
run kubectl run netcheck --restart=Never --image=busybox:1.36 -- sh -c "${probe}" >/dev/null
run kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/netcheck --timeout=120s >/dev/null
ordinary="$(kubectl logs netcheck)"
kubectl delete pod netcheck --wait=false >/dev/null
echo "sandbox workload pod: ${sandboxed:-no result}; ordinary pod: ${ordinary}"
[ "${ordinary}" = "CONNECTED" ] || fail "the ordinary pod can't reach the internet, so the check proves nothing"
[ "${sandboxed}" = "BLOCKED" ] || fail "the sandbox workload pod opened a direct connection"
pass "the workload can't connect out directly; its traffic must go through the supervisor"

step "6. Delete namespace ${NAMESPACE}"
started=$(date +%s)
run kubectl delete namespace "${NAMESPACE}" --timeout=300s
echo "namespace deleted in $(( $(date +%s) - started )) seconds"
[[ "$(os sandbox list --workspace "${NAMESPACE}" 2>&1)" == *"No sandboxes found"* ]] \
  || fail "the gateway sandbox is still there"
[[ "$(os sandbox template list --workspace "${NAMESPACE}" 2>&1)" == *"No sandbox templates found"* ]] \
  || fail "the gateway template revisions are still there"
[[ "$(os provider list --workspace "${NAMESPACE}" 2>&1)" == *"No providers found"* ]] \
  || fail "the gateway provider is still there"
pass "the finalizers removed the gateway sandbox, template, and provider, and the namespace is gone"

printf '\n%sPASS: in-cluster demo completed.%s\n' "${green}" "${reset}"
