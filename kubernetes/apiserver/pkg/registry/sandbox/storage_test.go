// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package sandbox

import (
	"context"
	"strings"
	"testing"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/convert"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/fields"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/watch"
	"k8s.io/apiserver/pkg/endpoints/request"
	"k8s.io/apiserver/pkg/registry/rest"
)

type absentBeforeDeleteReturns struct {
	gateway.Interface
	release chan struct{}
	done    chan struct{}
}

func (g *absentBeforeDeleteReturns) DeleteSandbox(ctx context.Context, workspace, name, requestID string) error {
	defer close(g.done)
	if err := g.Interface.DeleteSandbox(ctx, workspace, name, requestID); err != nil {
		return err
	}
	select {
	case <-g.release:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func TestDeleteAcknowledgesAbsenceWhileRPCFinishes(t *testing.T) {
	fake := gateway.NewFake()
	fake.SeedSandbox("default", &openshellv1.Sandbox{Metadata: &datamodelv1.ObjectMeta{Id: "original", Name: "worker", Workspace: "default"}})
	backend := &absentBeforeDeleteReturns{Interface: fake, release: make(chan struct{}), done: make(chan struct{})}
	defer close(backend.release)
	ctx, cancel := context.WithTimeout(request.WithNamespace(context.Background(), "default"), 3*time.Second)
	defer cancel()
	obj, deleted, err := New(backend).Delete(ctx, "worker", nil, nil)
	if err != nil || !deleted || obj.(*api.OpenShellSandbox).UID != "original" {
		t.Fatalf("absent sandbox should be acknowledged: object=%v deleted=%v err=%v", obj, deleted, err)
	}
	select {
	case <-backend.done:
		t.Fatal("RPC must still be finishing after HTTP acknowledgement")
	default:
	}
}

func TestDeletionWatchKeepsOriginalIdentity(t *testing.T) {
	fake := gateway.NewFake()
	fake.SeedSandbox("default", &openshellv1.Sandbox{Metadata: &datamodelv1.ObjectMeta{Id: "original", Name: "worker", Workspace: "default"}})
	storage := New(fake)
	ctx, cancel := context.WithTimeout(request.WithNamespace(context.Background(), "default"), 5*time.Second)
	defer cancel()
	options := &metainternalversion.ListOptions{FieldSelector: fields.OneTermEqualSelector("metadata.name", "worker"), ResourceVersion: "0"}
	listed, err := storage.List(ctx, options)
	if err != nil {
		t.Fatal(err)
	}
	options.ResourceVersion = listed.(*api.OpenShellSandboxList).ResourceVersion
	if _, err := storage.List(ctx, options); err != nil {
		t.Fatalf("reflector relist with deletion cursor: %v", err)
	}
	// Recreate between LIST and WATCH: the old UID, not the new sandbox,
	// determines when kubectl finishes waiting.
	fake.SeedSandbox("default", &openshellv1.Sandbox{Metadata: &datamodelv1.ObjectMeta{Id: "replacement", Name: "worker", Workspace: "default"}})
	relisted, err := storage.List(ctx, options)
	if err != nil {
		t.Fatal(err)
	}
	if list := relisted.(*api.OpenShellSandboxList); len(list.Items) != 0 || list.ResourceVersion != options.ResourceVersion {
		t.Fatalf("relist switched deletion tracking to the replacement: %#v", list)
	}
	w, err := storage.Watch(ctx, options)
	if err != nil {
		t.Fatal(err)
	}
	defer w.Stop()
	select {
	case event := <-w.ResultChan():
		if event.Type != watch.Deleted || event.Object.(*api.OpenShellSandbox).UID != "original" {
			t.Fatalf("wrong deleted identity: %#v", event)
		}
	case <-ctx.Done():
		t.Fatal("watch did not observe the original UID disappearing")
	}
	wrongNamespace := request.WithNamespace(ctx, "another")
	if _, err := storage.Watch(wrongNamespace, options); !apierrors.IsBadRequest(err) {
		t.Fatalf("cursor must be namespace-scoped: %v", err)
	}
}

func TestCreatePropagatesNamespaceAsWorkspace(t *testing.T) {
	fake := gateway.NewFake()
	fake.EnsureWorkspace("team-a")
	storage := New(fake)
	ctx := request.WithNamespace(context.Background(), "team-a")
	created, err := storage.Create(ctx, &api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: "worker"}, Spec: api.OpenShellSandboxSpec{DesiredState: "Running"}}, nil, nil)
	if err != nil {
		t.Fatalf("Create() error = %v", err)
	}
	if len(fake.CreateRequests) != 1 {
		t.Fatalf("create calls = %d", len(fake.CreateRequests))
	}
	if got := fake.CreateRequests[0].GetWorkspaceScope().GetWorkspace(); got != "team-a" {
		t.Fatalf("gateway workspace = %q", got)
	}
	if got := created.(*api.OpenShellSandbox).Namespace; got != "team-a" {
		t.Fatalf("returned namespace = %q", got)
	}
}

func TestCreateRejectsDryRunWithoutGatewayMutation(t *testing.T) {
	fake := gateway.NewFake()
	fake.EnsureWorkspace("team-a")
	storage := New(fake)
	ctx := request.WithNamespace(context.Background(), "team-a")
	_, err := storage.Create(ctx, &api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: "worker"}}, nil, &metav1.CreateOptions{DryRun: []string{metav1.DryRunAll}})
	if err == nil {
		t.Fatal("expected dry-run rejection")
	}
	if len(fake.CreateRequests) != 0 || len(fake.Sandboxes) != 0 {
		t.Fatal("dry-run mutated the fake gateway")
	}
}

func TestDeleteRejectsUnsupportedPreconditions(t *testing.T) {
	fake := gateway.NewFake()
	fake.EnsureWorkspace("team-a")
	fake.SeedSandbox("team-a", &openshellv1.Sandbox{Metadata: &datamodelv1.ObjectMeta{Id: "id", Name: "worker", Workspace: "team-a"}})
	storage := New(fake)
	ctx := request.WithNamespace(context.Background(), "team-a")
	uid := types.UID("id")
	_, _, err := storage.Delete(ctx, "worker", nil, &metav1.DeleteOptions{Preconditions: &metav1.Preconditions{UID: &uid}})
	if err == nil {
		t.Fatal("expected precondition rejection")
	}
	if _, err := fake.GetSandbox(ctx, "team-a", "worker"); err != nil {
		t.Fatalf("sandbox was deleted despite rejected precondition: %v", err)
	}
}

func TestProjectionPersistenceAndBounds(t *testing.T) {
	ctx := request.WithNamespace(context.Background(), "default")
	fake := gateway.NewFake()
	input := &api.OpenShellSandbox{
		ObjectMeta: metav1.ObjectMeta{Name: "worker", ManagedFields: []metav1.ManagedFieldsEntry{
			{Manager: "example", Operation: metav1.ManagedFieldsOperationApply, APIVersion: api.SchemeGroupVersion.String(), FieldsType: "FieldsV1", FieldsV1: &metav1.FieldsV1{Raw: []byte(`{"f:spec":{"f:workloadTemplate":{}}}`)}},
		}},
		Spec: api.OpenShellSandboxSpec{WorkloadTemplate: "named-template"},
	}
	if _, err := New(fake).Create(ctx, input, nil, nil); err != nil {
		t.Fatal(err)
	}
	fake.Sandboxes["default/worker"].Spec.Template = &openshellv1.SandboxTemplate{Image: "gateway-expanded-image"}
	restored, err := New(fake).Get(ctx, "worker", nil)
	if err != nil {
		t.Fatal(err)
	}
	obj := restored.(*api.OpenShellSandbox)
	if obj.Spec.WorkloadTemplate != "named-template" || obj.Spec.Template != nil || len(obj.ManagedFields) != 1 || obj.Generation != 1 {
		t.Fatalf("authored form or ownership lost on new storage: %#v", obj)
	}
	if _, exposed := obj.Annotations[convert.ProjectionAnnotation]; exposed {
		t.Fatal("internal projection exposed to Kubernetes clients")
	}
	for _, test := range []struct {
		name        string
		spec        api.OpenShellSandboxSpec
		annotations map[string]string
	}{
		{name: "reserved", annotations: map[string]string{convert.ProjectionAnnotation: "{}"}},
		{name: "oversized", spec: api.OpenShellSandboxSpec{Environment: map[string]string{"LARGE": strings.Repeat("x", 8192)}}},
	} {
		t.Run(test.name, func(t *testing.T) {
			_, err := New(fake).Create(ctx, &api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: test.name, Annotations: test.annotations}, Spec: test.spec}, nil, nil)
			if !apierrors.IsInvalid(err) || len(fake.CreateRequests) != 1 {
				t.Fatalf("expected rejection before gateway mutation: err=%v requests=%d", err, len(fake.CreateRequests))
			}
		})
	}
	fake.Sandboxes["default/worker"].Metadata.Annotations[convert.ProjectionAnnotation] = "invalid JSON"
	if _, err := New(fake).Get(ctx, "worker", nil); !apierrors.IsInternalError(err) {
		t.Fatalf("corrupted projection must fail closed: %v", err)
	}
}

type failingUpdater struct {
	gateway.Interface
	err error
}

func (g failingUpdater) UpdateSandbox(context.Context, *openshellv1.UpdateSandboxRequest) (*openshellv1.Sandbox, error) {
	return nil, g.err
}

func TestUpdateFailuresPreserveGatewayState(t *testing.T) {
	ctx := request.WithNamespace(context.Background(), "default")
	fake := gateway.NewFake()
	created, err := New(fake).Create(ctx, &api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: "worker"}}, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name  string
		code  codes.Code
		check func(error) bool
	}{
		{name: "gateway CAS conflict", code: codes.Aborted, check: apierrors.IsConflict},
		{name: "old gateway", code: codes.Unimplemented, check: apierrors.IsBadRequest},
		{name: "running sandbox", code: codes.FailedPrecondition, check: apierrors.IsBadRequest},
	} {
		t.Run(test.name, func(t *testing.T) {
			next := created.DeepCopyObject().(*api.OpenShellSandbox)
			next.Labels = map[string]string{"updated": "yes"}
			backend := failingUpdater{Interface: fake, err: status.Error(test.code, test.name)}
			_, _, err := New(backend).Update(ctx, "worker", rest.DefaultUpdatedObjectInfo(next), nil, nil, false, nil)
			if !test.check(err) {
				t.Fatalf("wrong API error: %v", err)
			}
		})
	}
	for _, test := range []struct {
		name    string
		change  func(*api.OpenShellSandbox)
		options *metav1.UpdateOptions
	}{
		{name: "dry-run", options: &metav1.UpdateOptions{DryRun: []string{metav1.DryRunAll}}, change: func(obj *api.OpenShellSandbox) { obj.Labels = map[string]string{"dryrun": "yes"} }},
		{name: "deletionTimestamp", change: func(obj *api.OpenShellSandbox) { now := metav1.Now(); obj.DeletionTimestamp = &now }},
	} {
		t.Run(test.name, func(t *testing.T) {
			next := created.DeepCopyObject().(*api.OpenShellSandbox)
			test.change(next)
			if _, _, err := New(fake).Update(ctx, "worker", rest.DefaultUpdatedObjectInfo(next), nil, nil, false, test.options); err == nil {
				t.Fatal("unsupported update succeeded")
			}
		})
	}
	if len(fake.UpdateRequests) != 0 || len(fake.Sandboxes["default/worker"].Metadata.Labels) != 0 {
		t.Fatal("rejected update mutated gateway state")
	}
}
