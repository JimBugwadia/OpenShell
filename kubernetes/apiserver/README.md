<!-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# OpenShell Kubernetes API server prototype

This standalone Go module serves `openshell.ai/v1alpha1` through Kubernetes API aggregation. It is an API translation layer: it has no CRD, does not register etcd storage, and treats the OpenShell gateway as authoritative. Kubernetes namespaces map directly to gateway workspace names.

The `APIService` registration makes these resources available to kubectl without
installing `openshell.ai` CRDs. Kubernetes still stores APIService, RBAC, Deployment,
and Secret objects in etcd; sandbox domain records remain in the gateway.

## Prototype scope

- `OpenShellSandbox`: create, get, list, delete, update, and patch, including client-side and server-side apply. The gateway sandbox ID, creation time, resource version, labels, and annotations are reflected in Kubernetes metadata. Create accepts `desiredState: Running` (or omits it); authored input round-trips without gateway-expanded template defaults. Status reflects the live gateway phase.
- `OpenShellSandboxTemplate`: get and list from the gateway `SandboxWorkloadTemplate` resource.
- `OpenShellProvider`: get and list. Credentials, credential expiration details, and credential handles are never serialized.
- `OpenShellWorkspace`: cluster-scoped get and list of gateway workspaces, with `status.phase` (`Active`, `Terminating`, or `Unknown`). No create, update, patch, delete, or watch.
- Unknown workspaces produce an empty list; object operations return a Kubernetes NotFound error that identifies the workspace.
- `kubectl get openshellsandboxes,openshellproviders,openshellsandboxtemplates -A` lists across gateway workspaces and preserves each object's namespace. It requires cluster-wide Kubernetes list permission and the bridge identity's gateway cross-workspace authorization; namespace-only access does not grant it.
- Kubernetes authentication and authorization delegate to the core API server when running in-cluster. Install the supplied auth-delegator ClusterRoleBinding and extension-apiserver-authentication-reader RoleBinding.

Kubectl short names are `oss` (sandboxes), `osst` (sandbox templates), `osp`
(providers), and `osw` (workspaces).

## Discover workspace namespaces

```shell
kubectl get osw
kubectl get osw team-a -o yaml
kubectl auth can-i create openshellsandboxes.openshell.ai -n team-b
```

Each workspace's name is the namespace name used to address its sandboxes,
templates, and providers. The catalog reads the existing gateway directly,
without new state, namespace creation, binding records, or workspace lifecycle
management. `Active` workspaces can accept new resources; `Terminating` is a
gateway lifecycle state, not permission to create.

This is a workspace catalog, not an authorization report. Results are those
visible to the bridge's gateway identity, gated by Kubernetes access to this
cluster-scoped resource; they are not filtered by the caller's namespace RBAC.
Listing a workspace does not grant sandbox access or guarantee its Kubernetes
namespace exists. Check namespace RBAC separately. With the broad demo gateway
identity, catalog readers can see all workspace names and metadata.

The manifest supplies `openshell-workspace-reader` with only `get` and `list`.
Administrators must explicitly bind it at cluster scope; a namespaced RoleBinding
cannot grant access to this catalog. Demo setup binds it only to `openshell-demo`.
No workspace access is granted to all authenticated users.

## API limitations

The prototype does not implement general watches, spec mutation, status or log
subresources, server-side dry-run, stable list pagination/snapshot consistency,
or a gateway change journal. Poll GET to observe readiness; `kubectl get --watch`
and Ready-condition waits are not supported.

`limit`/`continue` use best-effort offset tokens over fresh gateway lists, so
concurrent changes can cause duplicates or omissions. Label selectors are
evaluated locally after fetching the complete gateway list. Sandbox field
selection supports only an exact `metadata.name` within one namespace.
Admission plugins are omitted to avoid requiring core informers.

## Delete and wait

Use ordinary kubectl deletion to acknowledge acceptance before waiting for removal:

```shell
kubectl delete oss demo -n default --timeout=120s
```

DELETE returns once the gateway records `Deleting` or the original sandbox is
gone, without waiting for its delete RPC to finish. Kubectl prints its `deleted`
line, then waits for the original sandbox UID to disappear. That line acknowledges
the request, not completion of cleanup. `--wait=false` skips the wait for removal,
but still waits for acceptance.

Waiting uses a polling watch limited to one namespace and an exact `metadata.name`
selector, with a UID-bearing cursor from a preceding named LIST. Recreating the
same name does not make an active wait track the replacement. This supports
deletion only, not phase/Ready changes, historical events, label watches, or
collection watches. Ordinary list resource versions are not watch cursors.
The caller needs sandbox `get`, `list`, and `watch` permissions as well as `delete`;
the supplied `openshell-sandbox-user` role includes them.

The adapter keeps the gateway delete RPC alive independently of the HTTP request,
bounded to ten minutes, and logs later RPC failures. No deletion queue or new
persistent state is added; the gateway owns the durable `Deleting` record and
recovery. If deletion is rolled back or the API object remains during stalled
cleanup, kubectl keeps waiting until its timeout rather than claiming completion.

Completion means the gateway-backed API object is gone. The published gateway can
remove its record before the delete RPC returns; the adapter cannot promise to
wait for underlying Pod finalizers after that record disappears.

Delete preconditions, explicit grace periods, foreground propagation, and
orphan-dependent deletion are rejected because the gateway delete contract cannot
enforce them. Kubectl's default background propagation is accepted.

## Validation and apply

The server publishes typed OpenAPI v2 and v3 schemas for all four resources and
their lists. Ordinary `kubectl create` and `kubectl apply` work without
`--validate=false`. Kubernetes handles strict decoding, patch processing, and SSA
field ownership; policy JSON and driver-specific envelopes remain opaque in the
schema and retain their existing gateway/conversion validation.

Sandbox creation automatically saves a last-applied annotation when the client
has not supplied one. Plain `kubectl create -f sandbox.yaml` can therefore be
followed by unchanged `kubectl apply -f sandbox.yaml` without an adoption write.
The snapshot contains authored spec and user metadata, not status or server-owned
metadata. Client-supplied last-applied configuration is preserved. The gateway's
8 KiB per-annotation limit applies; oversized snapshots are rejected before create.
This only affects newly created sandboxes; existing objects are not backfilled.
Changed spec or metadata still has the restrictions below.

For example, on a fresh sandbox name:

```shell
kubectl create -f kubernetes/apiserver/deploy/demo-sandbox.yaml
kubectl apply -f kubernetes/apiserver/deploy/demo-sandbox.yaml
```

The apply reports `unchanged`. An older sandbox without the annotation still needs
a metadata update to adopt it into client-side apply. The pinned gateway cannot
perform that update; rerunning `up` does not backfill the annotation.

| Operation | Requirement |
|---|---|
| Client-side or server-side apply creation, unchanged reapply | Works with the published 0.1.6 demo gateway |
| Conflicting SSA field ownership | Returns Kubernetes Conflict without gateway mutation |
| Label/annotation edits, new field ownership, or forced ownership transfer | Requires a gateway exposing `SandboxUpdates.UpdateSandbox`, sandbox updates enabled, and a stopped sandbox |
| Spec edits, finalizers, owner references, server-side dry-run | Rejected; not simulated locally |

The published 0.1.6 demo gateway does not expose the update service. Metadata-changing
apply returns an explicit compatibility error there. With a compatible gateway,
metadata and ownership updates use its resource-version CAS; stale versions return
Kubernetes Conflict. The prototype does not stop or restart sandboxes automatically.

Apply adds bounded persistent metadata to the existing gateway sandbox record,
not to etcd or an adapter database. The reserved gateway annotation
`kubernetes.openshell.ai/prototype-projection` contains the authored spec,
generation, and managed fields; it is hidden from Kubernetes responses. A fresh
adapter restores this state from the gateway. The projection is limited to 8 KiB;
larger inputs or ownership sets are rejected before writes. Other gateway annotation
limits also apply, including to client-side apply's last-applied configuration.
Missing or corrupt projection data is not repaired locally; corrupt data returns
an error. Do not edit the reserved annotation through another gateway client.
Out-of-band gateway spec edits are not reconciled into the stored authored input.
This annotation envelope is a prototype bridge, not the RFC's production metadata
storage contract.

## Build and test

From the repository root:

```shell
go -C kubernetes/apiserver build ./...
go -C kubernetes/apiserver vet ./...
go -C kubernetes/apiserver test -race ./...
gofmt -l kubernetes/apiserver
```

The module replaces `github.com/NVIDIA/OpenShell/sdk/go` with `../../sdk/go` so it uses this checkout's generated gateway protobuf client.

## Run locally

The server requires a serving certificate for API aggregation. Example local flags:

```shell
kubernetes/apiserver/bin/openshell-kube-apiserver \
  --secure-port=8443 \
  --tls-cert-file=certs/tls.crt \
  --tls-private-key-file=certs/tls.key \
  --gateway-address=127.0.0.1:8080 \
  --gateway-ca-file=certs/gateway-ca.crt \
  --gateway-token-file=certs/gateway-token
```

Build the binary first with `mkdir -p kubernetes/apiserver/bin && go -C kubernetes/apiserver build -o bin/openshell-kube-apiserver ./cmd/openshell-kube-apiserver`. Gateway TLS and bearer-token flags are optional; client certificate/key flags enable gateway mTLS. The bearer token file is reread for each RPC. Do not use plaintext gateway connections outside a trusted development environment.

`spec.resources` maps only the gateway's portable `ResourceRequirements` field (`gpu.count`); CPU/memory or driver-specific resources belong in `spec.template.resources` as supported by the gateway template. `--fake-gateway` selects a development-only in-memory backend that accepts any workspace name; it is not persistent and must not be used in a real deployment.

## Real gateway demo on kind

Run these commands from the prototype worktree root. Prerequisites: Docker running,
kind, kubectl, Helm, OpenSSL with `-addext`, jq, and the `openshell` CLI.
Setup, the template scenario, and the automated test have been tested with CLI 0.1.5.

```shell
bash kubernetes/apiserver/hack/kind-demo.sh up
```

Equivalent mise task is `kube-api:demo:up`. Setup includes the two-workspace
template scenario described below. It
creates or reuses only the `openshell-api-demo` kind cluster. Repeated `up` runs
refresh its isolated kubeconfig, reconcile resources, and rebuild the adapter
without deleting existing sandboxes. Serving credentials are recovered from the
cluster Secret and reused unless they expire within a day; a changed adapter image
or serving certificate triggers a rollout. It writes local CLI credentials
under the ignored `kubernetes/apiserver/.demo/` directory, without modifying your
usual Kubernetes context or OpenShell configuration.

The demo pins Kubernetes 1.33.1, Agent Sandbox 1.0.3, and the published OpenShell
0.1.6 Helm chart and matching gateway, sandbox, and supervisor images. It builds
the aggregated API server from this checkout. Only runtime infrastructure uses
CRDs; `openshell.ai` resources have no CRDs or etcd storage.

The API server connects to `openshell.openshell.svc:8080` using the gateway's
`openshell-client-tls` Secret (`ca.crt`, `tls.crt`, `tls.key`). Kubernetes
automatically projects its short-lived ServiceAccount token for delegated
authentication and authorization. A separate, seven-day serving certificate is
mounted from `openshell-kube-apiserver-serving-cert`, with its CA embedded in the
APIService. No cert-manager installation is required.

This is an isolated development setup: the gateway's built-in local mTLS identity
has broad authority without OIDC role configuration. It is not an operator
certificate, but it is also not a workspace-scoped production bridge identity.
Use scoped gateway credentials for shared deployments. The prototype loads gateway
client certificates at startup; restart the Deployment after rotating that Secret.

### Run the disposable inline test

Use this test when `default/demo` and `default/demo-ssa` are disposable. Unlike
`up`, it applies and then deletes both names, and restarts the adapter Deployment.
It can fail on existing sandboxes with different specs or missing last-applied
annotations; it does not migrate them.

```shell
bash kubernetes/apiserver/hack/kind-demo.sh test
```

Equivalent mise task is `kube-api:demo:test`. The test impersonates a restricted
Kubernetes user and checks discovery, published
v2/v3 schemas, invalid field rejection, template/provider reads, both apply variants,
unchanged reapply, SSA ownership conflicts, readiness by polling, CLI/gateway identity
agreement, execution of a real workload command, label filtering, ownership and
authored-input preservation across an API-server restart, and deletion of both
gateway objects, runtime Sandboxes, and Pods.
It also verifies that an unauthorized user cannot read sandboxes and that the
restricted user cannot read the gateway's Secrets.
The demo has one API-server replica, so its intentional restart can briefly return
503; the test retries the read until aggregation recovers. It starts CLI
port-forwarding after sandbox readiness so runtime startup does not invalidate
the forwarding connection.

### Two workspaces for automated and interactive deployment

`up` prepares both gateway workspaces through the existing CLI, creates matching
Kubernetes namespaces and RoleBindings for `openshell-demo`, and creates the same
workspace-local template in each. Workspace and template creation remain outside
the aggregated API; both are read-only there.

| Workspace and namespace | Template | Sandbox after fresh setup |
|---|---|---|
| `team-a` | `ubuntu-demo` | `worker`, deployed and verified automatically |
| `team-b` | `ubuntu-demo` | None; ready for your interactive deployment |

Both templates use `nvcr.io/nvidia/base/ubuntu:24.04` and set
`DEMO_MESSAGE=from-workload-template`. Setup applies
`deploy/demo-template-sandbox.yaml` only in `team-a`, polls until Ready, checks
unchanged reapply and gateway identity, and verifies the inherited environment by
exec. It never applies a sandbox manifest in `team-b`.

To repeat just this scenario without rebuilding the adapter:

```shell
bash kubernetes/apiserver/hack/kind-demo.sh template-demo
```

Equivalent mise task is `kube-api:demo:template`. Both commands reuse workspaces
and templates, preserve `team-a/worker` identity, and leave all interactive
`team-b` sandboxes untouched. Existing resources must match the demonstrated
template; the script does not overwrite templates or recreate sandboxes to resolve
mismatches. It also leaves `default/demo` and resources from the earlier
`api-demo` scenario untouched.

Inspect the prepared resources:

```shell
export KUBECONFIG="$PWD/kubernetes/apiserver/.demo/kubeconfig"
kubectl get osw team-a team-b
kubectl get osst,oss -n team-a
kubectl get osst,oss -n team-b
kubectl get oss -A
```

### Deploy interactively in team-b

The interactive manifest references `ubuntu-demo` in `team-b`. It is not applied
by setup. Use the restricted demo user to exercise namespace RBAC:

```shell
kubectl --as=openshell-demo get osst ubuntu-demo -n team-b -o yaml
kubectl --as=openshell-demo create -f kubernetes/apiserver/deploy/demo-interactive-sandbox.yaml
kubectl --as=openshell-demo apply -f kubernetes/apiserver/deploy/demo-interactive-sandbox.yaml
kubectl --as=openshell-demo get oss worker -n team-b -o yaml
```

The apply reports `unchanged`, including after plain create. Repeat GET until
`status.phase` is `Ready`; general watches and Ready-condition waits remain
unsupported. Use `kubectl apply` instead of create if `team-b/worker` already exists.

To verify exec through the same gateway, forward it in a separate terminal:

```shell
export KUBECONFIG="$PWD/kubernetes/apiserver/.demo/kubeconfig"
kubectl -n openshell port-forward svc/openshell 18080:8080
```

In the first terminal, define a helper using the isolated demo credentials:

```shell
os_demo() {
  XDG_CONFIG_HOME="$PWD/kubernetes/apiserver/.demo/config" \
    openshell --gateway demo --gateway-endpoint https://localhost:18080 "$@"
}
os_demo --workspace team-a sandbox list
os_demo --workspace team-b sandbox list
os_demo --workspace team-b sandbox exec worker --no-tty --no-login-shell \
  --timeout 30 -- cat /sandbox/demo-marker
```

The output is `from-workload-template`. To remove only this sandbox:

```shell
kubectl --as=openshell-demo delete oss worker -n team-b --timeout=120s
```

The helper uses this demo gateway's isolated credentials, not your usual CLI
gateway. Kubernetes `-A` lists all workspaces on this gateway, not across gateways.
The namespaces are only Kubernetes addresses for gateway objects; creating a
namespace alone does not create its gateway workspace. Setup explicitly creates
both, without adding workspace-management behavior to the adapter.

### Present the inline sandbox demo manually

Use these commands on a fresh `default/demo`, or after the disposable test has
removed it. Reapplying an older object without a last-applied annotation requires
the compatible update service described in [Validation and apply](#validation-and-apply).

```shell
export KUBECONFIG="$PWD/kubernetes/apiserver/.demo/kubeconfig"
kubectl api-resources --api-group=openshell.ai
kubectl apply -f kubernetes/apiserver/deploy/demo-sandbox.yaml
kubectl apply -f kubernetes/apiserver/deploy/demo-sandbox.yaml
kubectl get openshellsandboxes -n default
kubectl get openshellsandbox demo -n default -o yaml
```

The second apply reports `unchanged`. Repeat GET to observe `status.phase: Ready`.
Only deletion waits are supported, as described in [Delete and wait](#delete-and-wait).
To demonstrate server-side apply, use a different sandbox name rather than changing
the field manager of the client-applied object:

```shell
sed 's/name: demo$/name: demo-ssa/' kubernetes/apiserver/deploy/demo-sandbox.yaml |
  kubectl apply --server-side --field-manager=demo -f -
```

Repeat the same command to reapply. Use `--show-managed-fields` with GET to inspect
persisted ownership. Delete `demo-ssa` when finished.

The inline demo uses the gateway's existing `default` workspace. It does not create
or manage OpenShell workspaces. The workload writes `real-gateway-demo` to
`/sandbox/demo-marker` and then sleeps. To show that the same sandbox runs in
OpenShell, forward the gateway in another terminal:

```shell
kubectl -n openshell port-forward svc/openshell 18080:8080
```

In the first terminal, use the demo's isolated CLI credentials:

```shell
XDG_CONFIG_HOME="$PWD/kubernetes/apiserver/.demo/config" \
  openshell --gateway demo --gateway-endpoint https://localhost:18080 \
  sandbox exec demo --no-tty --no-login-shell -- cat /sandbox/demo-marker
```

Delete through the aggregated API and inspect the runtime resources. Kubectl waits
for the gateway object to disappear; underlying runtime cleanup can finish later:

```shell
kubectl delete openshellsandbox demo -n default
kubectl get openshellsandboxes -n default
kubectl get sandboxes.agents.x-k8s.io -n openshell
```

Stop the port-forward with Ctrl-C. Tear down only the demo cluster:

```shell
bash kubernetes/apiserver/hack/kind-demo.sh down
unset KUBECONFIG
```

`down` retains local credentials and test artifacts in `.demo/` for diagnosis;
the next setup refreshes them from the cluster. Do not commit or share that directory.

For another real gateway, change `--gateway-address` and the Secret name of the
`gateway-credentials` volume in
`deploy/apiserver.yaml`. Both credential Secrets must exist in the API server's
namespace. A bearer-token deployment uses `--gateway-token-file` instead of the
client certificate/key flags and still needs gateway TLS trust.
