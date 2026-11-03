// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package sandbox

import (
	"context"
	"fmt"
	"strconv"
	"strings"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/convert"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/pagination"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apiequality "k8s.io/apimachinery/pkg/api/equality"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metavalidation "k8s.io/apimachinery/pkg/api/validation"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/util/uuid"
	"k8s.io/apimachinery/pkg/util/validation/field"
	"k8s.io/apiserver/pkg/endpoints/request"
	"k8s.io/apiserver/pkg/registry/rest"
	"sigs.k8s.io/structured-merge-diff/v4/fieldpath"
)

type Storage struct{ gateway gateway.Interface }

func New(gateway gateway.Interface) *Storage { return &Storage{gateway: gateway} }
func (*Storage) New() runtime.Object         { return &api.OpenShellSandbox{} }
func (*Storage) Destroy()                    {}
func (*Storage) NamespaceScoped() bool       { return true }
func (*Storage) NewList() runtime.Object     { return &api.OpenShellSandboxList{} }
func (*Storage) GetSingularName() string     { return "openshellsandbox" }
func (*Storage) ShortNames() []string        { return []string{"oss"} }
func (s *Storage) Get(ctx context.Context, name string, options *metav1.GetOptions) (runtime.Object, error) {
	if options != nil && options.ResourceVersion != "" {
		return nil, apierrors.NewBadRequest("historical resourceVersion reads are not supported")
	}
	namespace := request.NamespaceValue(ctx)
	item, err := s.gateway.GetSandbox(ctx, namespace, name)
	if err != nil {
		return nil, convert.MapError(err, convert.SandboxResource, name, namespace)
	}
	if item == nil {
		return nil, apierrors.NewNotFound(convert.SandboxResource, name)
	}
	return convert.ProjectSandbox(item, namespace)
}
func (s *Storage) List(ctx context.Context, options *metainternalversion.ListOptions) (runtime.Object, error) {
	name, err := selectedName(options)
	if err != nil {
		return nil, err
	}
	namespace := request.NamespaceValue(ctx)
	currentNamedRead := name != "" && options != nil && options.ResourceVersion == "0"
	if name != "" && options != nil && strings.HasPrefix(options.ResourceVersion, "deletion-") {
		_, err := parseDeletionCursor(options.ResourceVersion, namespace, name)
		if err != nil {
			return nil, err
		}
		currentNamedRead = true
	}
	if options != nil && ((options.ResourceVersion != "" && !currentNamedRead) || options.ResourceVersionMatch != "") {
		return nil, apierrors.NewBadRequest("historical resourceVersion list reads are not supported")
	}
	if name != "" {
		if options.Continue != "" {
			return nil, apierrors.NewBadRequest("named deletion tracking does not support pagination cursors")
		}
		return s.listNamed(ctx, namespace, name, options)
	}
	items, err := s.gateway.ListSandboxes(ctx, namespace)
	if err != nil {
		if namespace != "" && apierrors.IsNotFound(convert.MapError(err, convert.SandboxResource, "", namespace)) {
			items = nil
		} else {
			return nil, convert.MapError(err, convert.SandboxResource, "", namespace)
		}
	}
	convertedItems := make([]api.OpenShellSandbox, 0, len(items))
	selector, err := labelSelector(options)
	if err != nil {
		return nil, err
	}
	for _, item := range items {
		itemNamespace := namespace
		if itemNamespace == "" {
			itemNamespace = item.GetMetadata().GetWorkspace()
			if itemNamespace == "" {
				return nil, apierrors.NewServiceUnavailable("gateway returned a sandbox without a workspace")
			}
		}
		converted, err := convert.ProjectSandbox(item, itemNamespace)
		if err != nil {
			return nil, err
		}
		if selector.Matches(labels.Set(converted.Labels)) {
			convertedItems = append(convertedItems, *converted)
		}
	}
	page, listMeta, err := pagination.Page(convertedItems, namespace, selector.String(), options, convert.OpaqueListResourceVersion())
	if err != nil {
		return nil, err
	}
	return &api.OpenShellSandboxList{TypeMeta: metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellSandboxList"}, ListMeta: listMeta, Items: page}, nil
}
func (s *Storage) Create(ctx context.Context, obj runtime.Object, validate rest.ValidateObjectFunc, options *metav1.CreateOptions) (runtime.Object, error) {
	if options != nil && len(options.DryRun) > 0 {
		return nil, apierrors.NewBadRequest("server-side dry-run is not supported")
	}
	input, ok := obj.(*api.OpenShellSandbox)
	if !ok {
		return nil, apierrors.NewBadRequest("expected an OpenShellSandbox")
	}
	namespace := request.NamespaceValue(ctx)
	if input.Namespace != "" && input.Namespace != namespace {
		return nil, apierrors.NewBadRequest("metadata.namespace must match request namespace")
	}
	input.Namespace = namespace
	input.Status = api.OpenShellSandboxStatus{}
	input.Generation = 1
	if err := convert.EnsureLastApplied(input); err != nil {
		return nil, err
	}
	if errs := metavalidation.ValidateObjectMeta(&input.ObjectMeta, true, metavalidation.NameIsDNSSubdomain, field.NewPath("metadata")); len(errs) != 0 {
		return nil, apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, input.Name, errs)
	}
	if len(input.Finalizers) > 0 || len(input.OwnerReferences) > 0 {
		return nil, apierrors.NewBadRequest("finalizers and ownerReferences are not supported")
	}
	if input.Name == "" {
		return nil, apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, "", field.ErrorList{field.Required(field.NewPath("metadata", "name"), "name is required")})
	}
	if validate != nil {
		if err := validate(ctx, obj); err != nil {
			return nil, err
		}
	}
	request, err := convert.ToCreateRequest(input)
	if err != nil {
		return nil, err
	}
	request.Annotations, err = convert.ProjectionAnnotations(input)
	if err != nil {
		return nil, err
	}
	created, err := s.gateway.CreateSandbox(ctx, request)
	if err != nil {
		return nil, convert.MapError(err, convert.SandboxResource, input.Name, namespace)
	}
	if created == nil {
		return nil, apierrors.NewServiceUnavailable("gateway returned an empty sandbox")
	}
	return convert.ProjectSandbox(created, namespace)
}

func (s *Storage) Update(ctx context.Context, name string, info rest.UpdatedObjectInfo, createValidation rest.ValidateObjectFunc, updateValidation rest.ValidateObjectUpdateFunc, forceAllowCreate bool, options *metav1.UpdateOptions) (runtime.Object, bool, error) {
	if options != nil && len(options.DryRun) > 0 {
		return nil, false, apierrors.NewBadRequest("server-side dry-run is not supported")
	}
	namespace := request.NamespaceValue(ctx)
	current, err := s.Get(ctx, name, nil)
	if apierrors.IsNotFound(err) && forceAllowCreate {
		obj, updateErr := info.UpdatedObject(ctx, &api.OpenShellSandbox{})
		if updateErr != nil {
			return nil, false, updateErr
		}
		createOptions := &metav1.CreateOptions{}
		if options != nil {
			createOptions.FieldManager = options.FieldManager
			createOptions.FieldValidation = options.FieldValidation
		}
		created, createErr := s.Create(ctx, obj, createValidation, createOptions)
		return created, createErr == nil, createErr
	}
	if err != nil {
		return nil, false, err
	}
	old := current.(*api.OpenShellSandbox)
	obj, err := info.UpdatedObject(ctx, old.DeepCopyObject())
	if err != nil {
		return nil, false, err
	}
	next, ok := obj.(*api.OpenShellSandbox)
	if !ok {
		return nil, false, apierrors.NewBadRequest("expected an OpenShellSandbox")
	}
	if next.ResourceVersion != old.ResourceVersion {
		return nil, false, apierrors.NewConflict(convert.SandboxResource, name, fmt.Errorf("resourceVersion is stale"))
	}
	if next.UID != old.UID {
		return nil, false, apierrors.NewConflict(convert.SandboxResource, name, fmt.Errorf("UID does not match"))
	}
	if errs := metavalidation.ValidateObjectMetaUpdate(&next.ObjectMeta, &old.ObjectMeta, field.NewPath("metadata")); len(errs) != 0 {
		return nil, false, apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, name, errs)
	}
	if next.Name != name || next.Namespace != namespace || len(next.Finalizers) > 0 || len(next.OwnerReferences) > 0 {
		return nil, false, apierrors.NewBadRequest("name and namespace are immutable; finalizers and ownerReferences are unsupported")
	}
	if !apiequality.Semantic.DeepEqual(next.Spec, old.Spec) {
		return nil, false, apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, name,
			field.ErrorList{field.Forbidden(field.NewPath("spec"), "spec changes are not supported by this prototype; delete and recreate")})
	}
	next.Status = old.Status
	next.Generation = old.Generation
	metadata := next.ObjectMeta.DeepCopy()
	metadata.Labels, metadata.Annotations, metadata.ManagedFields = old.Labels, old.Annotations, old.ManagedFields
	if !apiequality.Semantic.DeepEqual(*metadata, old.ObjectMeta) {
		return nil, false, apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, name,
			field.ErrorList{field.Forbidden(field.NewPath("metadata"), "only labels, annotations, and managedFields can change")})
	}
	if updateValidation != nil {
		if err := updateValidation(ctx, next, old); err != nil {
			return nil, false, err
		}
	}
	if apiequality.Semantic.DeepEqual(next.ObjectMeta, old.ObjectMeta) {
		return old, false, nil
	}
	annotations, err := convert.ProjectionAnnotations(next)
	if err != nil {
		return nil, false, err
	}
	version, err := strconv.ParseUint(old.ResourceVersion, 10, 64)
	if err != nil {
		return nil, false, apierrors.NewInternalError(err)
	}
	updated, err := s.gateway.UpdateSandbox(ctx, &openshellv1.UpdateSandboxRequest{
		WorkspaceScope: &datamodelv1.WorkspaceSelector{Selection: &datamodelv1.WorkspaceSelector_Workspace{Workspace: namespace}},
		Name:           name, ExpectedResourceVersion: version, RequestId: string(uuid.NewUUID()),
		Labels: &openshellv1.SandboxMapUpdate{Values: next.Labels}, Annotations: &openshellv1.SandboxMapUpdate{Values: annotations},
	})
	if err != nil {
		if status.Code(err) == codes.Unimplemented {
			return nil, false, apierrors.NewBadRequest("gateway does not support SandboxUpdates.UpdateSandbox; metadata-changing apply requires a compatible gateway with sandbox updates enabled")
		}
		return nil, false, convert.MapError(err, convert.SandboxResource, name, namespace)
	}
	result, err := convert.ProjectSandbox(updated, namespace)
	return result, false, err
}

func (*Storage) GetResetFields() map[fieldpath.APIVersion]*fieldpath.Set {
	return map[fieldpath.APIVersion]*fieldpath.Set{
		fieldpath.APIVersion(api.SchemeGroupVersion.String()): fieldpath.NewSet(fieldpath.MakePathOrDie("status")),
	}
}
func (s *Storage) Delete(ctx context.Context, name string, validate rest.ValidateObjectFunc, options *metav1.DeleteOptions) (runtime.Object, bool, error) {
	if options != nil {
		if len(options.DryRun) > 0 {
			return nil, false, apierrors.NewBadRequest("server-side dry-run is not supported")
		}
		if options.Preconditions != nil {
			return nil, false, apierrors.NewBadRequest("delete preconditions are not supported by the gateway")
		}
		if options.GracePeriodSeconds != nil {
			return nil, false, apierrors.NewBadRequest("graceful deletion is not supported by the gateway")
		}
		if options.PropagationPolicy != nil && *options.PropagationPolicy != metav1.DeletePropagationBackground {
			return nil, false, apierrors.NewBadRequest("only background deletion propagation is supported")
		}
		if options.OrphanDependents != nil && *options.OrphanDependents {
			return nil, false, apierrors.NewBadRequest("orphan-dependent deletion is not supported")
		}
	}
	namespace := request.NamespaceValue(ctx)
	item, err := s.gateway.GetSandbox(ctx, namespace, name)
	if err != nil {
		return nil, false, convert.MapError(err, convert.SandboxResource, name, namespace)
	}
	if validate != nil {
		if err := validate(ctx, convert.SandboxToAPI(item, namespace)); err != nil {
			return nil, false, err
		}
	}
	deleted, err := s.beginDelete(ctx, namespace, name, item)
	if err != nil {
		return nil, false, err
	}
	return deleted, deleted.DeletionTimestamp == nil, nil
}
func (*Storage) ConvertToTable(ctx context.Context, obj runtime.Object, options runtime.Object) (*metav1.Table, error) {
	return rest.NewDefaultTableConvertor(convert.SandboxResource).ConvertToTable(ctx, obj, options)
}

func labelSelector(options *metainternalversion.ListOptions) (labels.Selector, error) {
	if options == nil || options.LabelSelector == nil {
		return labels.Everything(), nil
	}
	return options.LabelSelector, nil
}

var _ rest.Storage = (*Storage)(nil)
var _ rest.Scoper = (*Storage)(nil)
var _ rest.Getter = (*Storage)(nil)
var _ rest.Lister = (*Storage)(nil)
var _ rest.Creater = (*Storage)(nil)
var _ rest.Patcher = (*Storage)(nil)
var _ rest.ResetFieldsStrategy = (*Storage)(nil)
var _ rest.GracefulDeleter = (*Storage)(nil)
var _ rest.TableConvertor = (*Storage)(nil)
