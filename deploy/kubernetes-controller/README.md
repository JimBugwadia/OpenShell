# OpenShell Kubernetes controller prototype

This package contains the namespaced RBAC and a sample `OpenShellSandboxTemplate`
and `OpenShellSandbox`. The
controller is run from the source checkout with Cargo; it intentionally has no
Deployment or container image because this prototype does not publish a
controller image. Install its CRD from the executable so the served schema and
the controller stay in sync; `--print-crd` prints both CRDs.

## Requirements

- A Kubernetes cluster and `kubectl` context that you intend to use.
- Rust toolchain and Cargo from the repository's `rust-toolchain.toml`.
- An OpenShell gateway reachable from the controller process.
- A controller identity that is a workspace admin in the gateway workspace, so
  it can publish template revisions. A sandbox can instead name a workload
  template that already exists in the gateway.

The controller watches only the namespace, workspace, and gateway URL passed
at startup. Configure one controller process per binding. It uses the current
Kubernetes context through the default kubeconfig/in-cluster configuration.

## Install and run

Choose the namespace, gateway workspace, and gateway URL; replace the example
values with real ones. The sample declares the template `support-agent` and a
sandbox that uses it.

```shell
export KUBE_CONTEXT="$(kubectl config current-context)"
export CONTROLLER_NAMESPACE=openshell-sandbox
export GATEWAY_WORKSPACE=support-engineering
export GATEWAY_URL=https://gateway.example.test

kubectl --context "$KUBE_CONTEXT" create namespace "$CONTROLLER_NAMESPACE" \
  --dry-run=client -o yaml | kubectl --context "$KUBE_CONTEXT" apply -f -

cargo run --locked -p openshell-k8s-controller -- --print-crd \
  | kubectl --context "$KUBE_CONTEXT" apply -f -
kubectl --context "$KUBE_CONTEXT" wait \
  --for=condition=Established \
  --timeout=120s \
  crd/openshellsandboxes.openshell.ai

kubectl --context "$KUBE_CONTEXT" apply \
  -f deploy/kubernetes-controller/rbac.yaml
```

The included RoleBinding grants the service account namespaced `list`, `watch`,
and `patch` access to `OpenShellSandbox` objects and `patch` access to their
status subresource. It grants no permissions outside
`openshell-sandbox`, and does not grant create/delete or access to other
resources. The CRD is cluster-scoped; installing or upgrading it requires a
separate administrator identity. Do not grant the controller cluster-wide
permissions to install the CRD.

For host development, run the process with a kubeconfig identity granted the
same Role permissions. The packaged service account and RoleBinding are ready
for an in-cluster deployment, but this prototype intentionally supplies no
Deployment or image. To use the sample RoleBinding in a future in-cluster
runtime, set that runtime's service account to `openshell-k8s-controller`.

Configure gateway credentials with the options supported by the binary:

```shell
cargo run --locked -p openshell-k8s-controller -- \
  --namespace "$CONTROLLER_NAMESPACE" \
  --workspace "$GATEWAY_WORKSPACE" \
  --gateway "$GATEWAY_URL" \
  --token-file "$GATEWAY_TOKEN_FILE" \
  --tls-ca "$GATEWAY_CA_FILE" \
  --tls-cert "$GATEWAY_CLIENT_CERT" \
  --tls-key "$GATEWAY_CLIENT_KEY"
```

`--token-file` is optional and contains the gateway bearer token. `--tls-ca`
is optional. For mTLS, pass both `--tls-cert` and `--tls-key`; the CLI rejects
an unpaired certificate or key. Omit optional flags when the gateway does not
require them. Keep tokens and private keys out of Kubernetes custom resources,
manifests, command history, and logs.

In another terminal, create the custom resource and inspect its gateway
lifecycle status:

```shell
kubectl --context "$KUBE_CONTEXT" apply \
  -f deploy/kubernetes-controller/example-openshell-sandbox.yaml
kubectl --context "$KUBE_CONTEXT" get openshellsandboxes \
  --namespace "$CONTROLLER_NAMESPACE" --watch
kubectl --context "$KUBE_CONTEXT" wait --for=condition=Ready \
  --namespace "$CONTROLLER_NAMESPACE" \
  openshellsandbox/support-agent-demo
```

You can use the short names `ossbx` for `openshellsandboxes` and `ossbxt` for
`openshellsandboxtemplates`. The list
shows the Phase, Reason, Gateway Phase, and Age columns. The top-level status
fields `phase`, `reason`, `conditions`, `sandboxName`, `sandboxId`, and
`templateRevision`, and `gatewayPhase` are for you. A provisioned sandbox stays `Ready` while it is
stopped, suspended, or completed; the Gateway Phase column shows its runtime
phase. Start the controller with `--report-gateway-phase false` to omit the
runtime phase, so sleeping and waking cause no status writes.
`status.journal` holds the controller's recovery
journal, including request IDs, attempt counters, and RFC 3339 retry
timestamps; don't edit it.

The sandbox spec is immutable; create a new resource to request a different
template. `workloadTemplate` names an `OpenShellSandboxTemplate` in the same
namespace or, if none exists, a gateway template. The controller resolves it
once, before the first submission, and records the gateway revision in
`templateRevision`, so later template changes don't affect the sandbox.

A template's spec is mutable. The controller publishes each generation as an
immutable gateway template named `<name>-r<generation>`, labeled with its
family and generation, and reports it in `status.currentRevision`. Callers
that create sandboxes on demand can name the family, such as
`openshell sandbox create --template support-agent`, and the gateway uses the
latest revision. Deleting the template resource deletes its revisions;
sandboxes already created from them keep the workload they started with. The CR intentionally
has no command or restart-policy overrides; gateway/template defaults apply.
A `Ready` condition with status `True` means the gateway reports phase
`Ready`; it is not an application-level health or readiness check. The
condition's `lastTransitionTime` records only when its status last changed, so
a recorded `Ready` value is not proof that the controller is currently healthy
or observing the gateway.

Delete the resource to request gateway deletion and let the finalizer complete:

```shell
kubectl --context "$KUBE_CONTEXT" delete openshellsandbox support-agent-demo \
  --namespace "$CONTROLLER_NAMESPACE"
```

## Prototype constraints

- Run one controller replica for a single namespace/workspace/gateway binding.
  Leader election and multiple-replica coordination are not implemented.
- Retries replay the same deterministic request ID with exponential backoff
  (30 seconds, doubling to a one-hour cap) for at most 10 attempts and within a
  23-hour window. While retrying, the resource reports `Progressing`. Exhausted
  retries block the resource; they do not imply successful recovery.
- A gateway refusal, such as a missing workload template, blocks the resource
  with reason `CreateRejected`. Delete the resource and create a corrected one;
  deletion completes without manual finalizer removal.
- The controller never adopts an existing gateway sandbox, recreates a
  missing/stopped sandbox, starts an explicitly stopped sandbox, or overwrites
  explicit gateway lifecycle state. Gateway errors and uncertain outcomes may
  leave a resource blocked for operator action.
- List the controller's identity as the sandbox manager of its workspace in
  the gateway's `[openshell.gateway.sandbox_managers]` table, so only the
  controller can change that workspace's sandboxes. Without an entry, any
  authorized caller can.
- The gateway's `DeleteSandbox` API has no ID/version precondition. The
  controller checks the recorded sandbox ID immediately before deletion, but
  cannot close the race with a concurrent external replacement. Reserve the
  controller's `os-<16-hex-UUID-prefix>` sandbox names and do not allow other
  writers with the same gateway permissions to replace those records. The full
  Kubernetes UID remains in status and the request IDs; name-prefix collisions
  fail closed rather than adopting or deleting a record.
- These states block deletion until you act: `CreateReplayExhausted` or
  `DeleteReplayExhausted` (retries ran out), `NameCollision` after an uncertain
  create attempt, and `BindingChanged` (the controller's gateway or workspace
  flags changed). Check the gateway sandbox named in `status.sandboxName`
  before you remove the finalizer.
- The finalizer waits for gateway absence. If it must be removed manually,
  first verify and clean up the corresponding gateway sandbox yourself.
  Removing it while the remote sandbox still exists can orphan that sandbox;
  this procedure is not a recovery guarantee.
- The gateway remains the lifecycle owner. This CRD adds no gateway API and
  does not add recovery/adoption behavior for sandboxes created outside this
  controller.

## Live API-server validation

Run the focused validation against a newly created, isolated kind cluster:

```shell
tasks/scripts/test-k8s-controller-kind.sh
```

The script refuses to reuse an existing cluster named
`openshell-crd-prototype`; it creates that cluster, applies the generated CRD
and scoped RBAC, checks live schema/admission behavior and RBAC, then deletes
only the cluster it created. It does not modify the current kubectl context or
any pre-existing cluster. This validates the Kubernetes API server contract;
it does not install a gateway.

## Live gateway smoke test

For a real controller lifecycle, use an existing development gateway and an
existing workload template in its configured workspace. The namespace must
already exist and be dedicated and empty: no existing `OpenShellSandbox`
resources and no other controller watching it. The smoke script checks for
existing resources and exits before building or launching the controller if
the namespace is not empty. The kubeconfig's current context must match
`KUBE_CONTEXT` because the controller uses the kubeconfig default context.

```shell
export KUBE_CONTEXT=kind-openshell-dev
export OPENSHELL_CONTROLLER_NAMESPACE=openshell-sandbox
export OPENSHELL_CONTROLLER_GATEWAY=http://127.0.0.1:18080
export OPENSHELL_CONTROLLER_WORKSPACE=default
export OPENSHELL_CONTROLLER_TEMPLATE=controller-smoke

cargo build --locked -p openshell-cli
target/debug/openshell sandbox template create "$OPENSHELL_CONTROLLER_TEMPLATE" \
  --image python:3.13-alpine \
  --gateway-endpoint "$OPENSHELL_CONTROLLER_GATEWAY" \
  --workspace "$OPENSHELL_CONTROLLER_WORKSPACE"

# Optional: run a command inside the ready sandbox using the CLI.
export OPENSHELL_CONTROLLER_CLI=target/debug/openshell
tasks/scripts/test-k8s-controller-gateway-smoke.sh
```

The script creates a uniquely named CR, waits for its `Ready` condition, runs
`sh -c 'printf "controller-workload-ok\n"'` when
`OPENSHELL_CONTROLLER_CLI` is set, then deletes the CR and waits for finalizer
completion. It leaves the finalizer in place and prints a warning if cleanup
cannot finish; do not remove it until gateway-side cleanup has been verified.
Optional controller authentication inputs are `OPENSHELL_CONTROLLER_TOKEN_FILE`,
`OPENSHELL_CONTROLLER_TLS_CA`, and the paired
`OPENSHELL_CONTROLLER_TLS_CERT`/`OPENSHELL_CONTROLLER_TLS_KEY`.

Run the mocked namespace-guard regression test without a cluster:

```shell
tasks/scripts/test-k8s-controller-gateway-smoke-guard.sh
```

The successful source-checkout smoke in this worktree used the chart and Rust
controller built from this checkout, gateway/supervisor/sandbox-runtime images
tagged `0.1.6`, and the `python:3.13-alpine` workload image. It reached gateway
phase `Ready`, executed a Python command through `openshell sandbox exec`, and
completed gateway deletion through the CR finalizer. This verifies the current
SDK/controller wire contract against the `0.1.6` gateway image; it is not a
same-checkout gateway image build or a claim that every gateway version is
compatible. That smoke predates the `Ready` condition and the
`status.journal` layout. The generated CRD no longer uses the unrecognized
`uint32` format, and the kind validation applies it without warnings.
