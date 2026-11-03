// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package gateway

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"fmt"
	"os"
	"strings"

	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
)

// Interface is the gateway surface needed by the aggregated API.
type Interface interface {
	GetWorkspace(context.Context, string) (*datamodelv1.Workspace, error)
	ListWorkspaces(context.Context) ([]*datamodelv1.Workspace, error)
	CreateSandbox(context.Context, *openshellv1.CreateSandboxRequest) (*openshellv1.Sandbox, error)
	UpdateSandbox(context.Context, *openshellv1.UpdateSandboxRequest) (*openshellv1.Sandbox, error)
	GetSandbox(context.Context, string, string) (*openshellv1.Sandbox, error)
	ListSandboxes(context.Context, string) ([]*openshellv1.Sandbox, error)
	DeleteSandbox(context.Context, string, string, string) error
	GetTemplate(context.Context, string, string) (*openshellv1.SandboxWorkloadTemplate, error)
	ListTemplates(context.Context, string) ([]*openshellv1.SandboxWorkloadTemplate, error)
	GetProvider(context.Context, string, string) (*datamodelv1.Provider, error)
	ListProviders(context.Context, string) ([]*datamodelv1.Provider, error)
}

type GRPC struct {
	client openshellv1.OpenShellClient
	conn   *grpc.ClientConn
}

type TLSOptions struct {
	CAFile   string
	CertFile string
	KeyFile  string
}

func Dial(ctx context.Context, address string, tlsOptions TLSOptions, tokenFile string) (*GRPC, error) {
	var transport grpc.DialOption
	if tlsOptions.CAFile != "" || tlsOptions.CertFile != "" || tlsOptions.KeyFile != "" {
		if (tlsOptions.CertFile == "") != (tlsOptions.KeyFile == "") {
			return nil, fmt.Errorf("client certificate and key must be provided together")
		}
		tlsConfig := &tls.Config{MinVersion: tls.VersionTLS12}
		if tlsOptions.CAFile != "" {
			caPEM, err := os.ReadFile(tlsOptions.CAFile)
			if err != nil {
				return nil, fmt.Errorf("read gateway CA: %w", err)
			}
			pool := x509.NewCertPool()
			if !pool.AppendCertsFromPEM(caPEM) {
				return nil, fmt.Errorf("gateway CA file contains no certificates")
			}
			tlsConfig.RootCAs = pool
		}
		if tlsOptions.CertFile != "" {
			cert, err := tls.LoadX509KeyPair(tlsOptions.CertFile, tlsOptions.KeyFile)
			if err != nil {
				return nil, fmt.Errorf("load gateway client certificate: %w", err)
			}
			tlsConfig.Certificates = []tls.Certificate{cert}
		}
		transport = grpc.WithTransportCredentials(credentials.NewTLS(tlsConfig))
	} else {
		transport = grpc.WithTransportCredentials(insecure.NewCredentials())
	}

	options := []grpc.DialOption{transport}
	if tokenFile != "" {
		options = append(options, grpc.WithUnaryInterceptor(bearerTokenInterceptor(tokenFile)))
	}
	conn, err := grpc.NewClient(address, options...)
	if err != nil {
		return nil, fmt.Errorf("connect to gateway: %w", err)
	}
	return &GRPC{client: openshellv1.NewOpenShellClient(conn), conn: conn}, nil
}

func (g *GRPC) Close() error { return g.conn.Close() }

func (g *GRPC) GetWorkspace(ctx context.Context, name string) (*datamodelv1.Workspace, error) {
	response, err := g.client.GetWorkspace(ctx, &openshellv1.GetWorkspaceRequest{Name: name})
	if err != nil {
		return nil, err
	}
	return response.GetWorkspace(), nil
}

func (g *GRPC) ListWorkspaces(ctx context.Context) ([]*datamodelv1.Workspace, error) {
	var all []*datamodelv1.Workspace
	token := ""
	for {
		response, err := g.client.ListWorkspaces(ctx, &openshellv1.ListWorkspacesRequest{PageSize: 1000, PageToken: token})
		if err != nil {
			return nil, err
		}
		all = append(all, response.GetWorkspaces()...)
		token = response.GetNextPageToken()
		if token == "" {
			return all, nil
		}
	}
}

func (g *GRPC) CreateSandbox(ctx context.Context, request *openshellv1.CreateSandboxRequest) (*openshellv1.Sandbox, error) {
	response, err := g.client.CreateSandbox(ctx, request)
	if err != nil {
		return nil, err
	}

	return response.GetSandbox(), nil
}

func (g *GRPC) UpdateSandbox(ctx context.Context, request *openshellv1.UpdateSandboxRequest) (*openshellv1.Sandbox, error) {
	response, err := openshellv1.NewSandboxUpdatesClient(g.conn).UpdateSandbox(ctx, request)
	if err != nil {
		return nil, err
	}
	return response.GetSandbox(), nil
}

func (g *GRPC) GetSandbox(ctx context.Context, workspace, name string) (*openshellv1.Sandbox, error) {
	response, err := g.client.GetSandbox(ctx, &openshellv1.GetSandboxRequest{Name: name, WorkspaceScope: workspaceScope(workspace)})
	if err != nil {
		return nil, err
	}
	return response.GetSandbox(), nil
}

func (g *GRPC) ListSandboxes(ctx context.Context, workspace string) ([]*openshellv1.Sandbox, error) {
	scope := listScope(workspace)
	var all []*openshellv1.Sandbox
	token := ""
	for {
		response, err := g.client.ListSandboxes(ctx, &openshellv1.ListSandboxesRequest{WorkspaceScope: scope, PageSize: 1000, PageToken: token})
		if err != nil {
			return nil, err
		}
		all = append(all, response.GetSandboxes()...)
		token = response.GetNextPageToken()
		if token == "" {
			return all, nil
		}
	}
}

func (g *GRPC) DeleteSandbox(ctx context.Context, workspace, name, requestID string) error {
	_, err := g.client.DeleteSandbox(ctx, &openshellv1.DeleteSandboxRequest{Name: name, WorkspaceScope: workspaceScope(workspace), RequestId: requestID})
	return err
}

func (g *GRPC) GetTemplate(ctx context.Context, workspace, name string) (*openshellv1.SandboxWorkloadTemplate, error) {
	response, err := g.client.GetSandboxTemplate(ctx, &openshellv1.GetSandboxTemplateRequest{Name: name, WorkspaceScope: workspaceScope(workspace)})
	if err != nil {
		return nil, err
	}
	return response.GetTemplate(), nil
}

func (g *GRPC) ListTemplates(ctx context.Context, workspace string) ([]*openshellv1.SandboxWorkloadTemplate, error) {
	var all []*openshellv1.SandboxWorkloadTemplate
	token := ""
	for {
		response, err := g.client.ListSandboxTemplates(ctx, &openshellv1.ListSandboxTemplatesRequest{WorkspaceScope: listScope(workspace), PageSize: 1000, PageToken: token})
		if err != nil {
			return nil, err
		}
		all = append(all, response.GetTemplates()...)
		token = response.GetNextPageToken()
		if token == "" {
			return all, nil
		}
	}
}

func (g *GRPC) GetProvider(ctx context.Context, workspace, name string) (*datamodelv1.Provider, error) {
	response, err := g.client.GetProvider(ctx, &openshellv1.GetProviderRequest{Name: name, WorkspaceScope: workspaceScope(workspace)})
	if err != nil {
		return nil, err
	}
	return response.GetProvider(), nil
}

func (g *GRPC) ListProviders(ctx context.Context, workspace string) ([]*datamodelv1.Provider, error) {
	var all []*datamodelv1.Provider
	token := ""
	for {
		response, err := g.client.ListProviders(ctx, &openshellv1.ListProvidersRequest{WorkspaceScope: listScope(workspace), PageSize: 1000, PageToken: token})
		if err != nil {
			return nil, err
		}
		all = append(all, response.GetProviders()...)
		token = response.GetNextPageToken()
		if token == "" {
			return all, nil
		}
	}
}

func workspaceScope(workspace string) *datamodelv1.WorkspaceSelector {
	return &datamodelv1.WorkspaceSelector{Selection: &datamodelv1.WorkspaceSelector_Workspace{Workspace: workspace}}
}

func listScope(workspace string) *datamodelv1.WorkspaceSelector {
	if workspace == "" {
		return &datamodelv1.WorkspaceSelector{Selection: &datamodelv1.WorkspaceSelector_AllWorkspaces{AllWorkspaces: &datamodelv1.AllWorkspaces{}}}
	}
	return workspaceScope(workspace)
}

func bearerTokenInterceptor(path string) grpc.UnaryClientInterceptor {
	return func(ctx context.Context, method string, request, response any, conn *grpc.ClientConn, invoke grpc.UnaryInvoker, options ...grpc.CallOption) error {
		contents, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("read gateway bearer token file: %w", err)
		}
		token := strings.TrimSpace(string(contents))
		if token == "" {
			return fmt.Errorf("gateway bearer token file is empty")
		}
		ctx = metadata.AppendToOutgoingContext(ctx, "authorization", "Bearer "+token)
		return invoke(ctx, method, request, response, conn, options...)
	}
}
