// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package convert

import (
	"encoding/json"
	"strings"
	"testing"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/timestamppb"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

func TestToCreateRequest(t *testing.T) {
	obj := &api.OpenShellSandbox{
		ObjectMeta: metav1.ObjectMeta{Name: "worker", Namespace: "research", Labels: map[string]string{"team": "ml"}, Annotations: map[string]string{"note": "value"}},
		Spec:       api.OpenShellSandboxSpec{Template: &api.SandboxTemplate{Image: "example/image:tag", Resources: map[string]any{"cpu": "2"}}, Command: []string{"agent", "--run"}, TTY: true, RestartPolicy: "OnFailure", Environment: map[string]string{"MODE": "test"}, LogLevel: "debug", Providers: []string{"anthropic"}, Policy: json.RawMessage(`{"version":1}`), Resources: map[string]any{"gpu": map[string]any{"count": float64(2)}}, DesiredState: "Running"},
	}
	request, err := ToCreateRequest(obj)
	if err != nil {
		t.Fatalf("ToCreateRequest() error = %v", err)
	}
	if request.GetWorkspaceScope().GetWorkspace() != "research" {
		t.Fatalf("workspace = %q", request.GetWorkspaceScope().GetWorkspace())
	}
	if request.GetName() != "worker" || request.GetRequestId() == "" {
		t.Fatalf("name/request id not set: %#v", request)
	}
	if request.GetSpec().GetRestartPolicy() != openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_ON_FAILURE {
		t.Fatalf("restart policy = %v", request.GetSpec().GetRestartPolicy())
	}
	if request.GetSpec().GetTemplate().GetImage() != "example/image:tag" || request.GetSpec().GetTemplate().GetResources().AsMap()["cpu"] != "2" {
		t.Fatalf("template conversion = %#v", request.GetSpec().GetTemplate())
	}
	if request.GetSpec().GetResourceRequirements().GetGpu().GetCount() != 2 {
		t.Fatalf("resource conversion = %#v", request.GetSpec().GetResourceRequirements())
	}
	if request.GetSpec().GetEnvironment()["MODE"] != "test" || !request.GetSpec().GetTty() {
		t.Fatalf("spec conversion = %#v", request.GetSpec())
	}
	if request.GetLabels()["team"] != "ml" || request.GetAnnotations()["note"] != "value" {
		t.Fatalf("metadata was not copied")
	}
}

func TestToCreateRequestValidation(t *testing.T) {
	tests := []struct {
		name string
		spec api.OpenShellSandboxSpec
	}{
		{name: "template exclusivity", spec: api.OpenShellSandboxSpec{WorkloadTemplate: "base", Environment: map[string]string{}}},
		{name: "unsupported desired state", spec: api.OpenShellSandboxSpec{DesiredState: "Stopped"}},
		{name: "unsupported resource field", spec: api.OpenShellSandboxSpec{Resources: map[string]any{"cpu": "2"}}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := ToCreateRequest(&api.OpenShellSandbox{ObjectMeta: metav1.ObjectMeta{Name: "worker", Namespace: "research"}, Spec: test.spec})
			if !apierrors.IsInvalid(err) {
				t.Fatalf("expected Invalid, got %v", err)
			}
		})
	}
}

func TestSandboxToAPIReadinessAndMetadata(t *testing.T) {
	created := timestamppb.New(time.Date(2026, 10, 30, 12, 0, 0, 0, time.UTC))
	obj := SandboxToAPI(&openshellv1.Sandbox{
		Metadata: &datamodelv1.ObjectMeta{Id: "gateway-id", Name: "worker", CreatedTime: created, ResourceVersion: 17, Labels: map[string]string{"owner": "team"}, Annotations: map[string]string{"source": "cli"}},
		Spec:     &openshellv1.SandboxSpec{Command: []string{"agent"}},
		Status:   &openshellv1.SandboxStatus{Phase: openshellv1.SandboxPhase_SANDBOX_PHASE_READY, Conditions: []*openshellv1.SandboxCondition{{Type: "Ready", Status: "True", Reason: "Ready", Message: "started", TransitionTime: created}}},
	}, "team-a")
	if obj.UID != "gateway-id" || obj.ResourceVersion != "17" || obj.CreationTimestamp.Time != created.AsTime() {
		t.Fatalf("metadata conversion = %#v", obj.ObjectMeta)
	}
	if obj.Namespace != "team-a" || obj.Status.Phase != "Ready" || obj.Spec.DesiredState != "Running" {
		t.Fatalf("sandbox conversion = %#v", obj)
	}
	if len(obj.Status.Conditions) != 1 || obj.Status.Conditions[0].Type != "Ready" || obj.Status.Conditions[0].Status != metav1.ConditionTrue {
		t.Fatalf("conditions = %#v", obj.Status.Conditions)
	}
}

func TestProviderToAPIStripsCredentials(t *testing.T) {
	obj := ProviderToAPI(&datamodelv1.Provider{Metadata: &datamodelv1.ObjectMeta{Id: "provider-id", Name: "anthropic", ResourceVersion: 2}, Type: "anthropic", Credentials: map[string]string{"API_KEY": "secret"}, CredentialHandles: map[string]*datamodelv1.CredentialHandle{"API_KEY": {}}, Config: map[string]string{"region": "west"}}, "team-a")
	if obj.Spec.Config["region"] != "west" {
		t.Fatalf("config missing: %#v", obj.Spec)
	}
	encoded, err := json.Marshal(obj)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "secret") || strings.Contains(string(encoded), "credential") || strings.Contains(string(encoded), "API_KEY") {
		t.Fatalf("credential data leaked: %s", encoded)
	}
}

func TestMapErrorStatus(t *testing.T) {
	for _, tc := range []struct {
		code       codes.Code
		reason     metav1.StatusReason
		statusCode int32
	}{
		{codes.NotFound, metav1.StatusReasonNotFound, 404},
		{codes.AlreadyExists, metav1.StatusReasonAlreadyExists, 409},
		{codes.Unavailable, metav1.StatusReasonServiceUnavailable, 503},
	} {
		t.Run(tc.code.String(), func(t *testing.T) {
			err := MapError(status.Error(tc.code, "test error"), SandboxResource, "worker", "team-a")
			statusErr, ok := err.(*apierrors.StatusError)
			if !ok || statusErr.ErrStatus.Reason != tc.reason || statusErr.ErrStatus.Code != tc.statusCode {
				t.Fatalf("mapped error = %#v", err)
			}
		})
	}
}
