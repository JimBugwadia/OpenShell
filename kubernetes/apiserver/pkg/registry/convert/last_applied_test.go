// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package convert

import (
	"encoding/json"
	"strings"
	"testing"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

func TestLastAppliedAuthoredSnapshot(t *testing.T) {
	obj := &api.OpenShellSandbox{
		ObjectMeta: metav1.ObjectMeta{Name: "worker", Namespace: "team-a", UID: "server-id",
			ResourceVersion: "7", Generation: 1, CreationTimestamp: metav1.Now(),
			Annotations: map[string]string{"example.com/note": "kept"}},
		Spec:   api.OpenShellSandboxSpec{WorkloadTemplate: "base"},
		Status: api.OpenShellSandboxStatus{Phase: "Ready"},
	}
	if err := EnsureLastApplied(obj); err != nil {
		t.Fatal(err)
	}
	value := obj.Annotations[LastAppliedAnnotation]
	var document map[string]any
	if err := json.Unmarshal([]byte(value), &document); err != nil {
		t.Fatal(err)
	}
	metadata := document["metadata"].(map[string]any)
	for _, name := range []string{"uid", "resourceVersion", "generation", "creationTimestamp", "managedFields"} {
		if _, exists := metadata[name]; exists {
			t.Errorf("server-owned %s persisted in authored configuration", name)
		}
	}
	if _, exists := document["status"]; exists {
		t.Fatal("status included in last-applied configuration")
	}
	annotations := metadata["annotations"].(map[string]any)
	if annotations["example.com/note"] != "kept" || annotations[LastAppliedAnnotation] != nil {
		t.Fatalf("incorrect annotation snapshot: %v", annotations)
	}
	obj.Spec.WorkloadTemplate = "changed"
	if err := EnsureLastApplied(obj); err != nil || obj.Annotations[LastAppliedAnnotation] != value {
		t.Fatalf("existing last-applied configuration overwritten: %v", err)
	}
}

func TestLastAppliedRejectsOversizeBeforeMutation(t *testing.T) {
	obj := &api.OpenShellSandbox{Spec: api.OpenShellSandboxSpec{Environment: map[string]string{"BIG": strings.Repeat("x", 8192)}}}
	if err := EnsureLastApplied(obj); !apierrors.IsInvalid(err) || obj.Annotations != nil {
		t.Fatalf("oversized snapshot accepted or mutated object: %v", err)
	}
}
