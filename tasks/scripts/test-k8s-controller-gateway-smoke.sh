#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

: "${KUBE_CONTEXT:?Set KUBE_CONTEXT to the intended Kubernetes context.}"
: "${OPENSHELL_CONTROLLER_NAMESPACE:?Set the namespace watched by the controller.}"
: "${OPENSHELL_CONTROLLER_GATEWAY:?Set the reachable gateway URL.}"
: "${OPENSHELL_CONTROLLER_WORKSPACE:?Set the gateway workspace.}"
: "${OPENSHELL_CONTROLLER_TEMPLATE:?Set an existing workspace workload template.}"

for command in cargo kubectl; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "ERROR: required command not found: ${command}" >&2
    exit 2
  fi
done

current_context="$(kubectl config current-context)"
if [ "${current_context}" != "${KUBE_CONTEXT}" ]; then
  echo "ERROR: the controller uses the kubeconfig's current context (${current_context}), not ${KUBE_CONTEXT}." >&2
  echo "Set KUBECONFIG to a config whose current context is ${KUBE_CONTEXT}, then retry." >&2
  exit 2
fi

if [ -n "${OPENSHELL_CONTROLLER_TLS_CERT:-}" ] && [ -z "${OPENSHELL_CONTROLLER_TLS_KEY:-}" ] \
  || [ -z "${OPENSHELL_CONTROLLER_TLS_CERT:-}" ] && [ -n "${OPENSHELL_CONTROLLER_TLS_KEY:-}" ]; then
  echo "ERROR: OPENSHELL_CONTROLLER_TLS_CERT and OPENSHELL_CONTROLLER_TLS_KEY must be set together." >&2
  exit 2
fi

kctl() {
  kubectl --context "${KUBE_CONTEXT}" "$@"
}

kctl get namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" >/dev/null
kctl get crd/openshellsandboxes.openshell.ai >/dev/null
kctl auth can-i --quiet list openshellsandboxes --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"
kctl auth can-i --quiet watch openshellsandboxes --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"
kctl auth can-i --quiet patch openshellsandboxes --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"
kctl auth can-i --quiet patch openshellsandboxes/status --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"

existing_sandboxes="$(kctl get openshellsandboxes \
  --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" \
  -o name)"
if [ -n "${existing_sandboxes}" ]; then
  echo "ERROR: namespace ${OPENSHELL_CONTROLLER_NAMESPACE} is not empty; refusing to run the namespace-wide controller." >&2
  echo "Use a dedicated namespace with no OpenShellSandbox resources and no other controller watching it." >&2
  printf '%s\n' "${existing_sandboxes}" >&2
  exit 2
fi

suffix="$(date +%s)-${RANDOM}"
resource_name="controller-smoke-${suffix}"
controller_log="${ROOT}/target/${resource_name}-controller.log"
if [ -e "${controller_log}" ]; then
  echo "ERROR: refusing to overwrite ${controller_log}." >&2
  exit 2
fi
mkdir -p "${ROOT}/target"

controller_args=(
  --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"
  --workspace "${OPENSHELL_CONTROLLER_WORKSPACE}"
  --gateway "${OPENSHELL_CONTROLLER_GATEWAY}"
)
if [ -n "${OPENSHELL_CONTROLLER_TOKEN_FILE:-}" ]; then
  controller_args+=(--token-file "${OPENSHELL_CONTROLLER_TOKEN_FILE}")
fi
if [ -n "${OPENSHELL_CONTROLLER_TLS_CA:-}" ]; then
  controller_args+=(--tls-ca "${OPENSHELL_CONTROLLER_TLS_CA}")
fi
if [ -n "${OPENSHELL_CONTROLLER_TLS_CERT:-}" ]; then
  controller_args+=(--tls-cert "${OPENSHELL_CONTROLLER_TLS_CERT}")
  controller_args+=(--tls-key "${OPENSHELL_CONTROLLER_TLS_KEY}")
fi

resource_created=0
controller_pid=
cleanup() {
  result=$?
  trap - EXIT INT TERM
  if [ "${resource_created}" = "1" ]; then
    if kctl delete openshellsandbox "${resource_name}" \
      --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" \
      --wait=true \
      --timeout=180s; then
      resource_created=0
    else
      echo "ERROR: finalizer cleanup did not complete for ${OPENSHELL_CONTROLLER_NAMESPACE}/${resource_name}." >&2
      echo "Leave the finalizer in place until the corresponding gateway sandbox is verified and cleaned up." >&2
      result=1
    fi
  fi
  if [ -n "${controller_pid}" ]; then
    kill "${controller_pid}" 2>/dev/null || true
    wait "${controller_pid}" 2>/dev/null || true
  fi
  if [ "${result}" -ne 0 ] && [ -f "${controller_log}" ]; then
    echo "--- controller log ---" >&2
    cat "${controller_log}" >&2
  fi
  rm -f "${controller_log}"
  exit "${result}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cargo build --locked --manifest-path "${ROOT}/crates/openshell-k8s-controller/Cargo.toml" >/dev/null
controller_binary="${OPENSHELL_CONTROLLER_BINARY:-${ROOT}/target/debug/openshell-k8s-controller}"
"${controller_binary}" "${controller_args[@]}" \
  >"${controller_log}" 2>&1 &
controller_pid=$!
sleep 2
if ! ps -p "${controller_pid}" >/dev/null; then
  echo "ERROR: controller exited during startup." >&2
  exit 1
fi

kctl create -f - <<EOF
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: ${resource_name}
  namespace: ${OPENSHELL_CONTROLLER_NAMESPACE}
spec:
  templateRef:
    name: ${OPENSHELL_CONTROLLER_TEMPLATE}
EOF
resource_created=1

kctl wait \
  --for=condition=Ready \
  --timeout="${OPENSHELL_CONTROLLER_TIMEOUT:-300}s" \
  "openshellsandbox/${resource_name}" \
  --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}"

status="$(kctl get "openshellsandbox/${resource_name}" \
  --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" \
  -o 'jsonpath=phase={.status.phase} gatewayPhase={.status.gatewayPhase} sandboxName={.status.sandboxName} sandboxId={.status.sandboxId}')"
printf '%s\n' "${status}"

if [ -n "${OPENSHELL_CONTROLLER_CLI:-}" ]; then
  sandbox_name="$(kctl get "openshellsandbox/${resource_name}" \
    --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" \
    -o 'jsonpath={.status.sandboxName}')"
  "${OPENSHELL_CONTROLLER_CLI}" sandbox exec "${sandbox_name}" \
    --gateway-endpoint "${OPENSHELL_CONTROLLER_GATEWAY}" \
    --workspace "${OPENSHELL_CONTROLLER_WORKSPACE}" \
    --no-tty \
    -- sh -c 'printf "controller-workload-ok\n"'
fi

kctl delete openshellsandbox "${resource_name}" \
  --namespace "${OPENSHELL_CONTROLLER_NAMESPACE}" \
  --wait=true \
  --timeout="${OPENSHELL_CONTROLLER_TIMEOUT:-300}s"
resource_created=0
echo "PASS: gateway-backed create, Ready observation, and finalizer deletion succeeded."
