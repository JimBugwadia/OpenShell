# Kubernetes Sandbox CRD

This walkthrough shows the experimental `OpenShellSandboxTemplate` and
`OpenShellSandbox` controller and the gateway's sandbox manager setting. It takes about 15 minutes. Part 1 runs the
controller against an existing local gateway; Part 2 runs a gateway built from
this checkout to show which identities can change sandboxes.

Each part also has a script that runs the same steps, checks every result, and
cleans up. Add `DEMO_PAUSE=1` to pause between steps when you present.

```shell
DEMO_PAUSE=1 tasks/scripts/demo-k8s-controller.sh          # Part 1
DEMO_PAUSE=1 tasks/scripts/test-sandbox-managers-gateway.sh # Part 2
```

## Before you start

You need:

- Docker, kind, kubectl, and a Rust toolchain.
- For Part 1, a running OpenShell gateway registered with the CLI, using mTLS.
  The commands below assume the gateway named `openshell` at
  `https://localhost:17670`, with its client certificate in
  `~/.config/openshell/gateways/openshell/mtls`.
- For Part 2, port 18291 free and the OpenShell 0.1.6 supervisor and sandbox
  images available to Docker.

Run every command from the root of your OpenShell checkout, in both
terminals. The build output and the demo kubeconfig both go in its `target`
directory.

## Part 1: manage a sandbox from Kubernetes

### Step 0: set up

Create a throwaway cluster for the CRDs, plus a dedicated gateway workspace.
The cluster only stores the declaration; the sandbox runs
wherever the gateway runs it.

```shell
export KUBECONFIG=$PWD/target/openshell-crd-demo.kubeconfig
MTLS=~/.config/openshell/gateways/openshell/mtls

kind create cluster --name openshell-crd-demo --kubeconfig "$KUBECONFIG"
kubectl create namespace agents
openshell workspace create --name crd-demo
```

Build the controller and install its CRDs. The controller binary prints both
CRDs, `OpenShellSandbox` and `OpenShellSandboxTemplate`, so the installed
schema always matches the controller you run:

```shell
cargo build -p openshell-k8s-controller
target/debug/openshell-k8s-controller --print-crd | kubectl apply -f -
kubectl wait --for=condition=Established \
  crd/openshellsandboxes.openshell.ai crd/openshellsandboxtemplates.openshell.ai
kubectl get crd | grep openshell.ai
```

The last command lists both CRDs:

```text
openshellsandboxes.openshell.ai          2026-11-19T00:00:00Z
openshellsandboxtemplates.openshell.ai   2026-11-19T00:00:00Z
```

In a second terminal, from the same checkout, start the controller.
One controller binds one namespace to one gateway workspace:

```shell
export KUBECONFIG=$PWD/target/openshell-crd-demo.kubeconfig
MTLS=~/.config/openshell/gateways/openshell/mtls
target/debug/openshell-k8s-controller \
  --namespace agents --workspace crd-demo --gateway https://localhost:17670 \
  --tls-ca "$MTLS/ca.crt" --tls-cert "$MTLS/tls.crt" --tls-key "$MTLS/tls.key"
```

### Step 1: declare a template and a sandbox

The template holds the reviewed workload: its image and settings. The sandbox
names the template. Both live in Git in a real deployment.

```shell
cat <<EOF | kubectl apply -f -
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: demo-agent
  namespace: agents
spec:
  image: ghcr.io/nvidia/openshell/sandbox:0.1.6
  environment:
    AGENT_RELEASE: "1"
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: agents
spec:
  workloadTemplate: demo-agent
EOF
kubectl wait --for=condition=Ready ossbxt/demo-agent -n agents --timeout=60s
kubectl wait --for=condition=Ready ossbx/support-agent -n agents --timeout=300s
kubectl get ossbxt,ossbx -n agents
```

Expected:

```text
NAME                                                  PHASE   REASON      REVISION        AGE
openshellsandboxtemplate.openshell.ai/demo-agent      Ready   Published   demo-agent-r1   30s

NAME                                          PHASE   REASON         GATEWAY PHASE   AGE
openshellsandbox.openshell.ai/support-agent   Ready   GatewayReady   Ready           30s
```

The controller publishes the template to the gateway as revision
`demo-agent-r1`, and the sandbox records it in `status.templateRevision`.

Point out: `kubectl wait --for=condition=Ready` works, so Argo CD and Flux can
assess health without custom scripts.

### Step 2: see the same sandbox through the gateway

```shell
NAME=$(kubectl get ossbx support-agent -n agents -o jsonpath='{.status.sandboxName}')
openshell sandbox get "$NAME" --workspace crd-demo
openshell sandbox exec "$NAME" --workspace crd-demo --no-tty -- echo "hello from a CRD-managed sandbox"
```

Point out: the gateway record carries `kubernetes.openshell.ai/namespace`,
`name`, and `uid` annotations that point back to the resource.

### Step 3: try to change the workload

```shell
kubectl patch ossbx support-agent -n agents --type=merge \
  -p '{"spec":{"workloadTemplate":"something-else"}}'
```

Expected: the API server rejects the change with an `immutable` error. The
workload template can't change on a running sandbox, so create a new resource
to run a different workload. The prototype makes the whole spec immutable;
later versions can allow in-place changes to other fields, such as policy and
providers, while the workload stays immutable.

### Step 4: change the template

```shell
kubectl patch ossbxt demo-agent -n agents --type=merge \
  -p '{"spec":{"environment":{"AGENT_RELEASE":"2"}}}'
kubectl wait --for=jsonpath='{.status.currentRevision}'=demo-agent-r2 \
  ossbxt/demo-agent -n agents --timeout=60s
openshell sandbox template list --workspace crd-demo
kubectl get ossbx support-agent -n agents -o jsonpath='{.status.templateRevision}{"\n"}'
```

Expected: the gateway lists `demo-agent-r1` and `demo-agent-r2`, and
`support-agent` still reports `demo-agent-r1`. Gateway templates can't change in
place, so each template change publishes a new revision. New sandboxes get the
latest revision; running sandboxes keep the one they started with.

### Step 5: restart the controller

Stop the controller with Ctrl+C and start it again, then:

```shell
kubectl get ossbx support-agent -n agents \
  -o jsonpath='{.status.sandboxId} attempts={.status.journal.createAttempts}{"\n"}'
```

Expected: the same sandbox ID and `attempts=1`. The controller resumes from the
journal in status and replays nothing that already succeeded.

### Step 6: request a template that doesn't exist

```shell
cat <<EOF | kubectl apply -f -
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: missing-template
  namespace: agents
spec:
  workloadTemplate: does-not-exist
EOF
kubectl wait --for=jsonpath='{.status.reason}'=CreateRejected ossbx/missing-template -n agents
kubectl get ossbx -n agents
kubectl delete ossbx missing-template -n agents
```

Expected: `Blocked` with reason `CreateRejected`, and the delete completes
without removing the finalizer by hand, because no gateway record can exist.

### Step 7: measure the etcd footprint

```shell
kubectl -n kube-system exec etcd-openshell-crd-demo-control-plane -- etcdctl \
  --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt \
  --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key \
  get /registry/openshell.ai/openshellsandboxes/agents/support-agent --print-value-only | wc -c
```

Expected: about 2,800 bytes. A Ready sandbox causes no further writes; the
script checks this by watching the resource for 45 seconds.

### Step 8: delete the sandbox and the template

```shell
kubectl get ossbx support-agent -n agents -o jsonpath='{.metadata.finalizers}{"\n"}'
kubectl delete ossbx support-agent -n agents
openshell sandbox get "$NAME" --workspace crd-demo
kubectl delete ossbxt demo-agent -n agents
openshell sandbox template list --workspace crd-demo
```

Expected: the sandbox delete waits for the gateway to remove the sandbox, so
`openshell sandbox get` reports that it isn't found. Deleting the template
removes both of its gateway revisions, so the last command lists no
`demo-agent` revisions. Delete the template after its sandboxes.

### Clean up

Stop the controller, then:

```shell
openshell workspace delete crd-demo
kind delete cluster --name openshell-crd-demo --kubeconfig "$KUBECONFIG"
```

## Part 2: only the manager changes sandboxes

Run this part with the script: it generates its own certificates and gateway
configuration, which are tedious to set up by hand.

```shell
DEMO_PAUSE=1 tasks/scripts/test-sandbox-managers-gateway.sh
```

It starts a gateway from this checkout with:

```toml
[openshell.gateway.sandbox_managers]
default = ["mtls:openshell-k8s-controller", "mtls:agent-executor"]
```

and four client certificates: `openshell-k8s-controller` (a manager),
`agent-executor` (a second manager: a service that creates a sandbox per
conversation with the existing sandbox API), `openshell-cli` (an ordinary user), and
`oncall` with `OU=operator`.

| Step | What happens | Point out |
| :- | :- | :- |
| 1 | `openshell-cli` tries `sandbox create` | Rejected: "this workspace's sandboxes are managed by another identity" |
| 2 | `openshell-k8s-controller` creates `managed-1` | The sandbox records `internal.openshell.ai/sandbox-manager: mtls:openshell-k8s-controller` and the list in effect |
| 3 | `openshell-cli` lists, then tries `delete` and `stop` | Reading works; changes are rejected |
| 4 | The gateway restarts with an extra manager in the list | It refuses to start while sandboxes exist, and starts again with the original list |
| 5 | `oncall` deletes `managed-1` | Operators keep an emergency stop and delete |
| 6 | Revisions `agent-r1` and `agent-r2` of template family `agent` are published, and `agent-executor` creates `session-1` from `--template agent` | The session resolves to `agent-r2` and records `agent-executor` as its manager; the controller can't delete it |

The script uses a temporary directory for everything, including the CLI's
configuration, so it doesn't change your registered gateways.

## Troubleshooting

- **A step fails in a script:** the script prints the controller or gateway log
  and cleans up what it created.
- **A resource won't delete:** check `kubectl get ossbx -n agents -o
  jsonpath='{.items[*].status.reason}'`. `CreateReplayExhausted`,
  `DeleteReplayExhausted`, `NameCollision`, and `BindingChanged` need you to
  verify the gateway sandbox before you remove the finalizer; see the
  controller README.
- **Port 18291 is in use:** set `MANAGERS_TEST_PORT` for Part 2.
