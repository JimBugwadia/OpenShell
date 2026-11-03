// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package apiserver

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"net"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/wait"
	"k8s.io/apiserver/pkg/server/options"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/rest"
)

type delayedDeletion struct {
	gateway.Interface
	fake    *gateway.Fake
	started chan struct{}
	admit   chan struct{}
	release chan struct{}
}

func (g *delayedDeletion) DeleteSandbox(ctx context.Context, workspace, name, requestID string) error {
	if name == "delete-denied" {
		return status.Error(codes.PermissionDenied, "delete denied")
	}
	if name != "delete-wait" {
		return g.Interface.DeleteSandbox(ctx, workspace, name, requestID)
	}
	close(g.started)
	select {
	case <-g.admit:
	case <-ctx.Done():
		return ctx.Err()
	}
	g.fake.SetSandboxPhase(workspace+"/"+name, openshellv1.SandboxPhase_SANDBOX_PHASE_DELETING)
	select {
	case <-g.release:
		return g.Interface.DeleteSandbox(ctx, workspace, name, requestID)
	case <-ctx.Done():
		return ctx.Err()
	}
}

func TestAggregatedAPIWithFakeGateway(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	fake := gateway.NewFake()
	fake.EnsureWorkspace("team-a")
	recommended := options.NewRecommendedOptions("/registry/openshell.ai", nil)
	recommended.Etcd = nil
	recommended.SecureServing.SecureServingOptions.Listener = listener
	recommended.SecureServing.SecureServingOptions.ExternalAddress = net.ParseIP("127.0.0.1")
	recommended.Authentication.RemoteKubeConfigFileOptional = true
	recommended.Authentication.SkipInClusterLookup = true
	recommended.Authorization = nil
	delayed := &delayedDeletion{Interface: fake, fake: fake, started: make(chan struct{}), admit: make(chan struct{}), release: make(chan struct{})}
	apiServer, err := New(delayed, recommended)
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stopped, listenerStopped, err := apiServer.PrepareRun().NonBlockingRunWithContext(ctx, 5*time.Second)
	if err != nil {
		t.Fatalf("start apiserver: %v", err)
	}
	defer func() {
		cancel()
		select {
		case <-stopped:
		case <-time.After(6 * time.Second):
			t.Error("apiserver did not stop")
		}
		select {
		case <-listenerStopped:
		case <-time.After(6 * time.Second):
			t.Error("apiserver listener did not stop")
		}
	}()

	dynamicClient, err := dynamic.NewForConfig(&rest.Config{Host: "https://" + listener.Addr().String(), TLSClientConfig: rest.TLSClientConfig{Insecure: true}})
	if err != nil {
		t.Fatal(err)
	}
	resource := dynamicClient.Resource(schema.GroupVersionResource{Group: "openshell.ai", Version: "v1alpha1", Resource: "openshellsandboxes"}).Namespace("team-a")
	var created *unstructured.Unstructured
	err = wait.PollUntilContextTimeout(ctx, 50*time.Millisecond, 5*time.Second, true, func(ctx context.Context) (bool, error) {
		var createErr error
		created, createErr = resource.Create(ctx, &unstructured.Unstructured{Object: map[string]any{"apiVersion": "openshell.ai/v1alpha1", "kind": "OpenShellSandbox", "metadata": map[string]any{"name": "worker"}, "spec": map[string]any{"desiredState": "Running"}}}, metav1.CreateOptions{})
		if createErr != nil && (strings.Contains(createErr.Error(), "connection refused") || strings.Contains(createErr.Error(), "TLS handshake timeout")) {
			return false, nil
		}
		return createErr == nil, createErr
	})
	if err != nil {
		t.Fatalf("create through aggregated API: %v", err)
	}
	if created.GetUID() != "team-a/worker" {
		t.Fatalf("uid = %q", created.GetUID())
	}
	testDeletion := func(t *testing.T) {
		if _, err := exec.LookPath("kubectl"); err != nil {
			t.Skip("kubectl not installed")
		}
		defer func() {
			select {
			case <-delayed.admit:
			default:
				close(delayed.admit)
			}
			select {
			case <-delayed.release:
			default:
				close(delayed.release)
			}
		}()
		obj := &unstructured.Unstructured{Object: map[string]any{"apiVersion": "openshell.ai/v1alpha1", "kind": "OpenShellSandbox", "metadata": map[string]any{"name": "delete-wait"}, "spec": map[string]any{"desiredState": "Running"}}}
		if _, err := resource.Create(ctx, obj, metav1.CreateOptions{}); err != nil {
			t.Fatal(err)
		}
		commandCtx, stop := context.WithTimeout(ctx, 15*time.Second)
		defer stop()
		cmd := exec.CommandContext(commandCtx, "kubectl", "--kubeconfig=/dev/null", "--token=test", "--server=https://"+listener.Addr().String(), "--insecure-skip-tls-verify", "--namespace=team-a", "delete", "openshellsandboxes.openshell.ai", "delete-wait", "--timeout=10s")
		var stderr bytes.Buffer
		cmd.Stderr = &stderr
		stdout, err := cmd.StdoutPipe()
		if err != nil {
			t.Fatal(err)
		}
		if err := cmd.Start(); err != nil {
			t.Fatal(err)
		}
		line := make(chan string, 1)
		go func() {
			text, _ := bufio.NewReader(stdout).ReadString('\n')
			line <- text
		}()
		finished := make(chan error, 1)
		go func() { finished <- cmd.Wait() }()
		select {
		case <-delayed.started:
		case err := <-finished:
			t.Fatalf("kubectl exited before deletion admission: %v: %s", err, stderr.String())
		case <-time.After(5 * time.Second):
			t.Fatal("kubectl did not issue DELETE")
		}
		select {
		case text := <-line:
			t.Fatalf("acknowledged before gateway admission: %q", text)
		case <-time.After(250 * time.Millisecond):
		}
		close(delayed.admit)
		select {
		case text := <-line:
			if !strings.Contains(text, `"delete-wait" deleted`) {
				t.Fatalf("unexpected kubectl output: %q", text)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("kubectl did not acknowledge accepted deletion")
		}
		select {
		case err := <-finished:
			t.Fatalf("kubectl exited before cleanup: %v: %s", err, stderr.String())
		case <-time.After(500 * time.Millisecond):
		}
		if _, err := resource.Get(ctx, "delete-wait", metav1.GetOptions{}); err != nil {
			t.Fatalf("object must still exist during cleanup: %v", err)
		}
		close(delayed.release)
		select {
		case err := <-finished:
			if err != nil {
				t.Fatalf("kubectl delete failed: %v: %s", err, stderr.String())
			}
		case <-time.After(5 * time.Second):
			t.Fatal("kubectl did not finish after cleanup")
		}
		if _, err := resource.Get(ctx, "delete-wait", metav1.GetOptions{}); !apierrors.IsNotFound(err) {
			t.Fatalf("sandbox still exists: %v", err)
		}
		obj.SetName("delete-denied")
		if _, err := resource.Create(ctx, obj, metav1.CreateOptions{}); err != nil {
			t.Fatal(err)
		}
		if err := resource.Delete(ctx, "delete-denied", metav1.DeleteOptions{}); !apierrors.IsForbidden(err) {
			t.Fatalf("failed deletion must not be acknowledged: %v", err)
		}
	}
	discoveryClient, err := discovery.NewDiscoveryClientForConfig(&rest.Config{Host: "https://" + listener.Addr().String(), TLSClientConfig: rest.TLSClientConfig{Insecure: true}})
	if err != nil {
		t.Fatal(err)
	}
	resources, err := discoveryClient.ServerResourcesForGroupVersion("openshell.ai/v1alpha1")
	if err != nil {
		t.Fatal(err)
	}
	shortNames := map[string]string{"openshellsandboxes": "oss", "openshellsandboxtemplates": "osst", "openshellproviders": "osp", "openshellworkspaces": "osw"}
	for _, resource := range resources.APIResources {
		if expected, ok := shortNames[resource.Name]; ok {
			if resource.Name == "openshellworkspaces" && (resource.Namespaced || strings.Join(resource.Verbs, ",") != "get,list") {
				t.Errorf("workspace discovery must be cluster-scoped and read-only: %#v", resource)
			}
			if len(resource.ShortNames) != 1 || resource.ShortNames[0] != expected {
				t.Errorf("%s short names = %v, want %s", resource.Name, resource.ShortNames, expected)
			}
			delete(shortNames, resource.Name)
		}
	}
	if len(shortNames) != 0 {
		t.Fatalf("resources missing from discovery: %v", shortNames)
	}
	t.Run("read-only workspace catalog", func(t *testing.T) {
		fake.WorkspaceObjects["team-a"] = &datamodelv1.Workspace{
			Metadata: &datamodelv1.ObjectMeta{Name: "team-a", Id: "workspace-a", ResourceVersion: 7, Labels: map[string]string{"team": "a"}},
			Status:   &datamodelv1.WorkspaceStatus{Phase: datamodelv1.WorkspacePhase_WORKSPACE_PHASE_TERMINATING},
		}
		client := dynamicClient.Resource(schema.GroupVersionResource{Group: "openshell.ai", Version: "v1alpha1", Resource: "openshellworkspaces"})
		item, err := client.Get(ctx, "team-a", metav1.GetOptions{})
		if err != nil {
			t.Fatal(err)
		}
		phase, _, _ := unstructured.NestedString(item.Object, "status", "phase")
		if item.GetNamespace() != "" || item.GetUID() != "workspace-a" || item.GetResourceVersion() != "7" || phase != "Terminating" {
			t.Fatalf("incorrect workspace projection: %#v", item)
		}
		all, err := client.List(ctx, metav1.ListOptions{})
		if err != nil || len(all.Items) != 2 {
			t.Fatalf("workspace list: %#v, %v", all, err)
		}
		selected, err := client.List(ctx, metav1.ListOptions{LabelSelector: "team=a"})
		if err != nil || len(selected.Items) != 1 || selected.Items[0].GetName() != "team-a" {
			t.Fatalf("workspace label filtering: %#v, %v", selected, err)
		}
		page, err := client.List(ctx, metav1.ListOptions{Limit: 1})
		if err != nil || len(page.Items) != 1 || page.GetContinue() == "" {
			t.Fatalf("workspace first page: %#v, %v", page, err)
		}
		next, err := client.List(ctx, metav1.ListOptions{Limit: 1, Continue: page.GetContinue()})
		if err != nil || len(next.Items) != 1 || next.Items[0].GetName() == page.Items[0].GetName() {
			t.Fatalf("workspace next page: %#v, %v", next, err)
		}
		if _, err := client.Get(ctx, "missing", metav1.GetOptions{}); !apierrors.IsNotFound(err) {
			t.Fatalf("missing workspace: %v", err)
		}
		if _, err := client.Get(ctx, "team-a", metav1.GetOptions{ResourceVersion: "1"}); !apierrors.IsBadRequest(err) {
			t.Fatalf("historical read unexpectedly allowed: %v", err)
		}
		if _, err := client.Create(ctx, item, metav1.CreateOptions{}); !apierrors.IsMethodNotSupported(err) {
			t.Fatalf("workspace create must be unsupported: %v", err)
		}
		if _, err := client.Update(ctx, item, metav1.UpdateOptions{}); !apierrors.IsMethodNotSupported(err) {
			t.Fatalf("workspace update must be unsupported: %v", err)
		}
		if _, err := client.Patch(ctx, "team-a", types.MergePatchType, []byte(`{"metadata":{"labels":{"edit":"yes"}}}`), metav1.PatchOptions{}); !apierrors.IsMethodNotSupported(err) {
			t.Fatalf("workspace patch must be unsupported: %v", err)
		}
		if err := client.Delete(ctx, "team-a", metav1.DeleteOptions{}); !apierrors.IsMethodNotSupported(err) {
			t.Fatalf("workspace delete must be unsupported: %v", err)
		}
		if _, err := client.List(ctx, metav1.ListOptions{Watch: true}); err == nil {
			t.Fatal("workspace watch unexpectedly supported")
		}
		if _, err := fake.GetWorkspace(ctx, "team-a"); err != nil {
			t.Fatalf("read-only request deleted the workspace: %v", err)
		}
	})
	conditions, found, err := unstructured.NestedSlice(created.Object, "status", "conditions")
	if err != nil || !found || len(conditions) == 0 {
		t.Fatalf("Ready condition missing: conditions=%#v found=%v err=%v", conditions, found, err)
	}
	if condition, ok := conditions[0].(map[string]any); !ok || condition["type"] != "Ready" {
		t.Fatalf("Ready condition = %#v", conditions[0])
	}
	if len(fake.CreateRequests) != 1 || fake.CreateRequests[0].GetWorkspaceScope().GetWorkspace() != "team-a" {
		t.Fatalf("gateway create request = %#v", fake.CreateRequests)
	}
	current, err := resource.Get(ctx, "worker", metav1.GetOptions{})
	if err != nil {
		t.Fatalf("get through aggregated API: %v", err)
	}
	if _, err := resource.Update(ctx, current, metav1.UpdateOptions{}); err != nil {
		t.Fatalf("no-op update: %v", err)
	}
	t.Run("server-side apply", func(t *testing.T) {
		body := []byte(`{"apiVersion":"openshell.ai/v1alpha1","kind":"OpenShellSandbox","metadata":{"name":"applied","labels":{"owner":"first"}},"spec":{"desiredState":"Running"}}`)
		applied, err := resource.Patch(ctx, "applied", types.ApplyPatchType, body, metav1.PatchOptions{FieldManager: "first"})
		if err != nil {
			t.Fatalf("apply create: %v", err)
		}
		if len(applied.GetManagedFields()) == 0 {
			t.Fatal("apply ownership was not persisted")
		}
		uid := applied.GetUID()
		if _, err := resource.Patch(ctx, "applied", types.ApplyPatchType, body, metav1.PatchOptions{FieldManager: "first"}); err != nil {
			t.Fatalf("apply reapply: %v", err)
		}
		conflicting := []byte(`{"apiVersion":"openshell.ai/v1alpha1","kind":"OpenShellSandbox","metadata":{"name":"applied","labels":{"owner":"second"}}}`)
		if _, err := resource.Patch(ctx, "applied", types.ApplyPatchType, conflicting, metav1.PatchOptions{FieldManager: "second"}); !apierrors.IsConflict(err) {
			t.Fatalf("expected ownership conflict, got %v", err)
		}
		changedSpec := []byte(`{"apiVersion":"openshell.ai/v1alpha1","kind":"OpenShellSandbox","metadata":{"name":"applied"},"spec":{"desiredState":"Running","command":["changed"]}}`)
		if _, err := resource.Patch(ctx, "applied", types.ApplyPatchType, changedSpec, metav1.PatchOptions{FieldManager: "first"}); !apierrors.IsInvalid(err) {
			t.Fatalf("expected immutable spec rejection, got %v", err)
		}
		fake.SetSandboxPhase("team-a/applied", openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPED)
		forced, err := resource.Patch(ctx, "applied", types.ApplyPatchType, conflicting, metav1.PatchOptions{FieldManager: "second", Force: boolPtr(true)})
		if err != nil || forced.GetLabels()["owner"] != "second" || forced.GetUID() != uid {
			t.Fatalf("force ownership: object=%v err=%v", forced, err)
		}
		stale := applied.DeepCopy()
		stale.SetLabels(map[string]string{"stale": "true"})
		if _, err := resource.Update(ctx, stale, metav1.UpdateOptions{}); !apierrors.IsConflict(err) {
			t.Fatalf("expected stale update conflict, got %v", err)
		}
		if err := resource.Delete(ctx, "applied", metav1.DeleteOptions{}); err != nil {
			t.Fatal(err)
		}
	})
	t.Run("kubectl validation and client-side apply", func(t *testing.T) {
		path, err := exec.LookPath("kubectl")
		if err != nil {
			t.Skip("kubectl not installed")
		}
		run := func(body string, args ...string) (string, error) {
			base := []string{"--kubeconfig=/dev/null", "--token=test", "--server=https://" + listener.Addr().String(), "--insecure-skip-tls-verify", "--namespace=team-a"}
			cmd := exec.CommandContext(ctx, path, append(base, args...)...)
			cmd.Stdin = strings.NewReader(body)
			output, err := cmd.CombinedOutput()
			return string(output), err
		}
		manifest := `{"apiVersion":"openshell.ai/v1alpha1","kind":"OpenShellSandbox","metadata":{"name":"client-applied"},"spec":{"desiredState":"Running"}}`
		createdManifest := `{"apiVersion":"openshell.ai/v1alpha1","kind":"OpenShellSandbox","metadata":{"name":"create-then-apply","labels":{"app":"create-test"}},"spec":{"template":{"image":"example/image"},"command":["sh","-c","echo marker > /sandbox/marker"],"restartPolicy":"Never","desiredState":"Running"}}`
		if output, err := run(createdManifest, "create", "-f", "-"); err != nil {
			t.Fatalf("plain kubectl create: %v\n%s", err, output)
		}
		if output, err := run(createdManifest, "apply", "-f", "-"); err != nil || !strings.Contains(output, "unchanged") || strings.Contains(output, "Warning:") {
			t.Fatalf("apply after plain create: %v\n%s", err, output)
		}
		if err := resource.Delete(ctx, "create-then-apply", metav1.DeleteOptions{}); err != nil {
			t.Fatal(err)
		}
		for range 2 {
			if output, err := run(manifest, "apply", "--validate=strict", "-f", "-"); err != nil {
				t.Fatalf("kubectl apply: %v\n%s", err, output)
			}
		}
		invalid := strings.Replace(manifest, `"desiredState":"Running"`, `"desiredState":42`, 1)
		if output, err := run(invalid, "apply", "--validate=strict", "-f", "-"); err == nil {
			t.Fatalf("invalid field type accepted: %s", output)
		}
		invalid = strings.Replace(manifest, `"desiredState":"Running"`, `"unknownField":"value"`, 1)
		if output, err := run(invalid, "apply", "--validate=strict", "-f", "-"); err == nil {
			t.Fatalf("unknown field accepted: %s", output)
		}
		fake.SetSandboxPhase("team-a/client-applied", openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPED)
		updated := strings.Replace(manifest, `"name":"client-applied"`, `"name":"client-applied","labels":{"updated":"yes"}`, 1)
		if output, err := run(updated, "apply", "--validate=strict", "-f", "-"); err != nil {
			t.Fatalf("client-side apply metadata: %v\n%s", err, output)
		}
		saved, err := resource.Get(ctx, "client-applied", metav1.GetOptions{})
		if err != nil {
			t.Fatal(err)
		}
		data, _ := json.Marshal(saved.GetManagedFields())
		if !strings.Contains(string(data), "kubectl-client-side-apply") {
			t.Fatalf("client ownership missing: %s", data)
		}
		if err := resource.Delete(ctx, "client-applied", metav1.DeleteOptions{}); err != nil {
			t.Fatal(err)
		}
	})
	if _, err := resource.List(ctx, metav1.ListOptions{Watch: true}); err == nil || (!strings.Contains(strings.ToLower(err.Error()), "watch") && !strings.Contains(strings.ToLower(err.Error()), "supported")) {
		t.Fatalf("watch should return a clear unsupported error, error = %v", err)
	}

	unknownList, err := dynamicClient.Resource(schema.GroupVersionResource{Group: "openshell.ai", Version: "v1alpha1", Resource: "openshellsandboxes"}).Namespace("unknown-workspace").List(ctx, metav1.ListOptions{})
	if err != nil || len(unknownList.Items) != 0 {
		t.Fatalf("unknown workspace list = %#v, error = %v", unknownList, err)
	}
	list, err := resource.List(ctx, metav1.ListOptions{})
	if err != nil || len(list.Items) != 1 {
		t.Fatalf("list = %#v, error = %v", list, err)
	}
	t.Run("all namespaces", func(t *testing.T) {
		global := dynamicClient.Resource(schema.GroupVersionResource{Group: "openshell.ai", Version: "v1alpha1", Resource: "openshellsandboxes"})
		other := global.Namespace("default")
		_, err := other.Create(ctx, &unstructured.Unstructured{Object: map[string]any{
			"apiVersion": "openshell.ai/v1alpha1", "kind": "OpenShellSandbox",
			"metadata": map[string]any{"name": "worker", "labels": map[string]any{"scope": "other"}},
		}}, metav1.CreateOptions{})
		if err != nil {
			t.Fatal(err)
		}
		defer func() {
			if err := other.Delete(ctx, "worker", metav1.DeleteOptions{}); err != nil {
				t.Error(err)
			}
		}()
		all, err := global.List(ctx, metav1.ListOptions{})
		if err != nil || len(all.Items) != 2 {
			t.Fatalf("all-namespace list: %#v, error = %v", all, err)
		}
		namespaces := map[string]bool{}
		for _, obj := range all.Items {
			namespaces[obj.GetNamespace()] = true
		}
		if !namespaces["default"] || !namespaces["team-a"] {
			t.Fatalf("workspace namespaces not preserved: %v", namespaces)
		}
		selected, err := global.List(ctx, metav1.ListOptions{LabelSelector: "scope=other"})
		if err != nil || len(selected.Items) != 1 || selected.Items[0].GetNamespace() != "default" {
			t.Fatalf("all-namespace selector: %#v, error = %v", selected, err)
		}
		for _, namespace := range []string{"default", "team-a"} {
			meta := &datamodelv1.ObjectMeta{Name: "shared-name", Workspace: namespace, Labels: map[string]string{"scope": namespace}}
			fake.SeedProvider(namespace, &datamodelv1.Provider{Metadata: meta})
			fake.SeedTemplate(namespace, &openshellv1.SandboxWorkloadTemplate{Metadata: meta})
		}
		for _, name := range []string{"openshellproviders", "openshellsandboxtemplates"} {
			client := dynamicClient.Resource(schema.GroupVersionResource{Group: "openshell.ai", Version: "v1alpha1", Resource: name})
			all, err := client.List(ctx, metav1.ListOptions{})
			if err != nil || len(all.Items) != 2 {
				t.Fatalf("%s all-namespace list: %#v, error = %v", name, all, err)
			}
			namespaces := map[string]bool{}
			for _, obj := range all.Items {
				namespaces[obj.GetNamespace()] = true
			}
			if !namespaces["default"] || !namespaces["team-a"] {
				t.Fatalf("%s lost namespaces: %v", name, namespaces)
			}
			selected, err := client.List(ctx, metav1.ListOptions{LabelSelector: "scope=team-a"})
			if err != nil || len(selected.Items) != 1 || selected.Items[0].GetNamespace() != "team-a" {
				t.Fatalf("%s filtered list: %#v, error = %v", name, selected, err)
			}
			scoped, err := client.Namespace("default").List(ctx, metav1.ListOptions{})
			if err != nil || len(scoped.Items) != 1 || scoped.Items[0].GetNamespace() != "default" {
				t.Fatalf("%s namespace list: %#v, error = %v", name, scoped, err)
			}
		}
	})
	if err := resource.Delete(ctx, "worker", metav1.DeleteOptions{}); err != nil {
		t.Fatalf("delete through aggregated API: %v", err)
	}
	t.Run("kubectl acknowledges durable deletion before waiting", testDeletion)
	if _, err := resource.Get(ctx, "worker", metav1.GetOptions{}); err == nil {
		t.Fatal("get after delete unexpectedly succeeded")
	}
}

func boolPtr(value bool) *bool { return &value }
