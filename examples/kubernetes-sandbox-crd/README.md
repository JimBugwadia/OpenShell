# Kubernetes Sandbox CRD

This walkthrough shows the experimental `OpenShellSandboxTemplate` and
`OpenShellSandbox` controller and the gateway's workspace managers setting. It
takes about 15 minutes, plus 20 for Part 3.

- Part 1 installs the controller in a kind cluster with its Helm chart and
  connects it to an existing local gateway.
- Part 2 runs a gateway built from this checkout to show which identities can
  change sandboxes.
- Part 3 installs everything in one kind cluster: Cilium, Agent Sandbox, the
  gateway, and the controller. Start here to learn what a Kubernetes install
  needs.

Each part also has a script that runs the same steps, checks every result, and
cleans up. Add `DEMO_PAUSE=1` to pause between steps when you present.

```shell
DEMO_PAUSE=1 tasks/scripts/demo-k8s-controller.sh             # Part 1
DEMO_PAUSE=1 tasks/scripts/test-workspace-managers-gateway.sh # Part 2
DEMO_PAUSE=1 tasks/scripts/demo-k8s-controller-in-cluster.sh  # Part 3
```

## Before you start

You need:

- Docker, kind, kubectl, and Helm. Part 2 also needs a Rust toolchain.
- For Part 1, a running OpenShell gateway registered with the CLI, using mTLS,
  such as one installed with Homebrew. The commands below assume the gateway
  named `openshell` at `https://localhost:17670`, with its client certificate
  in `~/.config/openshell/gateways/openshell/mtls`. The controller runs in a
  kind pod and reaches the gateway at `host.docker.internal`, so the gateway's
  server certificate must include that name, as the certificates that
  `openshell-gateway generate-certs` creates do.
- For Part 2, port 18291 free and the OpenShell 0.1.6 supervisor and sandbox
  images available to Docker.

Run every command from the root of your OpenShell checkout, in both
terminals. The build output and the demo kubeconfig both go in its `target`
directory.

## Part 1: manage a sandbox from Kubernetes

### Step 0: set up

Create a throwaway cluster and a dedicated gateway workspace. The cluster only
stores the declarations and runs the controller; the sandbox runs wherever the
gateway runs it.

```shell
export KUBECONFIG=$PWD/target/openshell-crd-demo.kubeconfig
MTLS=~/.config/openshell/gateways/openshell/mtls

kind create cluster --name openshell-crd-demo --kubeconfig "$KUBECONFIG"
kubectl create namespace agents
openshell workspace create --name agents
```

No controller image is published yet, so build one and load it into the
cluster. The script compiles the controller in a Rust container, so it needs
only Docker; the first build takes a few minutes:

```shell
IMAGE_TAG=demo tasks/scripts/build-k8s-controller-image.sh
kind load docker-image openshell/k8s-controller:demo --name openshell-crd-demo
```

### Step 1: install the controller

The controller authenticates to the gateway with a client certificate. This
demo reuses your CLI's certificate; in production, issue the controller its
own and list it as the workspace manager, as Part 2 shows. The controller runs
in its own namespace, `openshell-system`, not in the namespace it watches: if it
ran in `agents`, deleting that namespace would delete the controller before it
could clean up the gateway sandboxes. Store the certificate in a Secret there:

```shell
kubectl create namespace openshell-system
kubectl create secret generic gateway-tls -n openshell-system \
  --from-file=ca.crt="$MTLS/ca.crt" \
  --from-file=tls.crt="$MTLS/tls.crt" \
  --from-file=tls.key="$MTLS/tls.key"
```

Install the chart. One release binds one namespace, `watchNamespace`, to one
gateway workspace. The demo gives both the same name, `agents`, but the
controller never derives one from the other: the administrator who installs
the release sets the binding with `gateway.workspace`, so creating a namespace
can't select a gateway workspace.

```shell
helm install openshell-controller deploy/helm/openshell-k8s-controller \
  --namespace openshell-system --wait \
  --set watchNamespace=agents \
  --set image.repository=openshell/k8s-controller \
  --set image.tag=demo \
  --set image.pullPolicy=Never \
  --set gateway.url=https://host.docker.internal:17670 \
  --set gateway.workspace=agents \
  --set gateway.tls.secretName=gateway-tls
kubectl get crd | grep openshell.ai
kubectl logs -n openshell-system deployment/openshell-controller-openshell-k8s-controller
```

The chart installs the three CRDs, and the log shows that the controller took
its leader Lease:

```text
openshellproviders.openshell.ai          2026-11-23T00:00:00Z
openshellsandboxes.openshell.ai          2026-11-23T00:00:00Z
openshellsandboxtemplates.openshell.ai   2026-11-23T00:00:00Z
leader election: waiting for Lease openshell-controller-openshell-k8s-controller as openshell-controller-openshell-k8s-controller-7d9c5b7f4-x2kqv
leader election: acquired Lease openshell-controller-openshell-k8s-controller
```

If the controller can't reach the gateway, the pod restarts and its log
says "gateway connection failed". Check that the gateway is running and that
its certificate includes `host.docker.internal`.

### Step 2: declare a provider, a template, and a sandbox

The provider holds the agent's GitHub credential by reference to a Secret. The
template holds the reviewed workload: its image and settings. The sandbox names
the template and the provider. All three live in Git in a real deployment; the
Secret comes from your secret manager.

The provider type must exist in the workspace, so import the example GitHub
profile first. Then store a read-only GitHub token in a Secret:

```shell
openshell provider profile import -f providers/github.yaml --workspace agents
kubectl create secret generic github-token -n agents \
  --from-literal=token="<your-github-token>"
```

```shell
cat <<EOF | kubectl apply -f -
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
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandboxTemplate
metadata:
  name: demo-agent
  namespace: agents
spec:
  image: nvcr.io/nvidia/base/ubuntu:24.04
  environment:
    AGENT_RELEASE: "1"
---
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: support-agent
  namespace: agents
spec:
  templateRef:
    name: demo-agent
  providers: [github]
EOF
kubectl wait --for=condition=Ready osprov/github -n agents --timeout=60s
kubectl wait --for=condition=Ready ossbxt/demo-agent -n agents --timeout=60s
kubectl wait --for=condition=Ready ossbx/support-agent -n agents --timeout=300s
kubectl get osprov,ossbxt,ossbx -n agents
```

Expected:

```text
NAME                                     TYPE     PHASE   REASON    AGE
openshellprovider.openshell.ai/github    github   Ready   Applied   30s

NAME                                                  PHASE   REASON      REVISION        AGE
openshellsandboxtemplate.openshell.ai/demo-agent      Ready   Published   demo-agent-r1   30s

NAME                                          SANDBOX               PHASE   REASON         GATEWAY PHASE   AGE
openshellsandbox.openshell.ai/support-agent   os-5b395f76bad54509   Ready   GatewayReady   Ready           30s
```

The `SANDBOX` column is the sandbox's name in the gateway, so you can pass it to
`openshell sandbox` commands. Add `-o wide` to also show the template revision
and the gateway sandbox ID.

The controller creates the gateway provider `github` from the Secret, publishes
the template to the gateway as revision `demo-agent-r1`, and the sandbox
records the revision in `status.templateRevision`. To rotate the token, update
the Secret; the controller applies the new value to the gateway provider, and
`openshell provider get github --workspace agents` shows the change without
showing the value.

Point out: `kubectl wait --for=condition=Ready` works, so Argo CD and Flux can
assess health without custom scripts.

### Step 3: see the same sandbox through the gateway

```shell
NAME=$(kubectl get ossbx support-agent -n agents -o jsonpath='{.status.sandboxName}')
openshell sandbox get "$NAME" --workspace agents
openshell sandbox exec "$NAME" --workspace agents --no-tty -- echo "hello from a CRD-managed sandbox"
```

Point out: the gateway record carries `kubernetes.openshell.ai/namespace`,
`name`, and `uid` annotations that point back to the resource.

### Step 4: try to change the workload

```shell
kubectl patch ossbx support-agent -n agents --type=merge \
  -p '{"spec":{"templateRef":{"name":"something-else"}}}'
```

Expected: the API server rejects the change with an `immutable` error. The
workload template can't change on a running sandbox, so create a new resource
to run a different workload. The prototype makes the whole spec immutable;
later versions can allow in-place changes to other fields, such as policy and
providers, while the workload stays immutable.

### Step 5: change the template

```shell
kubectl patch ossbxt demo-agent -n agents --type=merge \
  -p '{"spec":{"environment":{"AGENT_RELEASE":"2"}}}'
kubectl wait --for=jsonpath='{.status.currentRevision}'=demo-agent-r2 \
  ossbxt/demo-agent -n agents --timeout=60s
openshell sandbox template list --workspace agents
kubectl get ossbx support-agent -n agents -o jsonpath='{.status.templateRevision}{"\n"}'
```

Expected: the gateway lists `demo-agent-r1` and `demo-agent-r2`, and
`support-agent` still reports `demo-agent-r1`. Gateway templates can't change in
place, so each template change publishes a new revision. New sandboxes get the
latest revision; running sandboxes keep the one they started with.

### Step 6: restart the controller

Replace the controller pod, then check the sandbox:

```shell
kubectl rollout restart deployment/openshell-controller-openshell-k8s-controller -n openshell-system
kubectl rollout status deployment/openshell-controller-openshell-k8s-controller -n openshell-system
kubectl get ossbx support-agent -n agents \
  -o jsonpath='{.status.sandboxId} attempts={.status.journal.createAttempts}{"\n"}'
```

Expected: the same sandbox ID and `attempts=1`. The controller resumes from the
journal in status and replays nothing that already succeeded.

### Step 7: request a template that doesn't exist

```shell
cat <<EOF | kubectl apply -f -
apiVersion: openshell.ai/v1alpha1
kind: OpenShellSandbox
metadata:
  name: missing-template
  namespace: agents
spec:
  templateRef:
    name: does-not-exist
EOF
kubectl wait --for=jsonpath='{.status.reason}'=TemplateNotFound ossbx/missing-template -n agents
kubectl get ossbx -n agents
kubectl delete ossbx missing-template -n agents
```

Expected: `Blocked` with reason `TemplateNotFound`, and the delete completes
without removing the finalizer by hand, because no gateway record can exist.

### Step 8: measure the etcd footprint

```shell
kubectl -n kube-system exec etcd-openshell-crd-demo-control-plane -- etcdctl \
  --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt \
  --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key \
  get /registry/openshell.ai/openshellsandboxes/agents/support-agent --print-value-only | wc -c
```

Expected: about 2,800 bytes. A Ready sandbox causes no further writes; the
script checks this by watching the resource for 45 seconds.

### Step 9: delete the sandbox, the provider, and the template

```shell
kubectl get ossbx support-agent -n agents -o jsonpath='{.metadata.finalizers}{"\n"}'
kubectl delete ossbx support-agent -n agents
openshell sandbox get "$NAME" --workspace agents
kubectl delete osprov github -n agents
kubectl delete ossbxt demo-agent -n agents
openshell sandbox template list --workspace agents
```

Expected: the sandbox delete waits for the gateway to remove the sandbox, so
`openshell sandbox get` reports that it isn't found. Deleting the template
removes both of its gateway revisions, so the last command lists no
`demo-agent` revisions. Delete the provider and the template after their
sandboxes: the gateway refuses to delete a provider that a sandbox uses.

### Clean up

Delete the provider profile first: a workspace that still holds one never
finishes deleting. Deleting the cluster removes the controller.

```shell
openshell provider profile delete github --workspace agents
openshell workspace delete agents
kind delete cluster --name openshell-crd-demo --kubeconfig "$KUBECONFIG"
```

## Part 2: only the manager changes sandboxes

Run this part with the script: it generates its own certificates and gateway
configuration, which are tedious to set up by hand.

```shell
DEMO_PAUSE=1 tasks/scripts/test-workspace-managers-gateway.sh
```

It starts a gateway from this checkout with:

```toml
[openshell.gateway.workspace_managers]
default = ["mtls:openshell-k8s-controller", "mtls:agent-executor"]
```

and four client certificates: `openshell-k8s-controller` (a manager),
`agent-executor` (a second manager: a service that creates a sandbox per
conversation with the existing sandbox API), `openshell-cli` (an ordinary user), and
`oncall` with `OU=operator`.

| Step | What happens | Point out |
| :- | :- | :- |
| 1 | `openshell-cli` tries `sandbox create` | Rejected: "this workspace is managed by another identity" |
| 2 | `openshell-k8s-controller` creates `managed-1` | The sandbox records `internal.openshell.ai/workspace-manager: mtls:openshell-k8s-controller` and the list in effect |
| 3 | `openshell-cli` lists, then tries `delete` and `stop` | Reading works; changes are rejected |
| 4 | The gateway restarts with an extra manager in the list | It refuses to start while sandboxes exist, and starts again with the original list |
| 5 | `oncall` deletes `managed-1` | Operators keep an emergency stop and delete |
| 6 | Revisions `agent-r1` and `agent-r2` of template family `agent` are published, and `agent-executor` creates `session-1` from `--template agent` | The session resolves to `agent-r2` and records `agent-executor` as its manager; the controller can't delete it. `openshell-cli` can't publish a template, and `agent-executor` can't delete the controller's |

The script uses a temporary directory for everything, including the CLI's
configuration, so it doesn't change your registered gateways.

## Part 3: run everything in one cluster

Parts 1 and 2 run sandboxes on a gateway outside Kubernetes. This part installs
the whole stack in one kind cluster, the way you would on a real cluster: a
network plugin that enforces NetworkPolicy, the Agent Sandbox controller, the
OpenShell gateway, and the controller. Sandboxes then run as pods in the same
cluster. It takes about 20 minutes, and you need Docker, kind, kubectl, Helm,
and the [Cilium CLI](https://docs.cilium.io/en/stable/gettingstarted/k8s-install-default/#install-the-cilium-cli).

The script runs the same steps, checks each result, and deletes the cluster.
Its CLI configuration lives in a temporary directory, so it doesn't change your
registered gateways:

```shell
DEMO_PAUSE=1 tasks/scripts/demo-k8s-controller-in-cluster.sh
```

### Step 1: create a cluster that enforces NetworkPolicy

The gateway isolates each sandbox with NetworkPolicy: the workload pod may talk
only to its supervisor pod, which applies the sandbox's network policy. kind's
default network plugin accepts NetworkPolicy objects but doesn't enforce them,
so replace it with Cilium:

```shell
export KUBECONFIG=$PWD/target/openshell-e2e.kubeconfig

cat <<EOF > target/openshell-e2e-kind.yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  disableDefaultCNI: true
EOF
kind create cluster --name openshell-e2e --config target/openshell-e2e-kind.yaml \
  --kubeconfig "$KUBECONFIG"
cilium install --version 1.18.1 --wait
cilium status --wait
```

Until Cilium is running, the node reports `NotReady`; that's expected.

### Step 2: install Agent Sandbox

The gateway creates each sandbox's workload pod through the
[Agent Sandbox](https://agent-sandbox.sigs.k8s.io) controller, so install it
first:

```shell
kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.12/sandbox.yaml
kubectl -n agent-sandbox-system rollout status deployment
```

### Step 3: install the gateway

Install the gateway chart. The `0.0.0-dev` version is the latest build of the
main branch; use a release version when one includes the features you need.

```shell
helm install openshell oci://quay.io/ai-openshell/charts/openshell \
  --version 0.0.0-dev --namespace openshell --create-namespace --wait
kubectl -n openshell get pods
```

The chart generates a certificate authority, a server certificate, and a client
certificate with `CN=openshell-client`, stored in Secret `openshell-client-tls`.
The gateway accepts that client certificate as a user. For other ways to
authenticate users, see [Authentication options](#authentication-options).

### Step 4: connect the CLI

The gateway has no external address, so reach it through a port-forward. Run
this in a second terminal and leave it running:

```shell
export KUBECONFIG=$PWD/target/openshell-e2e.kubeconfig
kubectl -n openshell port-forward svc/openshell 18080:8080
```

Back in the first terminal, copy the client certificate to the CLI's
configuration and register the gateway as `kind`:

```shell
MTLS=~/.config/openshell/gateways/kind/mtls
mkdir -p "$MTLS"
for f in ca.crt tls.crt tls.key; do
  kubectl -n openshell get secret openshell-client-tls \
    -o "jsonpath={.data.${f//./\\.}}" | base64 -d > "$MTLS/$f"
done
OPENSHELL_LOCAL_TLS_DIR=/nonexistent \
  openshell gateway add https://127.0.0.1:18080 --local --name kind
openshell -g kind status
openshell -g kind workspace create --name agents
```

With `--local`, the CLI also imports the client certificate of any gateway
installed on your machine, such as one from Homebrew, over the files you just
copied. `OPENSHELL_LOCAL_TLS_DIR=/nonexistent` turns that import off. Without
it, `status` fails with `invalid peer certificate: BadSignature`.

`openshell -g kind status` shows `Status: Connected` and
`Authentication: Authenticated (mTLS transport)`. Adding a gateway makes it the
active one; to switch back, run `openshell gateway select openshell`.

### Step 5: install the controller

Run the controller in its own namespace, not in the namespace it watches. If it
ran in `agents`, deleting that namespace would delete the controller before it
could clean up the gateway sandboxes.

```shell
IMAGE_TAG=demo tasks/scripts/build-k8s-controller-image.sh
kind load docker-image openshell/k8s-controller:demo --name openshell-e2e

kubectl create namespace agents
kubectl create namespace openshell-system
kubectl -n openshell-system create secret generic gateway-tls \
  --from-file="$MTLS/ca.crt" --from-file="$MTLS/tls.crt" --from-file="$MTLS/tls.key"

helm install openshell-controller deploy/helm/openshell-k8s-controller \
  --namespace openshell-system --wait \
  --set watchNamespace=agents \
  --set image.repository=openshell/k8s-controller \
  --set image.tag=demo \
  --set image.pullPolicy=Never \
  --set gateway.url=https://openshell.openshell.svc:8080 \
  --set gateway.workspace=agents \
  --set gateway.tls.secretName=gateway-tls
kubectl -n openshell-system logs deployment/openshell-controller-openshell-k8s-controller
```

The controller reaches the gateway through its in-cluster Service,
`openshell.openshell.svc`, which the gateway's server certificate includes. The
log shows that the controller took its leader Lease.

### Step 6: declare a sandbox

Follow [Step 2 of Part 1](#step-2-declare-a-provider-a-template-and-a-sandbox),
adding `-g kind` to each `openshell` command. Then look at the pods the gateway
created for the sandbox:

```shell
kubectl -n openshell get pods
```

Expected: besides `openshell-0`, a supervisor pod (`os-supervisor-...`) and a
workload pod (`agents--os-...`). The workload pod runs the template's image; the
supervisor pod holds the credentials and enforces the sandbox's policy. Steps 3
to 7 of Part 1 work here too, with `-g kind`. For Step 8, the etcd pod is
`etcd-openshell-e2e-control-plane`.

### Step 7: check that NetworkPolicy is enforced

Run the same connection check in the workload pod's network and in an ordinary
pod. An ephemeral container shares the workload pod's network, and so its
NetworkPolicy:

```shell
kubectl -n openshell get networkpolicy
WORKLOAD=$(kubectl -n openshell get pod -l openshell.ai/boundary-role=workload \
  -o jsonpath='{.items[0].metadata.name}')
kubectl -n openshell debug "$WORKLOAD" -it --image=busybox:1.36 --profile=general \
  -- sh -c 'nc -w 5 -z 1.1.1.1 443 && echo CONNECTED || echo BLOCKED'
kubectl run netcheck -it --rm --restart=Never --image=busybox:1.36 \
  -- sh -c 'nc -w 5 -z 1.1.1.1 443 && echo CONNECTED || echo BLOCKED'
```

Expected: `BLOCKED` from the workload pod and `CONNECTED` from the ordinary
pod. The workload's traffic can leave only through its supervisor, which
applies the sandbox's network policy. With kind's default network plugin, both
would print `CONNECTED`.

### Step 8: delete the namespace

Deleting a namespace deletes every resource in it. The controller's finalizers
hold each one until the gateway object is gone:

```shell
kubectl delete namespace agents
openshell -g kind sandbox list --workspace agents
openshell -g kind sandbox template list --workspace agents
openshell -g kind provider list --workspace agents
```

Expected: the namespace takes about a minute to delete, and all three lists are
empty. The gateway refuses to delete a provider while a sandbox uses it, so the
provider waits for the sandbox and retries. The gateway workspace stays; delete
it yourself if you don't need it.

### Clean up

Stop the port-forward, then:

```shell
openshell gateway remove kind
kind delete cluster --name openshell-e2e --kubeconfig "$KUBECONFIG"
```

### Authentication options

The chart's client certificate is enough for a throwaway cluster, but everyone
who has it is the same user, and the controller in this part reuses it. For a
shared cluster, pick one of the gateway's other user authentication models,
described in [Access Control](https://docs.nvidia.com/openshell/latest/kubernetes/access-control):

| Option | How it works | What it means for the controller |
| :- | :- | :- |
| Client certificates (this part) | The gateway maps each verified client certificate to a user. | Issue the controller its own certificate, so the gateway can tell it from people and, with workspace managers, allow only it to change sandboxes. The chart's certificate authority key isn't kept, so issue certificates from your own CA, as Part 2 does. |
| OIDC | The gateway validates a bearer token from an identity provider, such as Keycloak, Microsoft Entra ID, or Okta, and maps its roles to OpenShell roles. Set `gatewayConfig.openshell.gateway.oidc.issuer` and `audience` on the gateway chart. | The controller reads a bearer token from a file (`gateway.token.secretName` on its chart) and reloads it when it changes, but it can't request tokens itself. Something else must refresh the Secret before the token expires, such as a job that runs the client credentials flow. Built-in client credentials support is a follow-up. |
| A trusted access proxy | A proxy such as Cloudflare Access, ngrok, or a corporate SSO gateway authenticates callers, and the gateway accepts every call it forwards (`server.disableTls=true` and `allow_unauthenticated_users=true`). | Point the controller at the gateway's in-cluster Service, which bypasses the proxy. The gateway then can't tell the controller from anyone else, so workspace managers can't protect the controller's sandboxes. Use this only when the gateway is unreachable except through the proxy. |

## Troubleshooting

- **A step fails in a script:** the script prints the controller or gateway log
  and cleans up what it created.
- **A resource won't delete:** check `kubectl get ossbx -n agents -o
  jsonpath='{.items[*].status.reason}'`. `CreateReplayExhausted`,
  `DeleteReplayExhausted`, `NameCollision`, and `BindingChanged` need you to
  verify the gateway sandbox before you remove the finalizer; see the
  controller README.
- **Port 18291 is in use:** set `MANAGERS_TEST_PORT` for Part 2.
