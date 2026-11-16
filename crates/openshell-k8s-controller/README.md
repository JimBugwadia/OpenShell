# Experimental Kubernetes controller

This optional, single-replica controller translates namespaced
`openshell.ai/v1alpha1` `OpenShellSandbox` resources into existing gateway gRPC
create, observe, and delete calls. The gateway remains the lifecycle owner.
It does not provision Kubernetes workloads directly.

```shell
cargo run -p openshell-k8s-controller -- --print-crd
cargo run -p openshell-k8s-controller -- \
  --namespace agents --workspace default --gateway https://gateway.example:8080 \
  --tls-ca ca.crt --tls-cert controller.crt --tls-key controller.key
```

`--print-crd` works without Kubernetes or gateway connections. Install its output
as a CRD before running the controller. Kubernetes 1.29 or newer is the prototype
baseline. Use one controller replica and one controller instance per namespace;
there is no leader election or HA support.

```yaml
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: agents
spec:
  workloadTemplate: support-agent-v2
```

The named workload template must already exist in the gateway workspace selected
by `--workspace`. The controller does not create templates, policies, or providers.
Gateway default governance applies; workload templates do not select policy or
credentials. The entire spec is immutable through a CEL admission transition
rule. Reapplying equivalent content is allowed. Change the workload by creating a
new resource, not by updating an existing resource.

The trusted flags bind exactly one namespace to one gateway URL and workspace.
Resources cannot override this mapping or supply credentials. Optional
`--token-file` reads a static bearer token; `--tls-ca` pins a CA, and
`--tls-cert`/`--tls-key` supply an mTLS identity. Mount these from administrator-owned
Secrets; token refresh/reload is not implemented. Changing identity or binding
while operations are outstanding is unsupported.

## Lifecycle and failure handling

- The finalizer is acknowledged before any gateway submission. The
  `status.journal` recovery journal records the source UID, name, and
  namespace, the exact gateway and workspace binding, the immutable template,
  operation IDs, attempt counters, and retry timestamps.
- Gateway names are `os-<first 16 source UUID hex characters>`, fitting the
  gateway's 19-character routable-name limit. The full source UID remains in
  status and operation identity; short-name collisions fail closed rather than
  adopting an existing sandbox. Kubernetes UIDs must parse as UUIDs.
  Create/delete request UUIDs are deterministic and independent of generation.
- The controller persists each attempt before making its bounded, 30-second RPC.
  It recovers ambiguous results by replaying the same request, including after
  controller restart and deletion during submission. Retries back off
  exponentially from 30 seconds to a one-hour cap, so ten attempts span about
  three hours. Replay stops after ten attempts or 23 hours, before the
  gateway's 24-hour success replay window. Exhaustion retains the finalizer and
  requires operator investigation.
- Validation, missing-template, and authorization refusals (`InvalidArgument`,
  `NotFound`, `FailedPrecondition`, `PermissionDenied`, `Unauthenticated`)
  block the resource with reason `CreateRejected` and are not retried.
- Status records `createMayExist` before each create submission. Deleting a
  resource whose every create attempt was rejected or collided completes
  without a gateway call. After any uncertain attempt, deletion first recovers
  the receipt by replay, and a later refusal is treated as uncertain.
- Successful create receipts, not lookup results or labels, establish the
  recorded gateway sandbox ID. Collisions and ID mismatches fail closed.
- Accepted sandboxes are never recreated, started, or updated. Explicit stops,
  completion, suspension, and missing records remain observable.
- Status exposes `phase` (`Ready`, `Progressing`, `Blocked`), a fixed safe
  `reason`, a `Ready` condition, `observedGeneration`, `sandboxName`,
  `sandboxId`, and `gatewayPhase`. `Ready` means only that the gateway reports
  Ready; no application health is inferred. Raw gateway errors and workload
  details are not copied into status or logs. `Progressing` includes
  automatic retries such as `CreateUncertain`, `DeleteUncertain`, and
  `ObservationUnavailable`; `Blocked` means the controller needs you to act.
- Delete admission can precede asynchronous cleanup. The finalizer remains
  until the gateway has accepted a delete and a later lookup reports the
  record absent. A lookup that finds nothing isn't enough on its own, because
  the gateway also reports records the controller can no longer access as not
  found; the controller submits the delete first, which fails for those.
  Equivalent status is not rewritten; writes use Kubernetes resource-version
  preconditions and preserve other finalizers.

## Status

Top-level fields are for users. `status.journal` is the controller's recovery
journal; don't edit it. Timestamps use RFC 3339.

```yaml
status:
  observedGeneration: 1
  phase: Ready
  reason: GatewayReady
  conditions:
    - type: Ready
      status: "True"
      reason: GatewayReady
      message: The gateway reports the sandbox phase as Ready.
      lastTransitionTime: "2026-11-13T18:04:12Z"
      observedGeneration: 1
  sandboxName: os-fd229996bc63413e
  sandboxId: 3b0c6f9e-2f44-4d7a-9a51-6f1f1c2b8e10
  gatewayPhase: Ready
  journal:
    binding:
      sourceUid: fd229996-bc63-413e-9dc0-cc5f05abe5bd
      sourceName: support-agent
      namespace: agents
      gateway: https://gateway.example:8080
      workspace: default
      sandboxName: os-fd229996bc63413e
      createRequestId: fd229996-bc63-413e-9dc0-cc5f05abe5bd
      deleteRequestId: fd229996-bc63-413e-9dc0-cc5f05abe5bc
      createStartedAt: "2026-11-13T18:03:58Z"
      workloadTemplate: support-agent-v2
    createAttempts: 1
    deleteAttempts: 0
    deleteStartedAt: null
    deleteAccepted: false
    retryAfter: null
```

The controller derives the `Ready` condition from `phase` and `reason` on
every status write. The condition is `True` when the phase is `Ready`,
`Unknown` when a create, delete, or observation result is uncertain, and
`False` otherwise. `lastTransitionTime` changes only when the condition status
changes. The condition message is fixed text selected by the reason.

`kubectl get openshellsandboxes` (short name `ossbx`) shows the Phase, Reason,
Gateway Phase, and Age columns. To wait for readiness, run:

```shell
kubectl wait --for=condition=Ready openshellsandbox/support-agent -n agents
```

## Sandbox managers

Run the controller against a gateway whose `sandbox_managers` setting lists
only the controller's identity, for example `mtls:<certificate CN>` or
`oidc:<token sub>`. The gateway then rejects sandbox changes from every other
caller, so each sandbox has one writer, while the CLI and SDK can still list
sandboxes, read logs, and exec. See `sandbox_managers` in the gateway
configuration reference. If the controller isn't a listed manager, creates fail
with reason `CreateRejected`. When several controllers share a gateway, each one can
change only the sandboxes it created. The gateway refuses to start if you change
the list while sandboxes exist.

Each gateway sandbox carries `kubernetes.openshell.ai/namespace`, `name`, and
`uid` annotations that point back to its resource. They are informational; the
controller never infers ownership from them.

## Safety limits

Without `sandbox_managers`, this is not gateway-enforced ownership. Existing public `DeleteSandbox` accepts
only a name and has no caller-supplied ID/resource-version precondition. The
controller checks the recorded ID immediately before deletion, but a concurrent
writer could replace the object between lookup and delete. Use a dedicated
workspace and exclusive gateway write access, reserve the `os-` name prefix,
and keep the same authorized controller identity. Gateway NotFound responses can
mask authorization failures; stable workspace permissions are also required.
This prototype must not be used where untrusted users can mutate gateway
resources in its workspace, modify controller status/finalizers, or replace its
credentials. Kubernetes status is a recovery journal, not an authorization boundary.
Do not manually remove a finalizer without resolving outstanding gateway work.

No automatic gateway recovery, mutable specs, adoption, multi-gateway mapping,
raw credentials in the CR, or server ownership enforcement is implemented.

## Verification

```shell
cargo fmt -p openshell-k8s-controller -- --check
cargo test -p openshell-k8s-controller
cargo clippy -p openshell-k8s-controller --all-targets -- -D warnings
```

Focused mock tests exercise durable lost-response recovery, request replay,
collisions, stopped/missing records, persistence failures, deletion during
submission, asynchronous cleanup, and replay-window exhaustion. They do not
replace a live Kubernetes/gateway smoke test.
