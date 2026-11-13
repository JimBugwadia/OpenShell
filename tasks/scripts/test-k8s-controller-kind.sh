#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLUSTER_NAME=openshell-crd-prototype
KUBE_CONTEXT="kind-${CLUSTER_NAME}"
NAMESPACE=openshell-sandbox
KUBECONFIG_PATH="${ROOT}/target/openshell-crd-prototype.kubeconfig"
CREATED_CLUSTER=0
CREATED_KUBECONFIG=0

for command in cargo kind kubectl docker; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "ERROR: required command not found: ${command}" >&2
    exit 2
  fi
done

if kind get clusters | grep -Fxq "${CLUSTER_NAME}"; then
  echo "ERROR: kind cluster '${CLUSTER_NAME}' already exists; refusing to modify it." >&2
  exit 2
fi
if [ -e "${KUBECONFIG_PATH}" ]; then
  echo "ERROR: refusing to overwrite ${KUBECONFIG_PATH}." >&2
  exit 2
fi

mkdir -p "${ROOT}/target"

kctl() {
  kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" "$@"
}

cleanup() {
  if [ "${CREATED_CLUSTER}" = "1" ]; then
    kind delete cluster --name "${CLUSTER_NAME}" --kubeconfig "${KUBECONFIG_PATH}"
  fi
  if [ "${CREATED_KUBECONFIG}" = "1" ]; then
    rm -f "${KUBECONFIG_PATH}"
  fi
}
trap cleanup EXIT

CREATED_CLUSTER=1
CREATED_KUBECONFIG=1
kind create cluster --name "${CLUSTER_NAME}" --kubeconfig "${KUBECONFIG_PATH}" --wait 120s
kctl cluster-info >/dev/null
kctl create namespace "${NAMESPACE}"

cargo run --locked --manifest-path "${ROOT}/crates/openshell-k8s-controller/Cargo.toml" -- \
  --print-crd \
  | kctl apply -f -
kctl wait \
  --for=condition=Established \
  --timeout=120s \
  crd/openshellsandboxes.openshell.ai

kctl apply \
  -f "${ROOT}/deploy/kubernetes-controller/rbac.yaml"

SA="system:serviceaccount:${NAMESPACE}:openshell-k8s-controller"
kctl auth can-i --quiet patch openshellsandboxes \
  --as="${SA}" --namespace="${NAMESPACE}"
kctl auth can-i --quiet patch openshellsandboxes/status \
  --as="${SA}" --namespace="${NAMESPACE}"
kctl auth can-i --quiet list openshellsandboxes \
  --as="${SA}" --namespace="${NAMESPACE}"
kctl auth can-i --quiet watch openshellsandboxes \
  --as="${SA}" --namespace="${NAMESPACE}"
if kctl auth can-i --quiet create openshellsandboxes \
  --as="${SA}" --namespace="${NAMESPACE}"; then
  echo "ERROR: controller service account unexpectedly has create access." >&2
  exit 1
fi
if kctl auth can-i --quiet get pods \
  --as="${SA}" --namespace="${NAMESPACE}"; then
  echo "ERROR: controller service account unexpectedly has pod access." >&2
  exit 1
fi
if kctl auth can-i --quiet patch openshellsandboxes \
  --as="${SA}" --namespace=default; then
  echo "ERROR: controller service account unexpectedly has access outside its namespace." >&2
  exit 1
fi

kctl create --dry-run=server -f - <<EOF
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: schema-check
  namespace: ${NAMESPACE}
spec:
  workloadTemplate: support-agent-v2
EOF

if kctl create --dry-run=server -f - <<EOF
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: missing-template
  namespace: ${NAMESPACE}
spec: {}
EOF
then
  echo "ERROR: API server accepted a resource without workloadTemplate." >&2
  exit 1
fi

kctl apply -f - <<EOF
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: immutable-check
  namespace: ${NAMESPACE}
spec:
  workloadTemplate: support-agent-v2
EOF

if kctl patch openshellsandbox immutable-check \
  --namespace="${NAMESPACE}" \
  --type=merge \
  --patch '{"spec":{"workloadTemplate":"different-template"}}'; then
  echo "ERROR: API server accepted a mutation to immutable spec." >&2
  exit 1
fi

echo "PASS: live CRD schema, immutability, and namespaced RBAC checks succeeded."
