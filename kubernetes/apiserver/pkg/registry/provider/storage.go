// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/convert"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/pagination"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apiserver/pkg/endpoints/request"
	"k8s.io/apiserver/pkg/registry/rest"
)

type Storage struct{ gateway gateway.Interface }

func New(gateway gateway.Interface) *Storage { return &Storage{gateway: gateway} }
func (*Storage) New() runtime.Object         { return &api.OpenShellProvider{} }
func (*Storage) Destroy()                    {}
func (*Storage) NamespaceScoped() bool       { return true }
func (*Storage) NewList() runtime.Object     { return &api.OpenShellProviderList{} }
func (*Storage) GetSingularName() string     { return "openshellprovider" }
func (*Storage) ShortNames() []string        { return []string{"osp"} }
func (s *Storage) Get(ctx context.Context, name string, options *metav1.GetOptions) (runtime.Object, error) {
	if options != nil && options.ResourceVersion != "" {
		return nil, apierrors.NewBadRequest("historical resourceVersion reads are not supported")
	}
	namespace := request.NamespaceValue(ctx)
	item, err := s.gateway.GetProvider(ctx, namespace, name)
	if err != nil {
		return nil, convert.MapError(err, convert.ProviderResource, name, namespace)
	}
	if item == nil {
		return nil, apierrors.NewNotFound(convert.ProviderResource, name)
	}
	return convert.ProviderToAPI(item, namespace), nil
}
func (s *Storage) List(ctx context.Context, options *metainternalversion.ListOptions) (runtime.Object, error) {
	if options != nil && (options.ResourceVersion != "" || options.ResourceVersionMatch != "") {
		return nil, apierrors.NewBadRequest("historical resourceVersion list reads are not supported")
	}
	namespace := request.NamespaceValue(ctx)
	items, err := s.gateway.ListProviders(ctx, namespace)
	if err != nil {
		mapped := convert.MapError(err, convert.ProviderResource, "", namespace)
		if namespace != "" && apierrors.IsNotFound(mapped) {
			items = nil
		} else {
			return nil, mapped
		}
	}
	selector := labels.Everything()
	if options != nil && options.LabelSelector != nil {
		selector = options.LabelSelector
	}
	convertedItems := make([]api.OpenShellProvider, 0, len(items))
	for _, item := range items {
		itemNamespace := namespace
		if itemNamespace == "" {
			itemNamespace = item.GetMetadata().GetWorkspace()
			if itemNamespace == "" {
				return nil, apierrors.NewServiceUnavailable("gateway returned a provider without a workspace")
			}
		}
		converted := convert.ProviderToAPI(item, itemNamespace)
		if selector.Matches(labels.Set(converted.Labels)) {
			convertedItems = append(convertedItems, *converted)
		}
	}
	page, listMeta, err := pagination.Page(convertedItems, namespace, selector.String(), options, convert.OpaqueListResourceVersion())
	if err != nil {
		return nil, err
	}
	return &api.OpenShellProviderList{TypeMeta: metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellProviderList"}, ListMeta: listMeta, Items: page}, nil
}
func (*Storage) ConvertToTable(ctx context.Context, obj runtime.Object, options runtime.Object) (*metav1.Table, error) {
	return rest.NewDefaultTableConvertor(convert.ProviderResource).ConvertToTable(ctx, obj, options)
}

var _ rest.Storage = (*Storage)(nil)
var _ rest.Scoper = (*Storage)(nil)
var _ rest.Getter = (*Storage)(nil)
var _ rest.Lister = (*Storage)(nil)
var _ rest.TableConvertor = (*Storage)(nil)
