// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package apiserver

import (
	"fmt"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/provider"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/sandbox"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/template"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/workspace"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/runtime/serializer"
	"k8s.io/apimachinery/pkg/util/sets"
	openapinamer "k8s.io/apiserver/pkg/endpoints/openapi"
	"k8s.io/apiserver/pkg/registry/rest"
	"k8s.io/apiserver/pkg/server"
	"k8s.io/apiserver/pkg/server/options"
	"k8s.io/component-base/compatibility"
)

func New(gatewayClient gateway.Interface, recommended *options.RecommendedOptions) (*server.GenericAPIServer, error) {
	if gatewayClient == nil {
		return nil, fmt.Errorf("gateway client is required")
	}
	if recommended == nil {
		return nil, fmt.Errorf("recommended options are required")
	}
	if recommended.Etcd != nil {
		return nil, fmt.Errorf("etcd must remain disabled for the gateway-backed API")
	}

	scheme := runtime.NewScheme()
	if err := api.AddToScheme(scheme); err != nil {
		return nil, err
	}
	metav1.AddToGroupVersion(scheme, schema.GroupVersion{Version: "v1"})
	codecs := serializer.NewCodecFactory(scheme)
	config := server.NewConfig(codecs)
	config.EffectiveVersion = compatibility.NewEffectiveVersionFromString("", "", "")
	definitionNamer := openapinamer.NewDefinitionNamer(scheme)
	config.OpenAPIConfig = server.DefaultOpenAPIConfig(api.GetOpenAPIDefinitions, definitionNamer)
	config.OpenAPIConfig.Info.Version = api.Version
	config.OpenAPIV3Config = server.DefaultOpenAPIV3Config(api.GetOpenAPIDefinitions, definitionNamer)
	config.OpenAPIV3Config.Info.Version = api.Version
	if err := recommended.SecureServing.ApplyTo(&config.SecureServing, &config.LoopbackClientConfig); err != nil {
		return nil, fmt.Errorf("configure secure serving: %w", err)
	}
	if err := recommended.Authentication.ApplyTo(&config.Authentication, config.SecureServing, config.OpenAPIConfig); err != nil {
		return nil, fmt.Errorf("configure delegated authentication: %w", err)
	}
	if err := recommended.Authorization.ApplyTo(&config.Authorization); err != nil {
		return nil, fmt.Errorf("configure delegated authorization: %w", err)
	}
	config.LegacyAPIGroupPrefixes = sets.NewString("/api")
	config.EnableIndex = true

	genericServer, err := config.Complete(nil).New("openshell-kube-apiserver", server.NewEmptyDelegate())
	if err != nil {
		return nil, fmt.Errorf("create generic API server: %w", err)
	}
	groupInfo := server.NewDefaultAPIGroupInfo(api.GroupName, scheme, runtime.NewParameterCodec(scheme), codecs)
	groupInfo.VersionedResourcesStorageMap[api.Version] = map[string]rest.Storage{
		"openshellsandboxes":        sandbox.New(gatewayClient),
		"openshellsandboxtemplates": template.New(gatewayClient),
		"openshellproviders":        provider.New(gatewayClient),
		"openshellworkspaces":       workspace.New(gatewayClient),
	}
	if err := genericServer.InstallAPIGroup(&groupInfo); err != nil {
		return nil, fmt.Errorf("install OpenShell API group: %w", err)
	}
	return genericServer, nil
}
