#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -Eeuo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
MODULE="${ROOT}/kubernetes/apiserver"
STATE="${MODULE}/.demo"
CLUSTER="openshell-api-demo"
VERSION="0.1.6"
IMAGE="openshell-kube-apiserver:dev"
KUBECONFIG="${STATE}/kubeconfig"
export KUBECONFIG

kube() { kubectl --kubeconfig="${KUBECONFIG}" "$@"; }
user_kube() { kube --as=openshell-demo "$@"; }
cli() {
  XDG_CONFIG_HOME="${STATE}/config" openshell \
    --gateway demo --gateway-endpoint "https://localhost:${PORT}" "$@"
}
diagnostics() {
  kube get pods -A -o wide >&2 || true
  kube -n openshell logs statefulset/openshell -c openshell-gateway --tail=80 >&2 || true
  kube -n openshell logs deployment/openshell-kube-apiserver --tail=80 >&2 || true
  kube get events -A --sort-by=.lastTimestamp | tail -30 >&2 || true
}
require() {
  local tool
  for tool in "$@"; do
    command -v "${tool}" >/dev/null || { echo "Missing required tool: ${tool}" >&2; exit 1; }
  done
}
up() {
  require docker kind kubectl helm openssl jq openshell
  mkdir -p "${STATE}"
  if kind get clusters | grep -qx "${CLUSTER}"; then
    echo "Reusing cluster ${CLUSTER}."
    kind export kubeconfig --name "${CLUSTER}" --kubeconfig "${KUBECONFIG}"
  else
    kind create cluster --name "${CLUSTER}" --image kindest/node:v1.33.1 \
      --kubeconfig "${KUBECONFIG}" --wait 120s
  fi
  trap diagnostics ERR
  kube wait --for=condition=Ready nodes --all --timeout=120s
  bash "${ROOT}/e2e/support/install-agent-sandbox.sh" --kubeconfig="${KUBECONFIG}"
  local image
  for image in gateway sandbox supervisor; do
    docker pull "ghcr.io/nvidia/openshell/${image}:${VERSION}"
    kind load docker-image "ghcr.io/nvidia/openshell/${image}:${VERSION}" --name "${CLUSTER}"
  done
  docker pull nvcr.io/nvidia/base/ubuntu:24.04
  kind load docker-image nvcr.io/nvidia/base/ubuntu:24.04 --name "${CLUSTER}"
  docker build -f "${MODULE}/Dockerfile" -t "${IMAGE}" "${ROOT}"
  kind load docker-image "${IMAGE}" --name "${CLUSTER}"
  helm upgrade --install openshell oci://ghcr.io/nvidia/openshell/helm-chart \
    --version "${VERSION}" --namespace openshell --create-namespace \
    --kubeconfig "${KUBECONFIG}" --set global.image.tag="${VERSION}" \
    --set server.telemetryEnabled=false --wait --timeout 5m
  local serving_secret
  serving_secret="$(kube -n openshell get secret openshell-kube-apiserver-serving-cert --ignore-not-found -o json)"
  if [ -n "${serving_secret}" ]; then
    printf '%s' "${serving_secret}" | jq -er '.data["tls.crt"]' | openssl base64 -d -A > "${STATE}/tls.crt"
    printf '%s' "${serving_secret}" | jq -er '.data["tls.key"]' | openssl base64 -d -A > "${STATE}/tls.key"
  fi
  if [ -z "${serving_secret}" ] || ! openssl x509 -in "${STATE}/tls.crt" -checkend 86400 -noout; then
    openssl req -x509 -newkey rsa:2048 -nodes -days 7 \
      -keyout "${STATE}/tls.key" -out "${STATE}/tls.crt" \
      -subj '/CN=openshell-kube-apiserver.openshell.svc' \
      -addext 'subjectAltName=DNS:openshell-kube-apiserver.openshell.svc'
  fi
  kube -n openshell create secret tls openshell-kube-apiserver-serving-cert \
    --cert="${STATE}/tls.crt" --key="${STATE}/tls.key" --dry-run=client -o yaml |
    kube apply --server-side --field-manager=openshell-demo-setup -f -
  local ca
  ca="$(base64 < "${STATE}/tls.crt" | tr -d '\n')"
  sed "s#REPLACE_WITH_BASE64_ENCODED_CA#${ca}#" "${MODULE}/deploy/apiserver.yaml" | kube apply -f -
  local image_id cert_hash
  image_id="$(docker image inspect "${IMAGE}" --format '{{.Id}}')"
  cert_hash="$(openssl x509 -in "${STATE}/tls.crt" -noout -fingerprint -sha256 | cut -d= -f2)"
  kube -n openshell patch deployment openshell-kube-apiserver --type=merge \
    -p "$(jq -nc --arg image "${image_id}" --arg cert "${cert_hash}" \
      '{spec:{template:{metadata:{annotations:{"demo.openshell.ai/image-id":$image,"demo.openshell.ai/serving-cert":$cert}}}}}')"
  kube -n openshell rollout status deployment/openshell-kube-apiserver --timeout=180s
  kube wait --for=condition=Available apiservice/v1alpha1.openshell.ai --timeout=120s
  kube -n default create rolebinding openshell-demo \
    --clusterrole=openshell-sandbox-user --user=openshell-demo --dry-run=client -o yaml | kube apply -f -
  kube create clusterrolebinding openshell-demo-workspace-reader \
    --clusterrole=openshell-workspace-reader --user=openshell-demo --dry-run=client -o yaml | kube apply -f -
  local mtls="${STATE}/config/openshell/gateways/demo/mtls"
  mkdir -p "${mtls}"
  local key
  for key in ca.crt tls.crt tls.key; do
    kube -n openshell get secret openshell-client-tls -o json |
      jq -r --arg key "${key}" '.data[$key]' | openssl base64 -d -A > "${mtls}/${key}"
  done
  trap - ERR
  template_demo
  echo "Demo ready: team-a/worker is running; team-b is prepared for interactive deployment."
  echo "Deploy interactively: kubectl --kubeconfig=${KUBECONFIG} apply -f ${MODULE}/deploy/demo-interactive-sandbox.yaml"
}
test_demo() {
  require kubectl jq openshell
  test -s "${KUBECONFIG}"
  trap diagnostics ERR
  for ((attempt=0; attempt<60; attempt++)); do
    if kube get --raw /apis/openshell.ai/v1alpha1 > "${STATE}/discovery.json" 2>/dev/null; then break; fi
    sleep 1
  done
  jq -e '.resources[] | select(.name == "openshellsandboxes")' "${STATE}/discovery.json" >/dev/null
  local resource
  for resource in oss osst osp; do
    kube get "${resource}" -A -o json | jq -e '.items | type == "array"' >/dev/null
  done
  test "$(kube auth can-i create openshellsandboxes.openshell.ai -n default --as=demo-denied || true)" = no
  if kube --as=demo-denied get openshellsandboxes -n default > "${STATE}/denied.log" 2>&1; then
    echo "Unprivileged user unexpectedly read sandboxes" >&2
    exit 1
  fi
  grep -q Forbidden "${STATE}/denied.log"
  user_kube get openshellworkspaces -o json | jq -e 'any(.items[]; .metadata.name == "default")' >/dev/null
  if kube --as=demo-denied get openshellworkspaces > "${STATE}/workspaces-denied.log" 2>&1; then
    echo "Unprivileged user unexpectedly read the workspace catalog" >&2
    exit 1
  fi
  grep -q Forbidden "${STATE}/workspaces-denied.log"
  test "$(kube auth can-i get secrets -n openshell --as=openshell-demo || true)" = no
  for resource in openshellsandboxes openshellproviders openshellsandboxtemplates; do
    kube get "${resource}" -A -o json | jq -e '.items | type == "array"' >/dev/null
    if user_kube get "${resource}" -A > "${STATE}/all-namespaces-denied.log" 2>&1; then
      echo "Namespace-only user unexpectedly listed ${resource} across all namespaces" >&2
      exit 1
    fi
    grep -q Forbidden "${STATE}/all-namespaces-denied.log"
  done
  user_kube -n default get openshellsandboxtemplates,openshellproviders -o json >/dev/null
  for ((attempt=0; attempt<150; attempt++)); do
    if kube get --raw /openapi/v3/apis/openshell.ai/v1alpha1 > "${STATE}/openapi.json" 2>/dev/null; then break; fi
    sleep 2
  done
  jq -e '.components.schemas | to_entries[] | select(.value.properties.spec.properties.desiredState.type == "string")' "${STATE}/openapi.json" >/dev/null
  kube get --raw /openapi/v2 |
    jq -e '.definitions | to_entries[] | select(.value.properties.spec.properties.desiredState.type == "string")' >/dev/null
  sed 's/desiredState: Running/desiredState: 42/' "${MODULE}/deploy/demo-sandbox.yaml" > "${STATE}/invalid.yaml"
  if user_kube apply --validate=strict -f "${STATE}/invalid.yaml" > "${STATE}/invalid.log" 2>&1; then
    echo "Invalid spec type unexpectedly accepted" >&2
    exit 1
  fi
  grep -Ei 'invalid|cannot unmarshal|must be of type' "${STATE}/invalid.log"
  user_kube apply --validate=strict -f "${MODULE}/deploy/demo-sandbox.yaml"
  user_kube apply --validate=strict -f "${MODULE}/deploy/demo-sandbox.yaml"
  sed 's/name: demo$/name: demo-ssa/' "${MODULE}/deploy/demo-sandbox.yaml" > "${STATE}/server-applied.yaml"
  user_kube apply --server-side --field-manager=demo --validate=strict -f "${STATE}/server-applied.yaml"
  user_kube apply --server-side --field-manager=demo --validate=strict -f "${STATE}/server-applied.yaml"
  kube get openshellsandboxes -A -l app=openshell-api-demo -o json |
    jq -e '(.items | length == 2) and all(.items[]; .metadata.namespace == "default")' >/dev/null
  for ((attempt=0; attempt<150; attempt++)); do
    user_kube get openshellsandbox demo -n default -o json > "${STATE}/sandbox.json"
    if jq -e '.status.phase == "Ready"' "${STATE}/sandbox.json" >/dev/null; then break; fi
    if jq -e '.status.phase == "Error"' "${STATE}/sandbox.json" >/dev/null; then
      cat "${STATE}/sandbox.json" >&2
      return 1
    fi
    sleep 2
  done
  jq -e '.status.phase == "Ready" and ([.status.conditions[] | select(.type == "Ready" and .status == "True")] | length > 0)' "${STATE}/sandbox.json" >/dev/null
  local forward_pid
  : > "${STATE}/port-forward.log"
  kubectl --kubeconfig="${KUBECONFIG}" -n openshell port-forward svc/openshell :8080 > "${STATE}/port-forward.log" 2>&1 &
  forward_pid=$!
  trap 'kill "${forward_pid}" 2>/dev/null || true' EXIT
  for ((attempt=0; attempt<50; attempt++)); do
    grep -q 'Forwarding from 127.0.0.1:' "${STATE}/port-forward.log" && break
    kill -0 "${forward_pid}" 2>/dev/null || { cat "${STATE}/port-forward.log" >&2; exit 1; }
    sleep 0.2
  done
  PORT="$(sed -n 's/^Forwarding from 127.0.0.1:\([0-9]*\).*/\1/p' "${STATE}/port-forward.log" | head -1)"
  test -n "${PORT}"
  local id
  id="$(jq -r '.metadata.uid' "${STATE}/sandbox.json")"
  cli sandbox get demo --output json > "${STATE}/gateway-sandbox.json"
  jq -e --arg id "${id}" '.. | objects | select(.id? == $id)' "${STATE}/gateway-sandbox.json" >/dev/null
  test "$(cli sandbox exec demo --no-tty --no-login-shell --timeout 30 -- cat /sandbox/demo-marker)" = real-gateway-demo
  kube -n openshell get sandboxes.agents.x-k8s.io -l openshell.ai/sandbox-id="${id}" -o json |
    jq -e '.items | length == 1' >/dev/null
  user_kube -n default get openshellsandboxes -l app=openshell-api-demo -o json |
    jq -e '.items | length == 2' >/dev/null
  kube -n openshell rollout restart deployment/openshell-kube-apiserver
  kube -n openshell rollout status deployment/openshell-kube-apiserver --timeout=120s
  for ((attempt=0; attempt<60; attempt++)); do
    if user_kube -n default get openshellsandbox demo -o json --show-managed-fields > "${STATE}/after-restart.json"; then break; fi
    sleep 1
  done
  jq -e --arg id "${id}" '.metadata.uid == $id' "${STATE}/after-restart.json" >/dev/null
  jq -e '[.metadata.managedFields[] | select(.manager == "kubectl-client-side-apply")] | length > 0' "${STATE}/after-restart.json" >/dev/null
  user_kube apply --validate=strict -f "${MODULE}/deploy/demo-sandbox.yaml"
  user_kube apply --server-side --field-manager=demo --validate=strict -f "${STATE}/server-applied.yaml"
  user_kube get openshellsandbox demo-ssa -n default -o json --show-managed-fields |
    jq -e '[.metadata.managedFields[] | select(.manager == "demo" and .operation == "Apply")] | length > 0' >/dev/null
  sed 's/app: openshell-api-demo/app: conflicting-manager/' "${STATE}/server-applied.yaml" > "${STATE}/conflict.yaml"
  if user_kube apply --server-side --field-manager=other --validate=strict -f "${STATE}/conflict.yaml" > "${STATE}/conflict.log" 2>&1; then
    echo "Conflicting field owner unexpectedly accepted" >&2
    exit 1
  fi
  grep -qi conflict "${STATE}/conflict.log"
  if user_kube -n default patch openshellsandbox demo --type=merge \
    -p '{"metadata":{"labels":{"compatibility-probe":"unsupported"}}}' > "${STATE}/metadata-update.log" 2>&1; then
    echo "The pinned gateway unexpectedly accepted an unsupported metadata update" >&2
    exit 1
  fi
  grep -q 'gateway does not support SandboxUpdates.UpdateSandbox' "${STATE}/metadata-update.log"
  user_kube -n default get openshellsandbox demo -o json |
    jq -e '.metadata.labels["compatibility-probe"] == null' >/dev/null
  local ssa_id runtime_selector
  ssa_id="$(user_kube get openshellsandbox demo-ssa -n default -o jsonpath='{.metadata.uid}')"
  runtime_selector="openshell.ai/sandbox-id in (${id},${ssa_id})"
  user_kube -n default delete openshellsandbox demo-ssa --timeout=120s
  user_kube -n default delete openshellsandbox demo --timeout=120s
  for ((attempt=0; attempt<60; attempt++)); do
    kube -n openshell get sandboxes.agents.x-k8s.io -l "${runtime_selector}" -o json > "${STATE}/runtime.json"
    kube -n openshell get pods -l "${runtime_selector}" -o json > "${STATE}/pods.json"
    if jq -e '.items | length == 0' "${STATE}/runtime.json" >/dev/null &&
      jq -e '.items | length == 0' "${STATE}/pods.json" >/dev/null; then break; fi
    sleep 2
  done
  jq -e '.items | length == 0' "${STATE}/runtime.json" >/dev/null
  jq -e '.items | length == 0' "${STATE}/pods.json" >/dev/null
  user_kube -n default get openshellsandboxes -l app=openshell-api-demo -o json |
    jq -e '.items | length == 0' >/dev/null
  if cli sandbox get demo > "${STATE}/deleted.log" 2>&1; then
    echo "Gateway still returns the deleted sandbox" >&2
    exit 1
  fi
  grep -qi 'not found' "${STATE}/deleted.log"
  kill "${forward_pid}"
  wait "${forward_pid}" 2>/dev/null || true
  trap - EXIT ERR
  echo "PASS: strict validation, client/server apply, ownership conflict, update compatibility, Ready, workload exec, RBAC, restart persistence, and delete."
}

template_demo() {
  require kubectl jq openshell
  test -s "${KUBECONFIG}"
  trap diagnostics ERR
  local forward_pid
  : > "${STATE}/template-port-forward.log"
  kubectl --kubeconfig="${KUBECONFIG}" -n openshell port-forward svc/openshell :8080 > "${STATE}/template-port-forward.log" 2>&1 &
  forward_pid=$!
  trap 'kill "${forward_pid}" 2>/dev/null || true' EXIT
  for ((attempt=0; attempt<50; attempt++)); do
    grep -q 'Forwarding from 127.0.0.1:' "${STATE}/template-port-forward.log" && break
    kill -0 "${forward_pid}" 2>/dev/null || { cat "${STATE}/template-port-forward.log" >&2; exit 1; }
    sleep 0.2
  done
  PORT="$(sed -n 's/^Forwarding from 127.0.0.1:\([0-9]*\).*/\1/p' "${STATE}/template-port-forward.log" | head -1)"
  test -n "${PORT}"

  echo "1. Prepare team-a and team-b workspaces, namespaces, access, and templates."
  local workspace
  for workspace in team-a team-b; do
    if ! cli workspace get "${workspace}" > "${STATE}/${workspace}-workspace.log" 2>&1; then
      grep -qi 'not found' "${STATE}/${workspace}-workspace.log" || { cat "${STATE}/${workspace}-workspace.log" >&2; exit 1; }
      cli workspace create --name "${workspace}"
    fi
    kube create namespace "${workspace}" --dry-run=client -o yaml | kube apply -f -
    kube -n "${workspace}" create rolebinding openshell-demo \
      --clusterrole=openshell-sandbox-user --user=openshell-demo --dry-run=client -o yaml | kube apply -f -
    user_kube get openshellworkspace "${workspace}" -o json |
      jq -e '.metadata.namespace == null and .status.phase == "Active"' >/dev/null
    if ! cli --workspace "${workspace}" sandbox template get ubuntu-demo > "${STATE}/${workspace}-template.log" 2>&1; then
      grep -qi 'not found' "${STATE}/${workspace}-template.log" || { cat "${STATE}/${workspace}-template.log" >&2; exit 1; }
      cli --workspace "${workspace}" sandbox template create ubuntu-demo \
        --image nvcr.io/nvidia/base/ubuntu:24.04 --env DEMO_MESSAGE=from-workload-template
    fi
    user_kube -n "${workspace}" get openshellsandboxtemplate ubuntu-demo -o json > "${STATE}/${workspace}-template.json"
    jq -e '.spec.workload.image == "nvcr.io/nvidia/base/ubuntu:24.04" and
      .spec.workload.environment.DEMO_MESSAGE == "from-workload-template"' "${STATE}/${workspace}-template.json" >/dev/null
    test "$(kube auth can-i create openshellsandboxes.openshell.ai -n "${workspace}" --as=openshell-demo)" = yes
    test "$(kube auth can-i watch openshellsandboxes.openshell.ai -n "${workspace}" --as=openshell-demo)" = yes
  done
  local interactive_before
  interactive_before="$(user_kube -n team-b get openshellsandboxes -o json | jq -c '[.items[].metadata.uid] | sort')"

  echo "2. Deploy team-a/worker; leave team-b sandboxes untouched for interactive use."
  user_kube apply --validate=strict -f "${MODULE}/deploy/demo-template-sandbox.yaml"
  user_kube apply --validate=strict -f "${MODULE}/deploy/demo-template-sandbox.yaml"
  for ((attempt=0; attempt<150; attempt++)); do
    user_kube -n team-a get openshellsandbox worker -o json > "${STATE}/template-sandbox.json"
    if jq -e '.status.phase == "Ready"' "${STATE}/template-sandbox.json" >/dev/null; then break; fi
    if jq -e '.status.phase == "Error"' "${STATE}/template-sandbox.json" >/dev/null; then
      cat "${STATE}/template-sandbox.json" >&2
      exit 1
    fi
    sleep 2
  done
  jq -e '.status.phase == "Ready" and .spec.workloadTemplate == "ubuntu-demo" and
    .spec.template == null' "${STATE}/template-sandbox.json" >/dev/null
  local id
  id="$(jq -r '.metadata.uid' "${STATE}/template-sandbox.json")"
  cli --workspace team-a sandbox get worker --output json > "${STATE}/template-gateway-sandbox.json"
  jq -e --arg id "${id}" '.. | objects | select(.id? == $id)' "${STATE}/template-gateway-sandbox.json" >/dev/null
  test "$(cli --workspace team-a sandbox exec worker --no-tty --no-login-shell --timeout 30 -- cat /sandbox/demo-marker)" = from-workload-template
  kube get openshellsandboxes -A -l app=openshell-template-demo -o json |
    jq -e 'any(.items[]; .metadata.namespace == "team-a" and .metadata.name == "worker")' >/dev/null
  test "$(user_kube -n team-b get openshellsandboxes -o json | jq -c '[.items[].metadata.uid] | sort')" = "${interactive_before}"
  user_kube -n team-a get openshellsandboxtemplate,openshellsandbox
  user_kube -n team-b get openshellsandboxtemplate,openshellsandbox
  kill "${forward_pid}"
  wait "${forward_pid}" 2>/dev/null || true
  trap - EXIT ERR
  echo "PASS: both workspaces/templates/access, team-a workload exec and unchanged reapply, and team-b sandboxes preserved."
  echo "Resources retained in team-a and team-b; only team-a/worker is deployed automatically."
}

case "${1:-}" in
  up) up ;;
  test) test_demo ;;
  template-demo) template_demo ;;
  down)
    test -s "${KUBECONFIG}"
    kind delete cluster --name "${CLUSTER}" --kubeconfig "${KUBECONFIG}"
    ;;
  *) echo "Usage: $0 {up|test|template-demo|down}" >&2; exit 2 ;;
esac
