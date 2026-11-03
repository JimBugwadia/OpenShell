// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package workspace

import (
	"context"
	"sort"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/convert"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/pagination"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apiserver/pkg/registry/rest"
)

type Storage struct{ gateway gateway.Interface }

func New(gateway gateway.Interface) *Storage { return &Storage{gateway: gateway} }
func (*Storage) New() runtime.Object         { return &api.OpenShellWorkspace{} }
func (*Storage) Destroy()                    {}
func (*Storage) NamespaceScoped() bool       { return false }
func (*Storage) NewList() runtime.Object     { return &api.OpenShellWorkspaceList{} }
func (*Storage) GetSingularName() string     { return "openshellworkspace" }
func (*Storage) ShortNames() []string        { return []string{"osw"} }

func (s *Storage) Get(ctx context.Context, name string, options *metav1.GetOptions) (runtime.Object, error) {
	if options != nil && options.ResourceVersion != "" {
		return nil, apierrors.NewBadRequest("historical resourceVersion reads are not supported")
	}
	item, err := s.gateway.GetWorkspace(ctx, name)
	if err != nil {
		return nil, convert.MapError(err, convert.WorkspaceResource, name, name)
	}
	if item == nil {
		return nil, apierrors.NewNotFound(convert.WorkspaceResource, name)
	}
	return convert.WorkspaceToAPI(item), nil
}

func (s *Storage) List(ctx context.Context, options *metainternalversion.ListOptions) (runtime.Object, error) {
	if options != nil && ((options.ResourceVersion != "" && options.ResourceVersion != "0") || options.ResourceVersionMatch != "") {
		return nil, apierrors.NewBadRequest("historical resourceVersion list reads are not supported")
	}
	name, err := selectedName(options)
	if err != nil {
		return nil, err
	}
	items, err := s.gateway.ListWorkspaces(ctx)
	if err != nil {
		return nil, convert.MapError(err, convert.WorkspaceResource, "", "")
	}
	selector := labels.Everything()
	if options != nil && options.LabelSelector != nil {
		selector = options.LabelSelector
	}
	convertedItems := make([]api.OpenShellWorkspace, 0, len(items))
	for _, item := range items {
		converted := convert.WorkspaceToAPI(item)
		if converted == nil || converted.Name == "" {
			return nil, apierrors.NewServiceUnavailable("gateway returned an invalid workspace")
		}
		if (name == "" || converted.Name == name) && selector.Matches(labels.Set(converted.Labels)) {
			convertedItems = append(convertedItems, *converted)
		}
	}
	sort.Slice(convertedItems, func(i, j int) bool { return convertedItems[i].Name < convertedItems[j].Name })
	page, meta, err := pagination.Page(convertedItems, "", selector.String()+";name="+name, options, "0")
	if err != nil {
		return nil, err
	}
	return &api.OpenShellWorkspaceList{
		TypeMeta: metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellWorkspaceList"},
		ListMeta: meta, Items: page,
	}, nil
}

func (*Storage) ConvertToTable(ctx context.Context, obj runtime.Object, options runtime.Object) (*metav1.Table, error) {
	return rest.NewDefaultTableConvertor(convert.WorkspaceResource).ConvertToTable(ctx, obj, options)
}

var _ rest.Storage = (*Storage)(nil)
var _ rest.Scoper = (*Storage)(nil)
var _ rest.Getter = (*Storage)(nil)
var _ rest.Lister = (*Storage)(nil)
var _ rest.ShortNamesProvider = (*Storage)(nil)
var _ rest.TableConvertor = (*Storage)(nil)
