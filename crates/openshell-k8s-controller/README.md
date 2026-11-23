# Experimental Kubernetes controller

This optional controller translates namespaced
`openshell.ai/v1alpha1` `OpenShellSandbox` resources into existing gateway gRPC
create, observe, and delete calls. The gateway remains the lifecycle owner.
It does not provision Kubernetes workloads directly. For a walkthrough, see the
[Kubernetes sandbox CRD example](../../examples/kubernetes-sandbox-crd/README.md).

```shell
cargo run -p openshell-k8s-controller -- --print-crd
cargo run -p openshell-k8s-controller -- \
  --namespace agents --workspace default --gateway https://gateway.example:8080 \
  --tls-ca ca.crt --tls-cert controller.crt --tls-key controller.key
```

`--print-crd` works without Kubernetes or gateway connections. Install its output
as a CRD before running the controller. Kubernetes 1.29 or newer is the prototype
baseline. Run one controller instance per namespace. To run in the cluster,
install the [Helm chart](../../deploy/helm/openshell-k8s-controller/README.md).

With `--leader-election-lease <name>`, the controllers run only in the replica
that holds that `coordination.k8s.io/v1` Lease, so several replicas can run
with one active. The holder renews it every third of
`--lease-duration-seconds` (default 15), exits if it can't renew before the
Lease expires, and releases it on shutdown so a standby takes over at once.
Every Lease write is conditional on its resource version.

Every `--credential-reload-seconds` (default 10), the controller rereads its
token, CA, and client certificate files. When they change, it connects with
the new credentials and switches to the new connection; if that fails, it
keeps the current connection and tries again. A Secret mounted as a volume
without `subPath` changes atomically, so a rotated certificate takes effect
without a restart.

```yaml
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: support-agent
  namespace: agents
spec:
  image: registry.example.com/support-agent@sha256:9f2c...
  resources:
    cpu: "2"
    memory: 4Gi
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: agents
spec:
  templateRef:
    name: support-agent
```

`templateRef.name` names an `OpenShellSandboxTemplate` in the same
namespace or, if none exists, a workload template that already exists in the
gateway workspace selected by `--workspace`. `templateRef.namespace` is
optional and must match the sandbox's namespace; the controller blocks any
other value with reason `TemplateNamespaceNotAllowed`. Instead of a template,
`workload` describes the image, environment, and resources inline; setting both
is rejected at admission. Omitting both uses the gateway's default image.

The spec also takes `providers` (gateway provider names), `command` (the
agent's start command; the image's own command never runs), `restartPolicy`
(`Never`, `OnFailure`, or `Always`), and `serviceExposures`. A provider name
can be an `OpenShellProvider` in the same namespace or a provider that already
exists in the gateway workspace. `policy` takes a sandbox policy inline, in
the same format as a policy file for `openshell sandbox create --policy`; the
controller validates it before submission and blocks an invalid policy with
reason `PolicyInvalid`. Without `policy`, the gateway's default policy applies.
The controller records these inputs in the journal so a replayed create sends
the original request. The entire sandbox spec is immutable through a CEL
admission transition rule. Reapplying equivalent content is allowed. Change the workload by creating a
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
  block the resource and are not retried. A missing workload template reports
  `TemplateNotFound`, an unknown provider reports `ProviderNotFound`, and other
  refusals report `CreateRejected`.
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
  `sandboxId`, and `gatewayPhase`. Status describes the declaration, not the
  runtime: `Ready` means the gateway provisioned the sandbox, and it stays
  `Ready` with reason `Provisioned` while the sandbox is starting, stopped,
  suspended, or completed, so sleeping and waking never change GitOps health.
  `gatewayPhase` shows the runtime phase. No application health is inferred. Raw gateway errors and workload
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

## Templates

An `OpenShellSandboxTemplate` (short name `ossbxt`) carries the image,
environment, and resources. Its spec is mutable, because gateway templates
aren't: the controller publishes each generation as an immutable gateway
template named `<name>-r<generation>`, with a deterministic request ID, and
labels it `openshell.ai/template-family=<name>`,
`openshell.ai/template-generation=<generation>`, and
`kubernetes.openshell.ai/uid=<resource UID>`. Status reports the published
revision in `currentRevision`, and the `Ready` condition is `True` only for
the current generation.

- A sandbox resolves its template once, before its first submission, and pins
  the revision in its journal and in `status.templateRevision`. A sandbox
  whose template isn't published yet waits without writing status.
- Callers that create sandboxes on demand name the family, and the gateway
  uses the revision with the highest generation; an exact template name
  always wins.
- A revision with the same name but another owner's UID label blocks the
  template with reason `NameCollision`; it's never adopted.
- Deleting the template resource deletes all its revisions. Sandboxes keep the
  workload they were created with. Old revisions otherwise stay until then.
- The controller identity must be a workspace admin to publish templates and,
  in a managed workspace, one of its managers.

## Providers

An `OpenShellProvider` (short name `osprov`) declares a gateway provider. Its
credentials and secret refresh material reference keys in Kubernetes Secrets in
the same namespace, so no value appears in the resource.

```yaml
apiVersion: openshell.ai/v1alpha1
kind: OpenShellProvider
metadata:
  name: github
  namespace: agents
spec:
  type: github
  credentials:
    - name: GITHUB_TOKEN
      secretKeyRef:
        name: github-token
        key: token
```

- The controller creates the gateway provider with the resource's name,
  labeled `kubernetes.openshell.ai/uid=<resource UID>`, so sandboxes list it in
  `providers` by that name. A provider with that name and another owner blocks
  the resource with reason `NameCollision`; it's never adopted.
- `type` names a provider type profile imported into the workspace. Set
  `globalProfile: true` to use the gateway's global catalog instead. Both
  fields are immutable; the rest of the spec is mutable.
- The controller watches the referenced Secrets. Updating a Secret, or the
  spec, applies the provider again: credentials and `config` keys the resource
  no longer declares are removed, and keys set outside the resource are kept.
- `refresh` configures gateway-owned refresh for a credential that the gateway
  mints, with a `strategy` (`OAuth2RefreshToken`, `OAuth2ClientCredentials`,
  `GoogleServiceAccountJwt`, `AwsStsAssumeRole`, or `GithubAppInstallation`)
  and `material`. Each material item sets an inline, non-secret `value` or a
  `secretKeyRef`; only Secret values are sent as secret material. A refreshed
  credential can't also be listed in `credentials`.
- Status records `phase`, `reason`, a `Ready` condition, the resource version
  of each referenced Secret, and the keys the controller set. It never records
  a value. A missing Secret or key blocks the resource with reason
  `SecretNotFound`, and a gateway refusal with `ProviderRejected`.
- Deleting the resource deletes the gateway provider if this resource owns
  it. The gateway refuses while sandboxes use the provider, and the resource
  reports `ProviderInUse` until they're gone.
- The controller needs `get`, `list`, and `watch` on Secrets in its namespace.
  Anyone who can create an `OpenShellProvider` can have the controller send any
  Secret in that namespace to the gateway, so keep unrelated Secrets elsewhere.

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
  templateRevision: support-agent-r1
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
      templateRef:
        name: support-agent-r1
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
Gateway Phase, and Age columns. `gatewayPhase` is informational and never
affects health; start the controller with `--report-gateway-phase false` to
omit it, so a sandbox sleeping and waking causes no status writes. To wait for readiness, run:

```shell
kubectl wait --for=condition=Ready openshellsandbox/support-agent -n agents
```

## Workspace managers

List the controller's identity, for example `mtls:<certificate CN>` or
`oidc:<token sub>`, as a workspace manager of the workspace bound with
`--workspace`, in the gateway's `[openshell.gateway.workspace_managers]` table.
The gateway then rejects changes to sandboxes, workload templates, and
providers in that workspace from every other caller, so each of them has one
writer and a resource, while the CLI and SDK can still list sandboxes, read
logs, and exec. Other workspaces on the same gateway keep their own rules. See
`workspace_managers` in the gateway configuration reference. If the controller
isn't a manager of its workspace, sandbox creates fail with `CreateRejected`,
template publishes with `PublishRejected`, and provider applies with
`ProviderRejected`. When several controllers share a workspace, each one can
change only the sandboxes, templates, and providers it created. The gateway
refuses to start if you change a workspace's list while it has sandboxes,
templates, or providers.

Each gateway sandbox carries `kubernetes.openshell.ai/namespace`, `name`, and
`uid` annotations that point back to its resource. They are informational; the
controller never infers ownership from them.

## Safety limits

Without a `workspace_managers` entry for its workspace, this is not
gateway-enforced ownership. Existing public `DeleteSandbox` accepts
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
