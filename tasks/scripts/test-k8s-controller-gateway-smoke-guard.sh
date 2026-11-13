#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="${ROOT}/target/k8s-controller-gateway-smoke-guard-test"
if [ -e "${work}" ]; then
  echo "ERROR: refusing to overwrite ${work}." >&2
  exit 2
fi
mkdir -p "${work}/bin"
trap 'rm -rf "${work}"' EXIT

cat >"${work}/bin/kubectl" <<'MOCK_KUBECTL'
#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = "config" ]; then
  printf '%s\n' "${KUBE_CONTEXT}"
  exit 0
fi

if [ "${1:-}" = "--context" ]; then
  shift 2
fi

case "${1:-}" in
  get)
    case "${2:-}" in
      namespace|crd/openshellsandboxes.openshell.ai)
        exit 0
        ;;
      openshellsandboxes)
        printf '%s\n' "kubectl list OpenShellSandbox" >>"${MOCK_LOG}"
        if [ "${MOCK_EXISTING_CR:-0}" = "1" ]; then
          printf '%s\n' "openshellsandbox/preexisting"
        fi
        exit 0
        ;;
      openshellsandbox/*)
        printf '%s\n' "phase=Ready gatewayPhase=Ready sandboxName=os-0123456789abcdef sandboxId=00000000-0000-0000-0000-000000000000"
        exit 0
        ;;
    esac
    ;;
  auth)
    exit 0
    ;;
  create)
    cat >/dev/null
    printf '%s\n' "kubectl create" >>"${MOCK_LOG}"
    exit 0
    ;;
  wait)
    printf '%s\n' "kubectl wait" >>"${MOCK_LOG}"
    exit 0
    ;;
  delete)
    printf '%s\n' "kubectl delete" >>"${MOCK_LOG}"
    exit 0
    ;;
esac

echo "unexpected kubectl invocation: $*" >&2
exit 1
MOCK_KUBECTL

cat >"${work}/bin/cargo" <<'MOCK_CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "cargo build" >>"${MOCK_LOG}"
MOCK_CARGO

cat >"${work}/controller" <<'MOCK_CONTROLLER'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "controller launch" >>"${MOCK_LOG}"
exec sleep 30
MOCK_CONTROLLER
chmod +x "${work}/bin/kubectl" "${work}/bin/cargo" "${work}/controller"

export KUBE_CONTEXT=mock-context
export OPENSHELL_CONTROLLER_NAMESPACE=guard-test
export OPENSHELL_CONTROLLER_GATEWAY=http://127.0.0.1:1
export OPENSHELL_CONTROLLER_WORKSPACE=default
export OPENSHELL_CONTROLLER_TEMPLATE=guard-test
export OPENSHELL_CONTROLLER_BINARY="${work}/controller"
export PATH="${work}/bin:${PATH}"
export MOCK_LOG="${work}/calls.log"

if MOCK_EXISTING_CR=1 bash "${ROOT}/tasks/scripts/test-k8s-controller-gateway-smoke.sh" \
  >"${work}/existing.out" 2>&1; then
  echo "FAIL: smoke script accepted a namespace containing an OpenShellSandbox." >&2
  exit 1
fi
if ! grep -Fq "namespace guard-test is not empty" "${work}/existing.out"; then
  echo "FAIL: existing-CR case did not report the namespace guard." >&2
  cat "${work}/existing.out" >&2
  exit 1
fi
if grep -Eq 'cargo build|controller launch|kubectl create' "${MOCK_LOG}"; then
  echo "FAIL: existing-CR case performed work after detecting a non-empty namespace." >&2
  cat "${MOCK_LOG}" >&2
  exit 1
fi

: >"${MOCK_LOG}"
if ! MOCK_EXISTING_CR=0 bash "${ROOT}/tasks/scripts/test-k8s-controller-gateway-smoke.sh" \
  >"${work}/empty.out" 2>&1; then
  echo "FAIL: empty-namespace case did not pass the namespace guard." >&2
  cat "${work}/empty.out" >&2
  exit 1
fi
list_line="$(grep -n -m1 -F 'kubectl list OpenShellSandbox' "${MOCK_LOG}" | cut -d: -f1)"
build_line="$(grep -n -m1 -F 'cargo build' "${MOCK_LOG}" | cut -d: -f1)"
launch_line="$(grep -n -m1 -F 'controller launch' "${MOCK_LOG}" | cut -d: -f1)"
create_line="$(grep -n -m1 -F 'kubectl create' "${MOCK_LOG}" | cut -d: -f1)"
if [ "${list_line}" -ge "${build_line}" ] || \
  [ "${build_line}" -ge "${launch_line}" ] || \
  [ "${launch_line}" -ge "${create_line}" ]; then
  echo "FAIL: empty-namespace guard did not precede build, launch, and resource creation." >&2
  cat "${MOCK_LOG}" >&2
  exit 1
fi

echo "gateway smoke namespace-guard tests passed"
