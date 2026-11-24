#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end demo and test of the experimental OpenShellSandbox controller.
#
# Creates a throwaway kind cluster, plus a dedicated workspace on an existing
# gateway, builds the controller image, and installs it with its Helm chart.
# Then it walks through:
#   1. Declare a provider backed by a Secret, a template, and a sandbox that
#      uses both and declares its policy inline, and wait for all three to be Ready. Rotate the Secret and
#      confirm the controller reapplies the provider. Then measure the
#      sandbox's etcd size and confirm a Ready sandbox causes no writes.
#   2. Show the same sandbox through the gateway CLI and run a command in it.
#   3. Reject a workload change (the workload template is immutable).
#   4. Change the template: it publishes a new revision, and the running
#      sandbox stays on the revision it started with.
#   5. Restart the controller and confirm it doesn't create a duplicate.
#   6. Request a missing template: blocked with TemplateNotFound, deletes cleanly.
#   7. Delete the sandbox, the provider, and the template, and confirm the
#      gateway removes the sandbox, the provider, and every template revision.
# The script deletes only what it created and exits nonzero on any failed check.
#
# Environment:
#   DEMO_GATEWAY       Registered CLI gateway name (default: openshell)
#   DEMO_GATEWAY_URL   Gateway URL as the controller pod reaches it (default:
#                      the CLI registration's URL, with localhost or 127.0.0.1
#                      replaced by host.docker.internal)
#   DEMO_MTLS_DIR      Directory with ca.crt, tls.crt, tls.key for the controller
#                      (default: ~/.config/openshell/gateways/$DEMO_GATEWAY/mtls)
#   DEMO_IMAGE         Template image (default: nvcr.io/nvidia/base/ubuntu:24.04)
#   DEMO_PAUSE=1       Pause between steps for a live walkthrough
#   DEMO_CLUSTER       kind cluster name (default: openshell-crd-demo)
#   DEMO_NAMESPACE     Namespace and gateway workspace (default: agents)
#   DEMO_KEEP_CLUSTER=1  Keep the kind cluster afterward
#   OPENSHELL_CLI      CLI binary (default: openshell)
#   DEMO_OTHER_CLI_CONFIG  XDG config directory for a second CLI identity; if
#                      set, checks that it can't change the controller's objects
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GW="${DEMO_GATEWAY:-openshell}"
CLI="${OPENSHELL_CLI:-openshell}"
MTLS_DIR="${DEMO_MTLS_DIR:-${HOME}/.config/openshell/gateways/${GW}/mtls}"
TIMEOUT="${DEMO_TIMEOUT:-300}"
CLUSTER="${DEMO_CLUSTER:-openshell-crd-demo}"
NAMESPACE="${DEMO_NAMESPACE:-agents}"
WORKSPACE="${NAMESPACE}"
TEMPLATE="crd-demo-agent"
PROVIDER="crd-demo-github"
IMAGE="${DEMO_IMAGE:-nvcr.io/nvidia/base/ubuntu:24.04}"
KCFG="${ROOT}/target/${CLUSTER}.kubeconfig"
WATCH_LOG="${ROOT}/target/${CLUSTER}-watch.log"
RELEASE=openshell-controller
DEPLOYMENT="${RELEASE}-openshell-k8s-controller"
CONTROLLER_NAMESPACE=openshell-system
CONTROLLER_IMAGE_TAG=demo

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
pp() { kctl get openshellprovider "$1" -n "${NAMESPACE}" -o "jsonpath=$2"; }
revisions() { os sandbox template list --workspace "${WORKSPACE}" 2>/dev/null | awk 'NR > 1 {print $1}' | grep -c "^${TEMPLATE}-r" || true; }

for command in kind kubectl helm docker "${CLI}"; do
  command -v "${command}" >/dev/null 2>&1 || fail "required command not found: ${command}"
done
for file in ca.crt tls.crt tls.key; do
  [ -f "${MTLS_DIR}/${file}" ] || fail "missing ${MTLS_DIR}/${file}; set DEMO_MTLS_DIR"
done
# The controller runs in a kind pod, which reaches the host's gateway through
# Docker's host alias. The gateway's certificate must include that name.
GATEWAY_URL="${DEMO_GATEWAY_URL:-$("${CLI}" gateway list 2>/dev/null \
  | awk -v gw="${GW}" '{sub(/^\*/, "")} $1 == gw {print $2}' \
  | sed -E 's#//(localhost|127\.0\.0\.1)([:/]|$)#//host.docker.internal\2#')}"
[ -n "${GATEWAY_URL}" ] || fail "cannot resolve the URL for gateway ${GW}; set DEMO_GATEWAY_URL"
if kind get clusters 2>/dev/null | grep -Fxq "${CLUSTER}"; then
  fail "kind cluster ${CLUSTER} already exists; delete it or rerun later"
fi
[ -e "${KCFG}" ] && fail "refusing to overwrite ${KCFG}"
if os workspace get "${WORKSPACE}" >/dev/null 2>&1; then
  fail "gateway workspace ${WORKSPACE} already exists; delete it or set DEMO_GATEWAY to another gateway"
fi
mkdir -p "${ROOT}/target"

created_cluster=0 created_workspace=0 imported_profile=0 watch_pid=''
resources=() templates=() providers=() sandbox_names=()

cleanup() {
  result=$?
  trap - EXIT INT TERM
  printf '\n%s==> Cleaning up%s\n' "${bold}" "${reset}"
  if [ "${result}" -ne 0 ] && [ "${created_cluster}" = "1" ]; then
    echo "--- controller log ---" >&2
    kctl logs -n "${CONTROLLER_NAMESPACE}" "deployment/${DEPLOYMENT}" --tail=40 >&2 2>/dev/null || true
  fi
  # The controller keeps running until the cluster is deleted, so the
  # finalizers can remove the gateway objects.
  for resource in "${resources[@]+"${resources[@]}"}"; do
    if kctl get openshellsandbox "${resource}" -n "${NAMESPACE}" >/dev/null 2>&1; then
      kctl delete openshellsandbox "${resource}" -n "${NAMESPACE}" --timeout=120s >/dev/null 2>&1 \
        || echo "WARNING: finalizer cleanup for ${resource} did not finish." >&2
    fi
  done
  # Delete providers after sandboxes; the gateway refuses to delete one in use.
  for provider in "${providers[@]+"${providers[@]}"}"; do
    if kctl get openshellprovider "${provider}" -n "${NAMESPACE}" >/dev/null 2>&1; then
      kctl delete openshellprovider "${provider}" -n "${NAMESPACE}" --timeout=120s >/dev/null 2>&1 \
        || echo "WARNING: finalizer cleanup for provider ${provider} did not finish." >&2
    fi
  done
  # Delete templates after sandboxes, so the controller removes their revisions last.
  for template in "${templates[@]+"${templates[@]}"}"; do
    if kctl get openshellsandboxtemplate "${template}" -n "${NAMESPACE}" >/dev/null 2>&1; then
      kctl delete openshellsandboxtemplate "${template}" -n "${NAMESPACE}" --timeout=120s >/dev/null 2>&1 \
        || echo "WARNING: finalizer cleanup for template ${template} did not finish." >&2
    fi
  done
  [ -n "${watch_pid}" ] && kill "${watch_pid}" 2>/dev/null
  rm -f "${WATCH_LOG}"
  # Fall back to the CLI only for gateway sandboxes this demo created.
  for name in "${sandbox_names[@]+"${sandbox_names[@]}"}"; do
    if os sandbox get "${name}" --workspace "${WORKSPACE}" >/dev/null 2>&1; then
      echo "Deleting leftover gateway sandbox ${name}." >&2
      os sandbox delete "${name}" --workspace "${WORKSPACE}" >/dev/null 2>&1 || result=1
    fi
  done
  # A workspace with a profile in it never finishes deleting.
  [ "${imported_profile}" = "1" ] && { os provider profile delete github --workspace "${WORKSPACE}" >/dev/null 2>&1 \
    || echo "WARNING: could not delete provider profile github." >&2; }
  [ "${created_workspace}" = "1" ] && { os workspace delete "${WORKSPACE}" >/dev/null 2>&1 \
    || echo "WARNING: could not delete workspace ${WORKSPACE}." >&2; }
  if [ "${created_workspace}" = "1" ]; then
    for _ in $(seq 30); do os workspace get "${WORKSPACE}" >/dev/null 2>&1 || break; sleep 1; done
    if os workspace get "${WORKSPACE}" >/dev/null 2>&1; then
      echo "ERROR: workspace ${WORKSPACE} is still terminating; something in it was left behind." >&2
      result=1
    fi
  fi
  if [ "${created_cluster}" = "1" ]; then
    if [ "${DEMO_KEEP_CLUSTER:-0}" = "1" ]; then
      echo "Kept kind cluster ${CLUSTER}; kubeconfig: ${KCFG}"
    else
      kind delete cluster --name "${CLUSTER}" --kubeconfig "${KCFG}" >/dev/null 2>&1 || true
      rm -f "${KCFG}"
    fi
  fi
  [ "${result}" -eq 0 ] && printf '\n%sPASS: controller demo completed.%s\n' "${green}" "${reset}"
  exit "${result}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
step "Set up: kind cluster, a dedicated gateway workspace, and the controller image"
echo "Gateway ${GW}; the controller reaches it at ${GATEWAY_URL}; workspace ${WORKSPACE}"
created_cluster=1
run kind create cluster --name "${CLUSTER}" --kubeconfig "${KCFG}" --wait 120s >/dev/null
kctl create namespace "${NAMESPACE}" >/dev/null
run os workspace create --name "${WORKSPACE}" >/dev/null
created_workspace=1
run env IMAGE_TAG="${CONTROLLER_IMAGE_TAG}" bash "${ROOT}/tasks/scripts/build-k8s-controller-image.sh"
run kind load docker-image "openshell/k8s-controller:${CONTROLLER_IMAGE_TAG}" --name "${CLUSTER}" >/dev/null

step "Install the controller with its Helm chart"
# The controller authenticates to the gateway with this client certificate.
# It runs in its own namespace, so deleting the watched namespace can't delete
# the controller before its finalizers finish.
kctl create namespace "${CONTROLLER_NAMESPACE}" >/dev/null
kctl create secret generic gateway-tls -n "${CONTROLLER_NAMESPACE}" --from-file=ca.crt="${MTLS_DIR}/ca.crt" \
  --from-file=tls.crt="${MTLS_DIR}/tls.crt" --from-file=tls.key="${MTLS_DIR}/tls.key" >/dev/null
run helm --kubeconfig "${KCFG}" install "${RELEASE}" "${ROOT}/deploy/helm/openshell-k8s-controller" \
  --namespace "${CONTROLLER_NAMESPACE}" --wait --timeout 120s --set watchNamespace="${NAMESPACE}" \
  --set image.repository=openshell/k8s-controller --set image.tag="${CONTROLLER_IMAGE_TAG}" \
  --set image.pullPolicy=Never \
  --set gateway.url="${GATEWAY_URL}" --set gateway.workspace="${WORKSPACE}" \
  --set gateway.tls.secretName=gateway-tls >/dev/null
run kctl wait --for=condition=Established --timeout=60s \
  crd/openshellsandboxes.openshell.ai crd/openshellsandboxtemplates.openshell.ai \
  crd/openshellproviders.openshell.ai
for _ in $(seq 30); do
  kctl logs -n "${CONTROLLER_NAMESPACE}" "deployment/${DEPLOYMENT}" 2>/dev/null | grep -q "acquired Lease" && break
  sleep 1
done
kctl logs -n "${CONTROLLER_NAMESPACE}" "deployment/${DEPLOYMENT}" | grep -q "acquired Lease" \
  || fail "the controller didn't start reconciling"
pass "controller deployed with Helm, bound to ${NAMESPACE} -> ${WORKSPACE}"

# ---------------------------------------------------------------------------
step "1. Declare a provider, a template, and a sandbox that uses both"
# The provider type must exist in the workspace. The token is a placeholder:
# the demo never calls GitHub.
run os provider profile import -f "${ROOT}/providers/github.yaml" --workspace "${WORKSPACE}" >/dev/null
imported_profile=1
run kctl create secret generic "${PROVIDER}-token" -n "${NAMESPACE}" \
  --from-literal=token=demo-placeholder-1 >/dev/null
providers+=("${PROVIDER}")
templates+=("${TEMPLATE}")
resources+=(support-agent)
# Record every change to the resource, to count the writes it causes.
kctl get ossbx -n "${NAMESPACE}" --watch-only --no-headers \
  -o 'custom-columns=NAME:.metadata.name,RV:.metadata.resourceVersion' >"${WATCH_LOG}" 2>/dev/null &
watch_pid=$!
sleep 1
cat <<EOF | tee /dev/stderr | kctl apply -f - >/dev/null
apiVersion: openshell.ai/v1alpha1
kind: OpenShellProvider
metadata:
  name: ${PROVIDER}
  namespace: ${NAMESPACE}
spec:
  type: github
  credentials:
    - name: GITHUB_TOKEN
      secretKeyRef:
        name: ${PROVIDER}-token
        key: token
---
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
  templateRef:
    name: ${TEMPLATE}
  providers: [${PROVIDER}]
  # The same format as a policy file for openshell sandbox create --policy.
  policy:
    version: 1
    filesystem_policy:
      include_workdir: true
      read_only: [/bin, /usr, /lib, /proc, /dev/urandom, /app, /etc, /var/log]
      read_write: [/sandbox, /tmp, /dev/null]
    landlock:
      compatibility: best_effort
    network_policies:
      github_api:
        name: github-api-readonly
        endpoints:
          - { host: api.github.com, port: 443, protocol: rest, enforcement: enforce, access: read-only }
        binaries:
          - { path: /usr/bin/curl }
EOF
run kctl wait --for=condition=Ready --timeout=60s "osprov/${PROVIDER}" -n "${NAMESPACE}"
run kctl wait --for=condition=Ready --timeout=60s "ossbxt/${TEMPLATE}" -n "${NAMESPACE}"
run kctl wait --for=condition=Ready "--timeout=${TIMEOUT}s" ossbx/support-agent -n "${NAMESPACE}"
run kctl get osprov,ossbxt,ossbx -n "${NAMESPACE}"
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
os provider get "${PROVIDER}" --workspace "${WORKSPACE}" >/dev/null \
  || fail "gateway provider ${PROVIDER} doesn't exist"
version="$(pp "${PROVIDER}" "{.status.secretVersions.${PROVIDER}-token}")"
kctl create secret generic "${PROVIDER}-token" -n "${NAMESPACE}" \
  --from-literal=token=demo-placeholder-2 --dry-run=client -o yaml | kctl apply -f - >/dev/null
for _ in $(seq 30); do
  [ "$(pp "${PROVIDER}" "{.status.secretVersions.${PROVIDER}-token}")" != "${version}" ] && break
  sleep 1
done
[ "$(pp "${PROVIDER}" "{.status.secretVersions.${PROVIDER}-token}")" != "${version}" ] \
  || fail "provider was not reapplied after the Secret changed"
pass "gateway provider ${PROVIDER} created from its Secret and reapplied after rotation"

if [ -n "${DEMO_OTHER_CLI_CONFIG:-}" ]; then
  step "Another identity can't change the controller's sandbox or provider"
  other() { XDG_CONFIG_HOME="${DEMO_OTHER_CLI_CONFIG}" "${CLI}" -g "${GW}" "$@"; }
  if output="$(other sandbox delete "${sandbox}" --workspace "${WORKSPACE}" 2>&1)"; then
    fail "another identity deleted the controller's sandbox"
  fi
  echo "${output}" | tr -s ' \n' ' ' | cut -c1-200
  if output="$(other provider delete "${PROVIDER}" --workspace "${WORKSPACE}" 2>&1)"; then
    fail "another identity deleted the controller's provider"
  fi
  echo "${output}" | tr -s ' \n' ' ' | cut -c1-200
  os sandbox get "${sandbox}" --workspace "${WORKSPACE}" >/dev/null || fail "the sandbox is gone"
  os provider get "${PROVIDER}" --workspace "${WORKSPACE}" >/dev/null || fail "the provider is gone"
  pass "the workspace manager's sandbox and provider are unchanged"
fi

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
os policy get "${sandbox}" --workspace "${WORKSPACE}" --full 2>/dev/null | grep -q "api.github.com" \
  || fail "the gateway sandbox doesn't have the declared policy"
pass "the gateway sandbox has the policy declared in the resource"

step "3. The workload template is immutable; changes are rejected at admission"
if kctl patch ossbx support-agent -n "${NAMESPACE}" --type=merge \
  -p '{"spec":{"templateRef":{"name":"something-else"}}}' 2>&1; then
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
run kctl rollout restart "deployment/${DEPLOYMENT}" -n "${CONTROLLER_NAMESPACE}"
run kctl rollout status "deployment/${DEPLOYMENT}" -n "${CONTROLLER_NAMESPACE}" --timeout=120s
for _ in $(seq 30); do
  kctl logs -n "${CONTROLLER_NAMESPACE}" "deployment/${DEPLOYMENT}" 2>/dev/null | grep -q "acquired Lease" && break
  sleep 1
done
# Give the new controller time to reconcile the existing resources.
sleep 10
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
  templateRef:
    name: does-not-exist
EOF
run kctl wait --for=jsonpath='{.status.reason}'=TemplateNotFound --timeout=120s \
  ossbx/missing-template -n "${NAMESPACE}"
run kctl get ossbx -n "${NAMESPACE}"
[ "$(jp missing-template '{.status.conditions[?(@.type=="Ready")].status}')" = "False" ] \
  || fail "rejected resource does not report Ready=False"
[ "$(jp missing-template '{.status.createMayExist}{.status.journal.createMayExist}')" = "false" ] \
  || fail "rejected create still marked as possibly existing"
run kctl delete ossbx missing-template -n "${NAMESPACE}" --timeout=60s
pass "rejected resource deleted without manual finalizer removal"

step "7. Delete the sandbox, the provider, and the template; the finalizers wait for the gateway"
echo "Finalizers: $(jp support-agent '{.metadata.finalizers}')"
run kctl delete ossbx support-agent -n "${NAMESPACE}" "--timeout=${TIMEOUT}s"
if os sandbox get "${sandbox}" --workspace "${WORKSPACE}" >/dev/null 2>&1; then
  fail "gateway sandbox ${sandbox} still exists after finalizer removal"
fi
pass "gateway sandbox ${sandbox} removed before the resource was released"
run kctl delete osprov "${PROVIDER}" -n "${NAMESPACE}" --timeout=120s
if os provider get "${PROVIDER}" --workspace "${WORKSPACE}" >/dev/null 2>&1; then
  fail "gateway provider ${PROVIDER} still exists"
fi
pass "provider ${PROVIDER} removed from the gateway"
run kctl delete ossbxt "${TEMPLATE}" -n "${NAMESPACE}" --timeout=120s
[ "$(revisions)" -eq 0 ] || fail "template revisions remain in the gateway"
pass "template ${TEMPLATE} and its gateway revisions removed"
