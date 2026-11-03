// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package gateway

import (
	"context"
	"net"
	"testing"

	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/test/bufconn"
)

type listServer struct {
	openshellv1.UnimplementedOpenShellServer
	t         *testing.T
	workspace string
}

func (s *listServer) check(scope *datamodelv1.WorkspaceSelector, token string) string {
	s.t.Helper()
	if s.workspace == "" {
		if scope.GetAllWorkspaces() == nil {
			s.t.Error("all-namespace request did not select all_workspaces")
		}
	} else if scope.GetWorkspace() != s.workspace {
		s.t.Errorf("workspace = %q, want %q", scope.GetWorkspace(), s.workspace)
	}
	switch token {
	case "":
		return "next"
	case "next":
		return ""
	default:
		s.t.Errorf("unexpected page token: %q", token)
		return ""
	}
}

func (s *listServer) ListSandboxes(_ context.Context, req *openshellv1.ListSandboxesRequest) (*openshellv1.ListSandboxesResponse, error) {
	return &openshellv1.ListSandboxesResponse{Sandboxes: []*openshellv1.Sandbox{{}}, NextPageToken: s.check(req.WorkspaceScope, req.PageToken)}, nil
}

func (s *listServer) ListProviders(_ context.Context, req *openshellv1.ListProvidersRequest) (*openshellv1.ListProvidersResponse, error) {
	return &openshellv1.ListProvidersResponse{Providers: []*datamodelv1.Provider{{}}, NextPageToken: s.check(req.WorkspaceScope, req.PageToken)}, nil
}

func (s *listServer) ListSandboxTemplates(_ context.Context, req *openshellv1.ListSandboxTemplatesRequest) (*openshellv1.ListSandboxTemplatesResponse, error) {
	return &openshellv1.ListSandboxTemplatesResponse{Templates: []*openshellv1.SandboxWorkloadTemplate{{}}, NextPageToken: s.check(req.WorkspaceScope, req.PageToken)}, nil
}

func (s *listServer) GetWorkspace(_ context.Context, req *openshellv1.GetWorkspaceRequest) (*openshellv1.GetWorkspaceResponse, error) {
	if req.Name != "team-a" {
		s.t.Errorf("workspace name = %q", req.Name)
	}
	return &openshellv1.GetWorkspaceResponse{Workspace: fakeWorkspace(req.Name)}, nil
}

func (s *listServer) ListWorkspaces(_ context.Context, req *openshellv1.ListWorkspacesRequest) (*openshellv1.ListWorkspacesResponse, error) {
	if req.PageSize != 1000 {
		s.t.Errorf("workspace page size = %d", req.PageSize)
	}
	switch req.PageToken {
	case "":
		return &openshellv1.ListWorkspacesResponse{Workspaces: []*datamodelv1.Workspace{fakeWorkspace("default")}, NextPageToken: "second"}, nil
	case "second":
		return &openshellv1.ListWorkspacesResponse{Workspaces: []*datamodelv1.Workspace{fakeWorkspace("team-a")}}, nil
	default:
		s.t.Errorf("unexpected workspace page token: %q", req.PageToken)
		return &openshellv1.ListWorkspacesResponse{}, nil
	}
}

func TestListWorkspaceScopesAndPagination(t *testing.T) {
	for _, workspace := range []string{"", "team-a"} {
		t.Run("workspace="+workspace, func(t *testing.T) {
			listener := bufconn.Listen(1 << 20)
			server := grpc.NewServer()
			openshellv1.RegisterOpenShellServer(server, &listServer{t: t, workspace: workspace})
			go func() { _ = server.Serve(listener) }()
			defer server.Stop()
			conn, err := grpc.NewClient("passthrough:///gateway", grpc.WithTransportCredentials(insecure.NewCredentials()),
				grpc.WithContextDialer(func(context.Context, string) (net.Conn, error) { return listener.Dial() }))
			if err != nil {
				t.Fatal(err)
			}
			defer conn.Close()
			g := &GRPC{client: openshellv1.NewOpenShellClient(conn), conn: conn}
			ctx := context.Background()
			sandboxes, err := g.ListSandboxes(ctx, workspace)
			if err != nil || len(sandboxes) != 2 {
				t.Fatalf("sandboxes: %d, %v", len(sandboxes), err)
			}
			providers, err := g.ListProviders(ctx, workspace)
			if err != nil || len(providers) != 2 {
				t.Fatalf("providers: %d, %v", len(providers), err)
			}
			templates, err := g.ListTemplates(ctx, workspace)
			if err != nil || len(templates) != 2 {
				t.Fatalf("templates: %d, %v", len(templates), err)
			}
			workspaces, err := g.ListWorkspaces(ctx)
			if err != nil || len(workspaces) != 2 || workspaces[1].GetMetadata().GetName() != "team-a" {
				t.Fatalf("workspaces: %v, %v", workspaces, err)
			}
			item, err := g.GetWorkspace(ctx, "team-a")
			if err != nil || item.GetMetadata().GetName() != "team-a" {
				t.Fatalf("workspace: %v, %v", item, err)
			}
		})
	}
}
