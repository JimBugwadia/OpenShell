// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package pagination

import (
	"encoding/base64"
	"encoding/json"
	"fmt"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

type token struct {
	Namespace string `json:"n"`
	Selector  string `json:"s"`
	Offset    int    `json:"o"`
	Version   string `json:"v"`
}

func Page[T any](items []T, namespace, selector string, options *metainternalversion.ListOptions, resourceVersion string) ([]T, metav1.ListMeta, error) {
	state := token{Namespace: namespace, Selector: selector, Version: resourceVersion}
	if options != nil && options.Continue != "" {
		decoded, err := base64.RawURLEncoding.DecodeString(options.Continue)
		if err != nil {
			return nil, metav1.ListMeta{}, apierrors.NewBadRequest("invalid continue token")
		}
		if err := json.Unmarshal(decoded, &state); err != nil || state.Namespace != namespace || state.Selector != selector || state.Version == "" || state.Offset < 0 {
			return nil, metav1.ListMeta{}, apierrors.NewBadRequest("continue token does not match this namespace and label selector")
		}
	}
	if options != nil && options.Limit < 0 {
		return nil, metav1.ListMeta{}, apierrors.NewBadRequest("list limit must not be negative")
	}
	start := state.Offset
	if start > len(items) {
		start = len(items)
	}
	end := len(items)
	if options != nil && options.Limit > 0 && options.Limit < int64(end-start) {
		end = start + int(options.Limit)
	}
	meta := metav1.ListMeta{ResourceVersion: state.Version}
	if meta.ResourceVersion == "" {
		return nil, metav1.ListMeta{}, fmt.Errorf("list resource version is required")
	}
	if end < len(items) {
		state.Offset = end
		data, err := json.Marshal(state)
		if err != nil {
			return nil, metav1.ListMeta{}, fmt.Errorf("encode list continue token: %w", err)
		}
		meta.Continue = base64.RawURLEncoding.EncodeToString(data)
	}
	return items[start:end], meta, nil
}
