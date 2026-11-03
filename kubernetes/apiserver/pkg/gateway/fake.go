// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package gateway

import (
	"context"
	"fmt"
	"sync"

	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/timestamppb"
)

// Fake is a thread-safe in-memory gateway for tests and local development.
type Fake struct {
	mu               sync.RWMutex
	Sandboxes        map[string]*openshellv1.Sandbox
	Templates        map[string]*openshellv1.SandboxWorkloadTemplate
	Providers        map[string]*datamodelv1.Provider
	Workspaces       map[string]bool
	WorkspaceObjects map[string]*datamodelv1.Workspace
	CreateRequests   []*openshellv1.CreateSandboxRequest
	UpdateRequests   []*openshellv1.UpdateSandboxRequest
}

func (f *Fake) UpdateSandbox(_ context.Context, request *openshellv1.UpdateSandboxRequest) (*openshellv1.Sandbox, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	item, ok := f.Sandboxes[request.GetWorkspaceScope().GetWorkspace()+"/"+request.GetName()]
	if !ok {
		return nil, status.Error(codes.NotFound, "sandbox not found")
	}
	if item.GetMetadata().GetResourceVersion() != request.GetExpectedResourceVersion() {
		return nil, status.Error(codes.Aborted, "resource version conflict")
	}
	if item.GetStatus().GetPhase() != openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPED {
		return nil, status.Error(codes.FailedPrecondition, "sandbox must be stopped before updating")
	}
	f.UpdateRequests = append(f.UpdateRequests, request)
	item.Metadata.Labels = request.GetLabels().GetValues()
	item.Metadata.Annotations = request.GetAnnotations().GetValues()
	item.Metadata.ResourceVersion++
	return item, nil
}

func NewFake() *Fake {
	return &Fake{Sandboxes: map[string]*openshellv1.Sandbox{}, Templates: map[string]*openshellv1.SandboxWorkloadTemplate{}, Providers: map[string]*datamodelv1.Provider{}, Workspaces: map[string]bool{"default": true}, WorkspaceObjects: map[string]*datamodelv1.Workspace{}}
}

func fakeWorkspace(name string) *datamodelv1.Workspace {
	return &datamodelv1.Workspace{Metadata: &datamodelv1.ObjectMeta{Name: name, Id: "workspace-" + name, ResourceVersion: 1}, Status: &datamodelv1.WorkspaceStatus{Phase: datamodelv1.WorkspacePhase_WORKSPACE_PHASE_ACTIVE}}
}

func (f *Fake) GetWorkspace(_ context.Context, name string) (*datamodelv1.Workspace, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if item := f.WorkspaceObjects[name]; item != nil {
		return item, nil
	}
	if !f.Workspaces[name] || name == "*" {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", name)
	}
	return fakeWorkspace(name), nil
}

func (f *Fake) ListWorkspaces(_ context.Context) ([]*datamodelv1.Workspace, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	var items []*datamodelv1.Workspace
	for name, exists := range f.Workspaces {
		if !exists || name == "*" {
			continue
		}
		item := f.WorkspaceObjects[name]
		if item == nil {
			item = fakeWorkspace(name)
		}
		items = append(items, item)
	}
	return items, nil
}

func (f *Fake) SetSandboxPhase(key string, phase openshellv1.SandboxPhase) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Sandboxes[key].Status = &openshellv1.SandboxStatus{Phase: phase}
}

func (f *Fake) CreateSandbox(_ context.Context, request *openshellv1.CreateSandboxRequest) (*openshellv1.Sandbox, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	workspace := request.GetWorkspaceScope().GetWorkspace()
	if !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	f.CreateRequests = append(f.CreateRequests, request)
	key := workspace + "/" + request.GetName()
	if _, ok := f.Sandboxes[key]; ok {
		return nil, status.Errorf(codes.AlreadyExists, "sandbox %q already exists", request.GetName())
	}
	sandbox := &openshellv1.Sandbox{Metadata: &datamodelv1.ObjectMeta{Id: key, Name: request.GetName(), Workspace: workspace, ResourceVersion: 1, Labels: request.GetLabels(), Annotations: request.GetAnnotations(), CreatedTime: timestamppb.Now()}, Spec: request.GetSpec()}
	f.Sandboxes[key] = sandbox
	return sandbox, nil
}

func (f *Fake) GetSandbox(_ context.Context, workspace, name string) (*openshellv1.Sandbox, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	sandbox, ok := f.Sandboxes[workspace+"/"+name]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "sandbox %q not found", name)
	}
	return proto.Clone(sandbox).(*openshellv1.Sandbox), nil
}

func (f *Fake) ListSandboxes(_ context.Context, workspace string) ([]*openshellv1.Sandbox, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if workspace != "" && !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	var result []*openshellv1.Sandbox
	for key, sandbox := range f.Sandboxes {
		if workspace == "" || (len(key) > len(workspace) && key[:len(workspace)+1] == workspace+"/") {
			result = append(result, sandbox)
		}
	}
	return result, nil
}

func (f *Fake) DeleteSandbox(_ context.Context, workspace, name, _ string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if !f.workspaceExists(workspace) {
		return status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	key := workspace + "/" + name
	if _, ok := f.Sandboxes[key]; !ok {
		return status.Errorf(codes.NotFound, "sandbox %q not found", name)
	}
	delete(f.Sandboxes, key)
	return nil
}

func (f *Fake) GetTemplate(_ context.Context, workspace, name string) (*openshellv1.SandboxWorkloadTemplate, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	item, ok := f.Templates[workspace+"/"+name]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "template %q not found", name)
	}
	return item, nil
}

func (f *Fake) ListTemplates(_ context.Context, workspace string) ([]*openshellv1.SandboxWorkloadTemplate, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if workspace != "" && !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	var result []*openshellv1.SandboxWorkloadTemplate
	for key, item := range f.Templates {
		if workspace == "" || (len(key) > len(workspace) && key[:len(workspace)+1] == workspace+"/") {
			result = append(result, item)
		}
	}
	return result, nil
}

func (f *Fake) GetProvider(_ context.Context, workspace, name string) (*datamodelv1.Provider, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	item, ok := f.Providers[workspace+"/"+name]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "provider %q not found", name)
	}
	return item, nil
}

func (f *Fake) ListProviders(_ context.Context, workspace string) ([]*datamodelv1.Provider, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if workspace != "" && !f.workspaceExists(workspace) {
		return nil, status.Errorf(codes.NotFound, "workspace %q not found", workspace)
	}
	var result []*datamodelv1.Provider
	for key, item := range f.Providers {
		if workspace == "" || (len(key) > len(workspace) && key[:len(workspace)+1] == workspace+"/") {
			result = append(result, item)
		}
	}
	return result, nil
}

func (f *Fake) SeedSandbox(workspace string, sandbox *openshellv1.Sandbox) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Sandboxes[workspace+"/"+sandbox.GetMetadata().GetName()] = sandbox
}

func (f *Fake) SeedTemplate(workspace string, template *openshellv1.SandboxWorkloadTemplate) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Templates[workspace+"/"+template.GetMetadata().GetName()] = template
}

func (f *Fake) SeedProvider(workspace string, provider *datamodelv1.Provider) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Providers[workspace+"/"+provider.GetMetadata().GetName()] = provider
}

func (f *Fake) EnsureWorkspace(name string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Workspaces[name] = true
}
func (f *Fake) DebugString() string {
	f.mu.RLock()
	defer f.mu.RUnlock()
	return fmt.Sprintf("fake gateway: %d sandboxes", len(f.Sandboxes))
}

func (f *Fake) workspaceExists(name string) bool { return f.Workspaces[name] || f.Workspaces["*"] }
