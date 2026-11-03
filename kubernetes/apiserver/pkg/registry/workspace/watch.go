// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package workspace

import (
	"context"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	"k8s.io/apimachinery/pkg/api/equality"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/watch"
	"k8s.io/apiserver/pkg/registry/rest"
)

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

// Watch replays the current selection, then diffs fresh gateway lists. It cannot
// resume object versions or observe changes that occur entirely between polls.
func (s *Storage) Watch(ctx context.Context, options *metainternalversion.ListOptions) (watch.Interface, error) {
	if options == nil {
		options = &metainternalversion.ListOptions{}
	}
	if options.ResourceVersion != "" && options.ResourceVersion != "0" {
		return nil, apierrors.NewResourceExpired("workspace watch history is unavailable; relist and watch from resourceVersion 0")
	}
	if options.Limit != 0 || options.Continue != "" || options.ResourceVersionMatch != "" || options.SendInitialEvents != nil {
		return nil, apierrors.NewBadRequest("workspace polling watch does not support pagination, resourceVersionMatch, or sendInitialEvents")
	}
	listOptions := &metainternalversion.ListOptions{LabelSelector: options.LabelSelector, FieldSelector: options.FieldSelector}
	initial, err := s.List(ctx, listOptions)
	if err != nil {
		return nil, err
	}
	pollCtx, cancel := context.WithCancel(ctx)
	events := make(chan watch.Event)
	w := watch.NewProxyWatcher(events)
	go func() {
		select {
		case <-w.StopChan():
			cancel()
		case <-pollCtx.Done():
		}
	}()
	go func() {
		defer close(events)
		defer cancel()
		send := func(kind watch.EventType, item *api.OpenShellWorkspace) bool {
			select {
			case events <- watch.Event{Type: kind, Object: item}:
				return true
			case <-pollCtx.Done():
				return false
			}
		}
		previous := map[string]*api.OpenShellWorkspace{}
		for _, item := range initial.(*api.OpenShellWorkspaceList).Items {
			item := item
			if !send(watch.Added, &item) {
				return
			}
			previous[item.Name] = &item
		}
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-pollCtx.Done():
				return
			case <-ticker.C:
			}
			obj, err := s.List(pollCtx, listOptions)
			if err != nil {
				var status metav1.Status
				if apiStatus, ok := err.(apierrors.APIStatus); ok {
					status = apiStatus.Status()
				} else {
					status = apierrors.NewServiceUnavailable(err.Error()).Status()
				}
				select {
				case events <- watch.Event{Type: watch.Error, Object: &status}:
				case <-pollCtx.Done():
				}
				return
			}
			current := map[string]*api.OpenShellWorkspace{}
			for _, item := range obj.(*api.OpenShellWorkspaceList).Items {
				item := item
				current[item.Name] = &item
				old := previous[item.Name]
				if old != nil && old.UID != item.UID {
					if !send(watch.Deleted, old) {
						return
					}
					old = nil
				}
				if old == nil {
					if !send(watch.Added, &item) {
						return
					}
				} else if !equality.Semantic.DeepEqual(old, &item) {
					if !send(watch.Modified, &item) {
						return
					}
				}
			}
			for name, old := range previous {
				if current[name] == nil && !send(watch.Deleted, old) {
					return
				}
			}
			previous = current
		}
	}()
	return w, nil
}

var _ rest.Watcher = (*Storage)(nil)
