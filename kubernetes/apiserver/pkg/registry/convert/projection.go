// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package convert

import (
	"encoding/json"
	"fmt"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

const ProjectionAnnotation = "kubernetes.openshell.ai/prototype-projection"

func ProjectSandbox(in *openshellv1.Sandbox, namespace string) (*api.OpenShellSandbox, error) {
	obj := SandboxToAPI(in, namespace)
	if obj == nil {
		return nil, apierrors.NewServiceUnavailable("gateway returned an empty sandbox")
	}
	if err := RestoreProjection(obj); err != nil {
		return nil, apierrors.NewInternalError(err)
	}
	return obj, nil
}

type projection struct {
	Spec          api.OpenShellSandboxSpec    `json:"spec"`
	ManagedFields []metav1.ManagedFieldsEntry `json:"managedFields,omitempty"`
	Generation    int64                       `json:"generation"`
}

// ProjectionAnnotations keeps prototype apply state in the existing gateway
// record. The production API requires a dedicated, transactionally updated record.
func ProjectionAnnotations(obj *api.OpenShellSandbox) (map[string]string, error) {
	if _, exists := obj.Annotations[ProjectionAnnotation]; exists {
		return nil, invalid(obj, "metadata.annotations", "the prototype projection annotation is reserved")
	}
	encoded, err := json.Marshal(projection{Spec: obj.Spec, ManagedFields: obj.ManagedFields, Generation: obj.Generation})
	if err != nil {
		return nil, err
	}
	if len(encoded) > 8192 {
		return nil, invalid(obj, "metadata.managedFields", "prototype apply state exceeds the gateway's 8 KiB annotation limit")
	}
	annotations := cloneMap(obj.Annotations)
	if annotations == nil {
		annotations = map[string]string{}
	}
	annotations[ProjectionAnnotation] = string(encoded)
	return annotations, nil
}

func RestoreProjection(obj *api.OpenShellSandbox) error {
	value, exists := obj.Annotations[ProjectionAnnotation]
	if !exists {
		return nil
	}
	var stored projection
	if err := json.Unmarshal([]byte(value), &stored); err != nil {
		return fmt.Errorf("decode prototype projection: %w", err)
	}
	obj.Spec = stored.Spec
	obj.ManagedFields = stored.ManagedFields
	obj.Generation = stored.Generation
	delete(obj.Annotations, ProjectionAnnotation)
	return nil
}
