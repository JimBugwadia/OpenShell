// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package sandbox

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"log/slog"
	"strings"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/registry/convert"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/uuid"
	"k8s.io/apimachinery/pkg/watch"
	"k8s.io/apiserver/pkg/endpoints/request"
	"k8s.io/apiserver/pkg/registry/rest"
)

// Keep the RPC alive after the HTTP response, but acknowledge only durable
// gateway admission. The gateway owns cleanup/recovery across adapter restarts.
func (s *Storage) beginDelete(ctx context.Context, namespace, name string, original *openshellv1.Sandbox) (*api.OpenShellSandbox, error) {
	rpcCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 10*time.Minute)
	result := make(chan error, 1)
	go func() {
		defer cancel()
		err := s.gateway.DeleteSandbox(rpcCtx, namespace, name, string(uuid.NewUUID()))
		if err != nil {
			slog.Error("gateway sandbox deletion failed", "namespace", namespace, "name", name, "error", err)
		}
		result <- err
	}()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case err := <-result:
			if err != nil {
				return nil, convert.MapError(err, convert.SandboxResource, name, namespace)
			}
			obj, err := convert.ProjectSandbox(original, namespace)
			if err != nil {
				return nil, err
			}
			// A successful RPC can mean asynchronous cleanup was accepted.
			current, err := s.gateway.GetSandbox(ctx, namespace, name)
			if err == nil && current.GetMetadata().GetId() == original.GetMetadata().GetId() {
				if current.GetStatus().GetPhase() != openshellv1.SandboxPhase_SANDBOX_PHASE_DELETING {
					return nil, apierrors.NewServiceUnavailable("gateway acknowledged deletion without recording Deleting")
				}
				return deletingObject(current, namespace)
			}
			if err != nil && !apierrors.IsNotFound(convert.MapError(err, convert.SandboxResource, name, namespace)) {
				return nil, convert.MapError(err, convert.SandboxResource, name, namespace)
			}
			return obj, nil
		case <-ticker.C:
			current, err := s.gateway.GetSandbox(ctx, namespace, name)
			if apierrors.IsNotFound(convert.MapError(err, convert.SandboxResource, name, namespace)) {
				// The original object is already absent. Driver cleanup can
				// still be running, but there is no API object left to wait on.
				return convert.ProjectSandbox(original, namespace)
			}
			if err != nil {
				// Other read failures are not proof of deletion admission.
				continue
			}
			if current.GetMetadata().GetId() != original.GetMetadata().GetId() {
				return convert.ProjectSandbox(original, namespace)
			}
			if current.GetMetadata().GetId() == original.GetMetadata().GetId() && current.GetStatus().GetPhase() == openshellv1.SandboxPhase_SANDBOX_PHASE_DELETING {
				return deletingObject(current, namespace)
			}
		}
	}
}

func deletingObject(item *openshellv1.Sandbox, namespace string) (*api.OpenShellSandbox, error) {
	obj, err := convert.ProjectSandbox(item, namespace)
	if err == nil && obj.DeletionTimestamp == nil {
		now := metav1.Now()
		obj.DeletionTimestamp = &now
	}
	return obj, err
}

type deletionCursor struct {
	Namespace string    `json:"namespace"`
	Name      string    `json:"name"`
	UID       types.UID `json:"uid"`
}

func parseDeletionCursor(version, namespace, name string) (deletionCursor, error) {
	data, err := base64.RawURLEncoding.DecodeString(strings.TrimPrefix(version, "deletion-"))
	var cursor deletionCursor
	if err != nil || json.Unmarshal(data, &cursor) != nil || cursor.Name != name || cursor.Namespace != namespace {
		return cursor, apierrors.NewBadRequest("invalid deletion tracking cursor")
	}
	return cursor, nil
}

func selectedName(options *metainternalversion.ListOptions) (string, error) {
	if options == nil || options.FieldSelector == nil || options.FieldSelector.Empty() {
		return "", nil
	}
	name, ok := options.FieldSelector.RequiresExactMatch("metadata.name")
	if !ok || name == "" || len(options.FieldSelector.Requirements()) != 1 {
		return "", apierrors.NewBadRequest("only an exact metadata.name field selector is supported")
	}
	return name, nil
}

func (s *Storage) listNamed(ctx context.Context, namespace, name string, options *metainternalversion.ListOptions) (runtime.Object, error) {
	if namespace == "" {
		return nil, apierrors.NewBadRequest("metadata.name selection requires a namespace")
	}
	obj, err := s.Get(ctx, name, nil)
	if err != nil && !apierrors.IsNotFound(err) {
		return nil, err
	}
	items := []api.OpenShellSandbox{}
	cursor := deletionCursor{Namespace: namespace, Name: name}
	relisting := strings.HasPrefix(options.ResourceVersion, "deletion-")
	if relisting {
		var cursorErr error
		cursor, cursorErr = parseDeletionCursor(options.ResourceVersion, namespace, name)
		if cursorErr != nil {
			return nil, cursorErr
		}
	}
	if err == nil {
		current := obj.(*api.OpenShellSandbox)
		if !relisting {
			cursor.UID = current.UID
		}
		selector, err := labelSelector(options)
		if err != nil {
			return nil, err
		}
		if current.UID == cursor.UID && selector.Matches(labels.Set(current.Labels)) {
			items = append(items, *current)
		}
	}
	data, _ := json.Marshal(cursor)
	return &api.OpenShellSandboxList{
		TypeMeta: metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellSandboxList"},
		ListMeta: metav1.ListMeta{ResourceVersion: "deletion-" + base64.RawURLEncoding.EncodeToString(data)},
		Items:    items,
	}, nil
}

// Watch supports only level-based deletion tracking for one named sandbox.
// It is not a historical watch or a collection change journal.
func (s *Storage) Watch(ctx context.Context, options *metainternalversion.ListOptions) (watch.Interface, error) {
	name, err := selectedName(options)
	if err != nil {
		return nil, err
	}
	if name == "" || options == nil || !strings.HasPrefix(options.ResourceVersion, "deletion-") {
		return nil, apierrors.NewBadRequest("watch supports only named deletion tracking after a metadata.name list")
	}
	if options.LabelSelector != nil && !options.LabelSelector.Empty() {
		return nil, apierrors.NewBadRequest("deletion tracking does not support label selectors")
	}
	namespace := request.NamespaceValue(ctx)
	cursor, err := parseDeletionCursor(options.ResourceVersion, namespace, name)
	if err != nil || cursor.UID == "" {
		return nil, apierrors.NewBadRequest("invalid deletion tracking cursor")
	}
	events := make(chan watch.Event)
	w := watch.NewProxyWatcher(events)
	go func() {
		defer close(events)
		ticker := time.NewTicker(200 * time.Millisecond)
		defer ticker.Stop()
		for {
			var event *watch.Event
			current, err := s.Get(ctx, name, nil)
			if apierrors.IsNotFound(err) || (err == nil && current.(*api.OpenShellSandbox).UID != cursor.UID) {
				event = &watch.Event{Type: watch.Deleted, Object: &api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: namespace, UID: cursor.UID}}}
			} else if err != nil {
				event = &watch.Event{Type: watch.Error, Object: &metav1.Status{Status: metav1.StatusFailure, Reason: metav1.StatusReasonServiceUnavailable, Code: 503, Message: err.Error()}}
			}
			if event != nil {
				select {
				case events <- *event:
				case <-ctx.Done():
				case <-w.StopChan():
				}
				return
			}
			select {
			case <-ctx.Done():
				return
			case <-w.StopChan():
				return
			case <-ticker.C:
			}
		}
	}()
	return w, nil
}

var _ rest.Watcher = (*Storage)(nil)
