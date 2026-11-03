// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package workspace

import (
	"context"
	"testing"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/fields"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/watch"
)

type snapshot struct {
	items []*datamodelv1.Workspace
	err   error
}

type workspaceGateway struct {
	gateway.Interface
	next      chan snapshot
	entered   chan struct{}
	cancelled chan struct{}
}

func (g *workspaceGateway) ListWorkspaces(ctx context.Context) ([]*datamodelv1.Workspace, error) {
	if g.entered != nil {
		g.entered <- struct{}{}
	}
	select {
	case result := <-g.next:
		return result.items, result.err
	case <-ctx.Done():
		if g.cancelled != nil {
			close(g.cancelled)
		}
		return nil, ctx.Err()
	}
}

func workspaceItem(uid string, phase datamodelv1.WorkspacePhase, team string) *datamodelv1.Workspace {
	return &datamodelv1.Workspace{
		Metadata: &datamodelv1.ObjectMeta{Name: "team-a", Id: uid, ResourceVersion: 1, Labels: map[string]string{"team": team}},
		Status:   &datamodelv1.WorkspaceStatus{Phase: phase},
	}
}

func receive(t *testing.T, stream watch.Interface, kind watch.EventType, uid, phase string) {
	t.Helper()
	select {
	case event, open := <-stream.ResultChan():
		item, ok := event.Object.(*api.OpenShellWorkspace)
		if !open || !ok || event.Type != kind || string(item.UID) != uid || item.Status.Phase != phase {
			t.Fatalf("want %s %s %s, got %#v", kind, uid, phase, event)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for workspace event")
	}
}

func TestWorkspaceWatchChanges(t *testing.T) {
	active := workspaceItem("one", datamodelv1.WorkspacePhase_WORKSPACE_PHASE_ACTIVE, "a")
	terminating := workspaceItem("one", datamodelv1.WorkspacePhase_WORKSPACE_PHASE_TERMINATING, "a")
	replacement := workspaceItem("two", datamodelv1.WorkspacePhase_WORKSPACE_PHASE_ACTIVE, "a")
	filtered := workspaceItem("two", datamodelv1.WorkspacePhase_WORKSPACE_PHASE_ACTIVE, "b")
	g := &workspaceGateway{next: make(chan snapshot, 1)}
	g.next <- snapshot{items: []*datamodelv1.Workspace{active}}
	stream, err := New(g).Watch(context.Background(), &metainternalversion.ListOptions{
		ResourceVersion: "0", LabelSelector: labels.SelectorFromSet(labels.Set{"team": "a"}),
		FieldSelector: fields.OneTermEqualSelector("metadata.name", "team-a"),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Stop()
	receive(t, stream, watch.Added, "one", "Active")
	g.next <- snapshot{items: []*datamodelv1.Workspace{active}}
	// No event for an unchanged snapshot, even if phase changes without an RV bump.
	g.next <- snapshot{items: []*datamodelv1.Workspace{terminating}}
	receive(t, stream, watch.Modified, "one", "Terminating")
	g.next <- snapshot{items: []*datamodelv1.Workspace{replacement}}
	receive(t, stream, watch.Deleted, "one", "Terminating")
	receive(t, stream, watch.Added, "two", "Active")
	g.next <- snapshot{items: []*datamodelv1.Workspace{filtered}}
	receive(t, stream, watch.Deleted, "two", "Active")
	g.next <- snapshot{items: []*datamodelv1.Workspace{replacement}}
	receive(t, stream, watch.Added, "two", "Active")
	g.next <- snapshot{}
	receive(t, stream, watch.Deleted, "two", "Active")
	g.next <- snapshot{err: status.Error(codes.Unavailable, "gateway offline")}
	select {
	case event := <-stream.ResultChan():
		result, ok := event.Object.(*metav1.Status)
		if !ok || event.Type != watch.Error || result.Code != 503 {
			t.Fatalf("gateway error event: %#v", event)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("missing gateway error event")
	}
	select {
	case _, open := <-stream.ResultChan():
		if open {
			t.Fatal("watch must end after gateway error")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("watch did not end after gateway error")
	}
}

func TestWorkspaceWatchValidationAndStop(t *testing.T) {
	s := New(gateway.NewFake())
	if _, err := s.Watch(context.Background(), &metainternalversion.ListOptions{ResourceVersion: "5"}); !apierrors.IsResourceExpired(err) {
		t.Fatalf("historical watch: %v", err)
	}
	for _, options := range []*metainternalversion.ListOptions{
		{Limit: 1}, {Continue: "token"},
		{FieldSelector: fields.OneTermEqualSelector("status.phase", "Active")},
	} {
		if _, err := s.Watch(context.Background(), options); !apierrors.IsBadRequest(err) {
			t.Fatalf("unsupported watch options %#v: %v", options, err)
		}
	}
	g := &workspaceGateway{next: make(chan snapshot, 1), entered: make(chan struct{}, 2), cancelled: make(chan struct{})}
	g.next <- snapshot{}
	stream, err := New(g).Watch(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	<-g.entered
	select {
	case <-g.entered:
	case <-time.After(5 * time.Second):
		t.Fatal("watch did not poll gateway")
	}
	stream.Stop()
	select {
	case <-g.cancelled:
	case <-time.After(5 * time.Second):
		t.Fatal("Stop did not cancel in-flight gateway poll")
	}
	select {
	case _, open := <-stream.ResultChan():
		if open {
			t.Fatal("stopped watch must close its event channel")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("stopped watch did not close")
	}
}

func TestWorkspaceListNameSelection(t *testing.T) {
	g := gateway.NewFake()
	g.EnsureWorkspace("team-a")
	s := New(g)
	for _, name := range []string{"team-a", "missing"} {
		obj, err := s.List(context.Background(), &metainternalversion.ListOptions{FieldSelector: fields.OneTermEqualSelector("metadata.name", name)})
		if err != nil {
			t.Fatal(err)
		}
		items := obj.(*api.OpenShellWorkspaceList).Items
		if (name == "missing" && len(items) != 0) || (name == "team-a" && (len(items) != 1 || items[0].Name != name)) {
			t.Fatalf("name selection %s: %#v", name, items)
		}
	}
}
