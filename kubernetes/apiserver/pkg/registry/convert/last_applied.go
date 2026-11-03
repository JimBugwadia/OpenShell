// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package convert

import (
	"encoding/json"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
)

const LastAppliedAnnotation = "kubectl.kubernetes.io/last-applied-configuration"

// EnsureLastApplied snapshots authored fields only, never status or server-owned
// metadata. Preserve client-supplied configuration.
func EnsureLastApplied(obj *api.OpenShellSandbox) error {
	if _, exists := obj.Annotations[LastAppliedAnnotation]; exists {
		return nil
	}
	annotations := cloneMap(obj.Annotations)
	if annotations == nil {
		annotations = map[string]string{}
	}
	metadata := map[string]any{"name": obj.Name, "namespace": obj.Namespace, "annotations": cloneMap(annotations)}
	if len(obj.Labels) > 0 {
		metadata["labels"] = obj.Labels
	}
	data, err := json.Marshal(map[string]any{
		"apiVersion": api.SchemeGroupVersion.String(), "kind": "OpenShellSandbox",
		"metadata": metadata, "spec": obj.Spec,
	})
	if err != nil {
		return err
	}
	// kubectl encodes unstructured objects with sorted keys at every level.
	// Matching its encoding avoids an annotation-only patch on unchanged apply.
	var document map[string]any
	if err := json.Unmarshal(data, &document); err != nil {
		return err
	}
	data, err = json.Marshal(document)
	if err != nil {
		return err
	}
	if len(data)+1 > 8192 {
		return invalid(obj, "metadata.annotations", "last-applied configuration exceeds the gateway's 8 KiB annotation limit")
	}
	annotations[LastAppliedAnnotation] = string(data) + "\n"
	obj.Annotations = annotations
	return nil
}
