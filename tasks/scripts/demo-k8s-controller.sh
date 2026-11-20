#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end demo and test of the experimental OpenShellSandbox controller.
#
# Creates a throwaway kind cluster for the CRDs, plus a dedicated workspace on
# an existing gateway, then walks through:
#   1. Declare a template and a sandbox that uses it, and wait for both to be
#      Ready, then measure the sandbox's etcd size and confirm a Ready sandbox
#      causes no writes.
#   2. Show the same sandbox through the gateway CLI and run a command in it.
#   3. Reject a workload change (the workload template is immutable).
#   4. Change the template: it publishes a new revision, and the running
#      sandbox stays on the revision it started with.
#   5. Restart the controller and confirm it doesn't create a duplicate.
#   6. Request a missing template: blocked with CreateRejected, deletes cleanly.
#   7. Delete the sandbox and the template, and confirm the gateway removes the
#      sandbox and every template revision.
# The script deletes only what it created and exits nonzero on any failed check.
#
# Environment:
#   DEMO_GATEWAY       Registered CLI gateway name (default: openshell)
#   DEMO_GATEWAY_URL   Gateway URL for the controller (default: from the CLI registration)
#   DEMO_MTLS_DIR      Directory with ca.crt, tls.crt, tls.key
#                      (default: ~/.config/openshell/gateways/$DEMO_GATEWAY/mtls)
#   DEMO_IMAGE         Template image (default: nvcr.io/nvidia/base/ubuntu:24.04)
#   DEMO_PAUSE=1       Pause between steps for a live walkthrough
#   DEMO_KEEP_CLUSTER=1  Keep the kind cluster afterward
#   OPENSHELL_CLI      CLI binary (default: openshell)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GW="${DEMO_GATEWAY:-openshell}"
CLI="${OPENSHELL_CLI:-openshell}"
MTLS_DIR="${DEMO_MTLS_DIR:-${HOME}/.config/openshell/gateways/${GW}/mtls}"
TIMEOUT="${DEMO_TIMEOUT:-300}"
CLUSTER=openshell-crd-demo
NAMESPACE=agents
suffix="$(printf '%04x%04x' "${RANDOM}" "${RANDOM}" | cut -c1-5)"
WORKSPACE="crd-demo-${suffix}"
TEMPLATE="crd-demo-agent"
IMAGE="${DEMO_IMAGE:-nvcr.io/nvidia/base/ubuntu:24.04}"
KCFG="${ROOT}/target/${CLUSTER}.kubeconfig"
CONTROLLER_LOG="${ROOT}/target/${CLUSTER}-controller.log"
WATCH_LOG="${ROOT}/target/${CLUSTER}-watch.log"
CONTROLLER="${ROOT}/target/debug/openshell-k8s-controller"

bold=$'\033[1m'; green=$'\033[32m'; red=$'\033[31m'; dim=$'\033[2m'; reset=$'\033[0m'
[ -t 1 ] || { bold=; green=; red=; dim=; reset=; }

step() { printf '\n%s==> %s%s\n' "${bold}" "$*" "${reset}"; pause; }
run() { printf '%s$ %s%s\n' "${dim}" "$*" "${reset}"; "$@"; }
pass() { printf '%s✔ %s%s\n' "${green}" "$*" "${reset}"; }
fail() { printf '%s✘ %s%s\n' "${red}" "$*" "${reset}" >&2; exit 1; }
pause() { if [ "${DEMO_PAUSE:-0}" = "1" ]; then read -r -p "   [Enter to continue]" </dev/tty; fi; }

kctl() { kubectl --kubeconfig "${KCFG}" "$@"; }
os() { "${CLI}" -g "${GW}" "$@"; }
jp() { kctl get openshellsandbox "$1" -n "${NAMESPACE}" -o "jsonpath=$2"; }
tp() { kctl get openshellsandboxtemplate "$1" -n "${NAMESPACE}" -o "jsonpath=$2"; }
revisions() { os sandbox template list --workspace "${WORKSPACE}" 2>/dev/null | awk 'NR > 1 {print $1}' | grep -c "^${TEMPLATE}-r" || true; }

for command in cargo kind kubectl docker "${CLI}"; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
for file in ca.crt tls.crt tls.key; do
  [ -f "${MTLS_DIR}/${file}" ] || fail "missing ${MTLS_DIR}/${file}; set DEMO_MTLS_DIR"
done
GATEWAY_URL="${DEMO_GATEWAY_URL:-$("${CLI}" gateway list 2>/dev/null \
  | awk -v gw="${GW}" '{sub(/^\*/, "")} $1 == gw {print $2}')}"
[ -n "${GATEWAY_URL}" ] || fail "cannot resolve the URL for gateway ${GW}; set DEMO_GATEWAY_URL"
if kind get clusters 2>/dev/null | grep -Fxq "${CLUSTER}"; then
  fail "kind cluster ${CLUSTER} already exists; delete it or rerun later"
fi
[ -e "${KCFG}" ] && fail "refusing to overwrite ${KCFG}"
mkdir -p "${ROOT}/target"

created_cluster=0 created_workspace=0 controller_pid= watch_pid=
resources=() templates=() sandbox_names=()

stop_controller() {
  if [ -n "${controller_pid}" ]; then
    kill "${controller_pid}" 2>/dev/null || true
    wait "${controller_pid}" 2>/dev/null || true
    controller_pid=
  fi
}

start_controller() {
  KUBECONFIG="${KCFG}" "${CONTROLLER}" \
    --namespace "${NAMESPACE}" --workspace "${WORKSPACE}" --gateway "${GATEWAY_URL}" \
    --tls-ca "${MTLS_DIR}/ca.crt" --tls-cert "${MTLS_DIR}/tls.crt" --tls-key "${MTLS_DIR}/tls.key" \
    >>"${CONTROLLER_LOG}" 2>&1 &
  controller_pid=$!
  sleep 2
  ps -p "${controller_pid}" >/dev/null || fail "controller exited during startup (see ${CONTROLLER_LOG})"
}

cleanup() {
  result=$?
  trap - EXIT INT TERM
  printf '\n%s==> Cleaning up%s\n' "${bold}" "${reset}"
  for resource in "${resources[@]+"${resources[@]}"}"; do
    if kctl get openshellsandbox "${resource}" -n "${NAMESPACE}" >/dev/null 2>&1; then
      [ -n "${controller_pid}" ] || start_controller || true
      kctl delete openshellsandbox "${resource}" -n "${NAMESPACE}" --timeout=120s >/dev/null 2>&1 \
        || echo "WARNING: finalizer cleanup for ${resource} did not finish." >&2
    fi
  done
  # Delete templates after sandboxes, so the controller removes their revisions last.
  for template in "${templates[@]+"${templates[@]}"}"; do
    if kctl get openshellsandboxtemplate "${template}" -n "${NAMESPACE}" >/dev/null 2>&1; then
      [ -n "${controller_pid}" ] || start_controller || true
      kctl delete openshellsandboxtemplate "${template}" -n "${NAMESPACE}" --timeout=120s >/dev/null 2>&1 \
        || echo "WARNING: finalizer cleanup for template ${template} did not finish." >&2
    fi
  done
  stop_controller
  [ -n "${watch_pid}" ] && kill "${watch_pid}" 2>/dev/null
  rm -f "${WATCH_LOG}"
  # Fall back to the CLI only for gateway sandboxes this demo created.
  for name in "${sandbox_names[@]+"${sandbox_names[@]}"}"; do
    if os sandbox get "${name}" --workspace "${WORKSPACE}" >/dev/null 2>&1; then
      echo "Deleting leftover gateway sandbox ${name}." >&2
      os sandbox delete "${name}" --workspace "${WORKSPACE}" >/dev/null 2>&1 || result=1
    fi
  done
  [ "${created_workspace}" = "1" ] && { os workspace delete "${WORKSPACE}" >/dev/null 2>&1 \
    || echo "WARNING: could not delete workspace ${WORKSPACE}." >&2; }
  if [ "${created_cluster}" = "1" ]; then
    if [ "${DEMO_KEEP_CLUSTER:-0}" = "1" ]; then
      echo "Kept kind cluster ${CLUSTER}; kubeconfig: ${KCFG}"
    else
      kind delete cluster --name "${CLUSTER}" --kubeconfig "${KCFG}" >/dev/null 2>&1 || true
      rm -f "${KCFG}"
    fi
  fi
  if [ "${result}" -ne 0 ] && [ -f "${CONTROLLER_LOG}" ]; then
    echo "--- controller log ---" >&2
    cat "${CONTROLLER_LOG}" >&2
  fi
  rm -f "${CONTROLLER_LOG}"
  [ "${result}" -eq 0 ] && printf '\n%sPASS: controller demo completed.%s\n' "${green}" "${reset}"
  exit "${result}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
step "Set up: kind cluster and a dedicated gateway workspace"
echo "Gateway ${GW} at ${GATEWAY_URL}; workspace ${WORKSPACE}"
created_cluster=1
run kind create cluster --name "${CLUSTER}" --kubeconfig "${KCFG}" --wait 120s >/dev/null
kctl create namespace "${NAMESPACE}" >/dev/null
created_workspace=1
run os workspace create --name "${WORKSPACE}" >/dev/null
run cargo build --locked -q -p openshell-k8s-controller --manifest-path "${ROOT}/Cargo.toml"

step "Install the CRDs generated by the controller binary"
"${CONTROLLER}" --print-crd | kctl apply -f - >/dev/null
run kctl wait --for=condition=Established --timeout=60s \
  crd/openshellsandboxes.openshell.ai crd/openshellsandboxtemplates.openshell.ai
start_controller
pass "controller running (pid ${controller_pid}), bound to ${NAMESPACE} -> ${WORKSPACE}"

# ---------------------------------------------------------------------------
step "1. Declare a template and a sandbox that uses it"
templates+=("${TEMPLATE}")
resources+=(support-agent)
# Record every change to the resource, to count the writes it causes.
kctl get ossbx -n "${NAMESPACE}" --watch-only --no-headers \
  -o 'custom-columns=NAME:.metadata.name,RV:.metadata.resourceVersion' >"${WATCH_LOG}" 2>/dev/null &
watch_pid=$!
sleep 1
cat <<EOF | tee /dev/stderr | kctl apply -f - >/dev/null
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: ${TEMPLATE}
  namespace: ${NAMESPACE}
spec:
  image: ${IMAGE}
  environment:
    AGENT_RELEASE: "1"
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: ${NAMESPACE}
spec:
  workloadTemplate: ${TEMPLATE}
EOF
run kctl wait --for=condition=Ready --timeout=60s "ossbxt/${TEMPLATE}" -n "${NAMESPACE}"
run kctl wait --for=condition=Ready "--timeout=${TIMEOUT}s" ossbx/support-agent -n "${NAMESPACE}"
run kctl get ossbxt,ossbx -n "${NAMESPACE}"
sandbox="$(jp support-agent '{.status.sandboxName}')"
sandbox_id="$(jp support-agent '{.status.sandboxId}')"
sandbox_names+=("${sandbox}")
echo "Ready condition: $(jp support-agent '{.status.conditions[?(@.type=="Ready")].message}')"
[ -n "${sandbox_id}" ] || fail "status has no gateway sandbox ID"
[ "$(tp "${TEMPLATE}" '{.status.currentRevision}')" = "${TEMPLATE}-r1" ] \
  || fail "template did not publish revision ${TEMPLATE}-r1"
[ "$(jp support-agent '{.status.templateRevision}')" = "${TEMPLATE}-r1" ] \
  || fail "sandbox is not pinned to ${TEMPLATE}-r1"
pass "template published ${TEMPLATE}-r1; gateway sandbox ${sandbox} (${sandbox_id}) uses it and is Ready"

step "Scale check: etcd footprint and write count for one sandbox"
writes() { grep -c '^support-agent ' "${WATCH_LOG}" || true; }
writes_to_ready="$(writes)"
api_bytes="$(kctl get ossbx support-agent -n "${NAMESPACE}" -o json | wc -c | tr -d ' ')"
etcd_bytes="$(kctl -n kube-system exec "etcd-${CLUSTER}-control-plane" -- etcdctl \
  --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt \
  --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key \
  get "/registry/openshell.ai/openshellsandboxes/${NAMESPACE}/support-agent" --print-value-only \
  2>/dev/null | wc -c | tr -d ' ')"
echo "Stored size in etcd: ${etcd_bytes} bytes (API JSON: ${api_bytes} bytes)"
echo "Writes from creation to Ready: ${writes_to_ready}"
steady="${DEMO_STEADY_SECONDS:-45}"
echo "Watching ${steady}s of steady state..."
sleep "${steady}"
steady_writes=$(( $(writes) - writes_to_ready ))
echo "Writes while Ready: ${steady_writes}"
[ "${steady_writes}" -eq 0 ] || fail "a Ready sandbox caused ${steady_writes} writes"
pass "a Ready sandbox adds no etcd writes"

step "2. The same sandbox is visible through the gateway CLI"
run os sandbox get "${sandbox}" --workspace "${WORKSPACE}" | sed -n '/^ *Id:/,/^ *Workload template:/p'
output="$(run os sandbox exec "${sandbox}" --workspace "${WORKSPACE}" --no-tty \
  -- sh -c 'echo "hello from a CRD-managed sandbox on $(uname -n)"')"
echo "${output}"
grep -q "hello from a CRD-managed sandbox" <<<"${output}" || fail "exec output missing"
pass "command ran inside the sandbox"

step "3. The workload template is immutable; changes are rejected at admission"
if kctl patch ossbx support-agent -n "${NAMESPACE}" --type=merge \
  -p '{"spec":{"workloadTemplate":"something-else"}}' 2>&1; then
  fail "API server accepted a workload template change"
fi
pass "workload change rejected; create a new resource to run a different workload"

step "4. Change the template; running sandboxes keep their revision"
run kctl patch ossbxt "${TEMPLATE}" -n "${NAMESPACE}" --type=merge \
  -p '{"spec":{"environment":{"AGENT_RELEASE":"2"}}}'
run kctl wait --for=jsonpath='{.status.currentRevision}'="${TEMPLATE}-r2" --timeout=60s \
  "ossbxt/${TEMPLATE}" -n "${NAMESPACE}"
run os sandbox template list --workspace "${WORKSPACE}"
[ "$(revisions)" -eq 2 ] || fail "gateway does not list both template revisions"
[ "$(jp support-agent '{.status.templateRevision}')" = "${TEMPLATE}-r1" ] \
  || fail "running sandbox moved off ${TEMPLATE}-r1"
[ "$(jp support-agent '{.status.sandboxId}')" = "${sandbox_id}" ] || fail "sandbox was replaced"
pass "template published ${TEMPLATE}-r2; support-agent still runs ${TEMPLATE}-r1"

step "5. Restart the controller; it resumes from the status journal without duplicating work"
attempts_before="$(jp support-agent '{.status.journal.createAttempts}')"
stop_controller
start_controller
sleep 5
[ "$(jp support-agent '{.status.sandboxId}')" = "${sandbox_id}" ] || fail "sandbox ID changed after restart"
[ "$(jp support-agent '{.status.journal.createAttempts}')" = "${attempts_before}" ] \
  || fail "controller submitted another create after restart"
[ "$(jp support-agent '{.status.conditions[?(@.type=="Ready")].status}')" = "True" ] \
  || fail "Ready condition lost after restart"
pass "same sandbox ID, ${attempts_before} create attempt(s), still Ready"

step "6. A missing template is rejected, and the resource still deletes cleanly"
resources+=(missing-template)
cat <<EOF | tee /dev/stderr | kctl apply -f - >/dev/null
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: missing-template
  namespace: ${NAMESPACE}
spec:
  workloadTemplate: does-not-exist
EOF
run kctl wait --for=jsonpath='{.status.reason}'=CreateRejected --timeout=120s \
  ossbx/missing-template -n "${NAMESPACE}"
run kctl get ossbx -n "${NAMESPACE}"
[ "$(jp missing-template '{.status.conditions[?(@.type=="Ready")].status}')" = "False" ] \
  || fail "rejected resource does not report Ready=False"
[ "$(jp missing-template '{.status.createMayExist}{.status.journal.createMayExist}')" = "false" ] \
  || fail "rejected create still marked as possibly existing"
run kctl delete ossbx missing-template -n "${NAMESPACE}" --timeout=60s
pass "rejected resource deleted without manual finalizer removal"

step "7. Delete the sandbox and the template; the finalizers wait for the gateway"
echo "Finalizers: $(jp support-agent '{.metadata.finalizers}')"
run kctl delete ossbx support-agent -n "${NAMESPACE}" "--timeout=${TIMEOUT}s"
if os sandbox get "${sandbox}" --workspace "${WORKSPACE}" >/dev/null 2>&1; then
  fail "gateway sandbox ${sandbox} still exists after finalizer removal"
fi
pass "gateway sandbox ${sandbox} removed before the resource was released"
run kctl delete ossbxt "${TEMPLATE}" -n "${NAMESPACE}" --timeout=120s
[ "$(revisions)" -eq 0 ] || fail "template revisions remain in the gateway"
pass "template ${TEMPLATE} and its gateway revisions removed"
