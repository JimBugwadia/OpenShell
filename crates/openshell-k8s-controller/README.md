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

- The finalizer is acknowledged before any gateway submission. Status records
  the source UID/name/namespace, exact gateway/workspace binding, immutable
  template, operation IDs, and gateway receipt.
- Gateway names are `os-<first 16 source UUID hex characters>`, fitting the
  gateway's 19-character routable-name limit. The full source UID remains in
  status and operation identity; short-name collisions fail closed rather than
  adopting an existing sandbox. Kubernetes UIDs must parse as UUIDs.
  Create/delete request UUIDs are deterministic and independent of generation.
- The controller persists each attempt before making its bounded, 30-second RPC.
  It recovers ambiguous results by replaying the same request, including after
  controller restart and deletion during submission. Replay stops after ten
  attempts or 23 hours, before the gateway's 24-hour success replay window.
  Exhaustion retains the finalizer and requires operator investigation.
- Successful create receipts, not lookup results or labels, establish the
  recorded gateway sandbox ID. Collisions and ID mismatches fail closed.
- Accepted sandboxes are never recreated, started, or updated. Explicit stops,
  completion, suspension, and missing records remain observable.
- Status exposes `phase` (`Ready`, `Progressing`, `Blocked`), a fixed safe `reason`,
  `observedGeneration`, and the gateway phase. `Ready` means only that the
  gateway reports Ready; no application health is inferred. Raw gateway errors
  and workload details are not copied into status or logs.
- Delete admission can precede asynchronous cleanup. The finalizer remains
  until a subsequent authorized gateway lookup reports the record absent.
  Equivalent status is not rewritten; writes use Kubernetes resource-version
  preconditions and preserve other finalizers.

## Safety limits

This is not gateway-enforced ownership. Existing public `DeleteSandbox` accepts
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
