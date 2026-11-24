# OpenShell Kubernetes controller Helm chart

> **Experimental** - this chart deploys the Kubernetes controller prototype.
> The resource API and chart values can change without notice.

This chart runs the OpenShell Kubernetes controller as a Deployment. The
controller watches `OpenShellSandbox`, `OpenShellSandboxTemplate`, and
`OpenShellProvider` resources in one namespace and applies them to one
workspace of an OpenShell gateway through the gateway's API. The gateway can
run in this cluster, another cluster, or outside Kubernetes.

The chart installs:

- The three OpenShell CRDs, generated from the controller binary. They're kept
  when you uninstall the release.
- A ServiceAccount; a ClusterRole and ClusterRoleBinding for the OpenShell
  resources; a Role and RoleBinding for Secrets in the watched namespace; and a
  Role for the leader Lease in the release namespace. The OpenShell resources
  are granted cluster-wide so that deleting the watched namespace can't remove
  the controller's access before its finalizers finish. The controller still
  watches only its own namespace.
- The `openshell-sandbox-platform` and `openshell-sandbox-developer`
  ClusterRoles for people. Bind them per namespace with a RoleBinding.
- A Deployment with one replica that reconciles while it holds a leader
  Lease. The Lease keeps the old and new pods from reconciling at once during
  a rolling update. Set `replicaCount` to 2 for a standby that takes over
  within seconds if the leader stops.

## Before you install

1. Issue a client certificate for the controller from the gateway's client CA,
   and list its identity as the workspace manager in the gateway's
   `[openshell.gateway.workspace_managers]` table. For example, for a
   certificate with the common name `openshell-k8s-controller`:

   ```toml
   [openshell.gateway.workspace_managers]
   agents = ["mtls:openshell-k8s-controller"]
   ```

2. Create the gateway workspace, and make the controller's identity a
   workspace admin.

3. Install the release in its own namespace, not in the namespace it watches.
   If it ran in the watched namespace, deleting that namespace would delete the
   controller before it could clean up the gateway sandboxes. Store the gateway
   CA and the controller's certificate and key in a Secret in the release
   namespace:

   ```shell
   kubectl create namespace openshell-system
   kubectl create secret generic openshell-controller-tls -n openshell-system \
     --from-file=ca.crt --from-file=tls.crt --from-file=tls.key
   ```

## Install

```shell
helm install openshell-controller deploy/helm/openshell-k8s-controller \
  --namespace openshell-system \
  --set watchNamespace=agents \
  --set gateway.url=https://openshell.openshell.svc:8080 \
  --set gateway.workspace=agents \
  --set gateway.tls.secretName=openshell-controller-tls
```

To bind another namespace, install another release with
`crds.install=false` and `userRoles.create=false`, because only one release
can own those cluster-scoped objects.

## Rotate credentials

Update the Secret in place. The kubelet updates the mounted files, and the
controller reconnects with the new certificate within
`credentialReloadSeconds` of the update, without a restart. If it can't
connect with the new certificate, it keeps the current connection and tries
again.

## Values

| Key | Default | Description |
|-----|---------|-------------|
| `gateway.url` | `""` | Required. The gateway URL that the controller calls. |
| `gateway.workspace` | `""` | Required. The gateway workspace that the watched namespace maps to. |
| `gateway.tls.secretName` | `""` | Secret with `ca.crt`, `tls.crt`, and `tls.key`. |
| `gateway.token.secretName` | `""` | Secret with an OIDC bearer token, instead of or with a certificate. |
| `gateway.token.key` | `token` | Key of the token in the Secret. |
| `watchNamespace` | release namespace | Namespace whose OpenShell resources the release reconciles. |
| `replicaCount` | `1` | Replicas. More than one requires `leaderElection.enabled`. |
| `leaderElection.enabled` | `true` | Run the controllers only in the replica that holds the Lease. |
| `leaderElection.leaseDurationSeconds` | `15` | How long the Lease stays valid without a renewal. |
| `credentialReloadSeconds` | `10` | Seconds between checks for rotated credentials; `0` disables reloading. |
| `maxConcurrentReconciles` | `16` | Reconciles that may run at once. |
| `reportGatewayPhase` | `true` | Report the gateway's runtime phase in sandbox status. |
| `crds.install` | `true` | Install the CRDs. Set `false` in every release but one. |
| `userRoles.create` | `true` | Create the ClusterRoles for people. Set `false` in every release but one. |
| `image.repository` | `ghcr.io/nvidia/openshell/k8s-controller` | Controller image. |
| `image.tag` | chart `appVersion` | Image tag. |
| `hostAliases` | `[]` | Extra `/etc/hosts` entries, to reach a gateway that cluster DNS doesn't resolve. |

See [values.yaml](values.yaml) for resources, security contexts, and
scheduling values.

## Test

`tasks/scripts/test-k8s-controller-helm.sh` installs the chart in a kind
cluster against a gateway built from the checkout, then tests leader failover,
credential rotation, and uninstall.

After you change the CRD types, regenerate `files/crds.yaml`; a controller
test fails while it's out of date:

```shell
{ sed -n '1,5p' deploy/helm/openshell-k8s-controller/files/crds.yaml
  cargo run -q -p openshell-k8s-controller -- --print-crd
} > crds.yaml.new && mv crds.yaml.new deploy/helm/openshell-k8s-controller/files/crds.yaml
```
