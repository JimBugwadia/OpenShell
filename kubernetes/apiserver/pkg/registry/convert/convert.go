// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package convert

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"time"

	api "github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apis/openshell/v1alpha1"
	datamodelv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/datamodelv1"
	openshellv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/openshellv1"
	sandboxv1 "github.com/NVIDIA/OpenShell/sdk/go/proto/sandboxv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/types/known/structpb"
	"google.golang.org/protobuf/types/known/timestamppb"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/uuid"
	"k8s.io/apimachinery/pkg/util/validation/field"
)

var SandboxResource = schema.GroupResource{Group: api.GroupName, Resource: "openshellsandboxes"}
var TemplateResource = schema.GroupResource{Group: api.GroupName, Resource: "openshellsandboxtemplates"}
var ProviderResource = schema.GroupResource{Group: api.GroupName, Resource: "openshellproviders"}
var WorkspaceResource = schema.GroupResource{Group: api.GroupName, Resource: "openshellworkspaces"}

func WorkspaceToAPI(in *datamodelv1.Workspace) *api.OpenShellWorkspace {
	if in == nil {
		return nil
	}
	phase := "Unknown"
	switch in.GetStatus().GetPhase() {
	case datamodelv1.WorkspacePhase_WORKSPACE_PHASE_ACTIVE:
		phase = "Active"
	case datamodelv1.WorkspacePhase_WORKSPACE_PHASE_TERMINATING:
		phase = "Terminating"
	}
	return &api.OpenShellWorkspace{
		TypeMeta:   metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellWorkspace"},
		ObjectMeta: objectMeta(in.GetMetadata(), ""),
		Status:     api.OpenShellWorkspaceStatus{Phase: phase},
	}
}

func ToCreateRequest(obj *api.OpenShellSandbox) (*openshellv1.CreateSandboxRequest, error) {
	if obj == nil {
		return nil, apierrors.NewBadRequest("sandbox object is required")
	}
	if obj.Spec.DesiredState != "" && obj.Spec.DesiredState != "Running" {
		return nil, invalid(obj, "spec.desiredState", "only Running is accepted when creating a sandbox")
	}
	spec := obj.Spec
	if spec.WorkloadTemplate != "" && (spec.Template != nil || spec.Environment != nil || spec.LogLevel != "" || spec.Resources != nil) {
		return nil, invalid(obj, "spec.workloadTemplate", "cannot be combined with spec.template, spec.environment, spec.logLevel, or spec.resources")
	}

	gatewaySpec := &openshellv1.SandboxSpec{Command: append([]string(nil), spec.Command...), Tty: spec.TTY, Providers: append([]string(nil), spec.Providers...)}
	gatewaySpec.LogLevel = spec.LogLevel
	gatewaySpec.Environment = cloneMap(spec.Environment)
	if policy := strings.TrimSpace(string(spec.Policy)); policy != "" {
		var parsed sandboxv1.SandboxPolicy
		if err := protojson.Unmarshal(spec.Policy, &parsed); err != nil {
			return nil, invalid(obj, "spec.policy", fmt.Sprintf("invalid gateway SandboxPolicy: %v", err))
		}
		gatewaySpec.Policy = &parsed
	}
	switch spec.RestartPolicy {
	case "":
	case "Never":
		gatewaySpec.RestartPolicy = openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_NEVER
	case "OnFailure":
		gatewaySpec.RestartPolicy = openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_ON_FAILURE
	case "Always":
		gatewaySpec.RestartPolicy = openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_ALWAYS
	default:
		return nil, invalid(obj, "spec.restartPolicy", "must be Never, OnFailure, or Always")
	}
	if spec.Resources != nil {
		resourceRequirements, err := toResourceRequirements(spec.Resources)
		if err != nil {
			return nil, invalid(obj, "spec.resources", err.Error())
		}
		gatewaySpec.ResourceRequirements = resourceRequirements
	}
	if spec.Template != nil {
		gatewayTemplate := &openshellv1.SandboxTemplate{}
		if spec.Template != nil {
			gatewayTemplate.Image = spec.Template.Image
			gatewayTemplate.RuntimeClassName = spec.Template.RuntimeClassName
			gatewayTemplate.AgentSocket = spec.Template.AgentSocket
			gatewayTemplate.Labels = cloneMap(spec.Template.Labels)
			gatewayTemplate.Annotations = cloneMap(spec.Template.Annotations)
			gatewayTemplate.Environment = cloneMap(spec.Template.Environment)
			var err error
			gatewayTemplate.Resources, err = toStruct(spec.Template.Resources)
			if err != nil {
				return nil, invalid(obj, "spec.template.resources", err.Error())
			}
			gatewayTemplate.DriverConfig, err = toStruct(spec.Template.DriverConfig)
			if err != nil {
				return nil, invalid(obj, "spec.template.driverConfig", err.Error())
			}
			gatewayTemplate.UserNamespaces = spec.Template.UserNamespaces
		}
		gatewaySpec.Template = gatewayTemplate
	}
	requestID := uuid.NewUUID()
	return &openshellv1.CreateSandboxRequest{
		WorkspaceScope:   workspaceScope(obj.Namespace),
		Spec:             gatewaySpec,
		Name:             obj.Name,
		Labels:           cloneMap(obj.Labels),
		Annotations:      cloneMap(obj.Annotations),
		WorkloadTemplate: spec.WorkloadTemplate,
		RequestId:        string(requestID),
	}, nil
}

func SandboxToAPI(in *openshellv1.Sandbox, namespace string) *api.OpenShellSandbox {
	if in == nil {
		return nil
	}
	metadata := in.GetMetadata()
	obj := &api.OpenShellSandbox{
		TypeMeta:   metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellSandbox"},
		ObjectMeta: objectMeta(metadata, namespace),
	}
	if in.GetSpec() != nil {
		spec := in.GetSpec()
		obj.Spec = api.OpenShellSandboxSpec{
			DesiredState: desiredStateForPhase(in.GetStatus().GetPhase()),
			Command:      append([]string(nil), spec.GetCommand()...),
			TTY:          spec.GetTty(), RestartPolicy: restartPolicyName(spec.GetRestartPolicy()),
			Environment: cloneMap(spec.GetEnvironment()), LogLevel: spec.GetLogLevel(),
			Providers: append([]string(nil), spec.GetProviders()...),
		}
		if in.GetCreatedFromWorkloadTemplate() != nil {
			obj.Spec.WorkloadTemplate = in.GetCreatedFromWorkloadTemplate().GetName()
		}
		if spec.GetTemplate() != nil {
			template := spec.GetTemplate()
			obj.Spec.Template = &api.SandboxTemplate{Image: template.GetImage(), RuntimeClassName: template.GetRuntimeClassName(), AgentSocket: template.GetAgentSocket(), Labels: cloneMap(template.GetLabels()), Annotations: cloneMap(template.GetAnnotations()), Environment: cloneMap(template.GetEnvironment()), Resources: structMap(template.GetResources()), DriverConfig: structMap(template.GetDriverConfig()), UserNamespaces: cloneBool(template.UserNamespaces)}
		}
		if requirements := spec.GetResourceRequirements(); requirements != nil && requirements.GetGpu() != nil {
			gpu := map[string]any{}
			if count := requirements.GetGpu().Count; count != nil {
				gpu["count"] = *count
			}
			obj.Spec.Resources = map[string]any{"gpu": gpu}
		}
		if spec.GetPolicy() != nil {
			obj.Spec.Policy, _ = protojson.Marshal(spec.GetPolicy())
		}
	}
	gatewayStatus := in.GetStatus()
	phase := openshellv1.SandboxPhase_SANDBOX_PHASE_UNSPECIFIED
	if gatewayStatus != nil {
		phase = gatewayStatus.GetPhase()
	}
	obj.Status = api.OpenShellSandboxStatus{Phase: phaseName(phase)}
	if gatewayStatus != nil {
		obj.Status.AgentPod = gatewayStatus.GetAgentPod()
		obj.Status.CurrentPolicyVersion = gatewayStatus.GetCurrentPolicyVersion()
	}
	if in.GetSpec() != nil {
		obj.Spec.DesiredState = desiredStateForPhase(phase)
	}
	readySeen := false
	if gatewayStatus != nil {
		for _, condition := range gatewayStatus.GetConditions() {
			if condition == nil {
				continue
			}
			conditionType := condition.GetType()
			if strings.EqualFold(conditionType, "Ready") {
				conditionType = "Ready"
				readySeen = true
			}
			conditionTime := timestamp(condition.GetTransitionTime())
			if conditionTime.IsZero() && metadata != nil && metadata.GetCreatedTime() != nil {
				conditionTime = metadata.GetCreatedTime().AsTime()
			}
			obj.Status.Conditions = append(obj.Status.Conditions, metav1.Condition{Type: conditionType, Status: conditionStatus(condition.GetStatus()), Reason: defaultReason(condition.GetReason()), Message: condition.GetMessage(), LastTransitionTime: metav1.NewTime(conditionTime)})
		}
	}
	if !readySeen {
		readyStatus, reason, message := readyCondition(phase)
		transitionTime := time.Time{}
		if metadata != nil && metadata.GetCreatedTime() != nil {
			transitionTime = metadata.GetCreatedTime().AsTime()
		}
		obj.Status.Conditions = append(obj.Status.Conditions, metav1.Condition{Type: "Ready", Status: readyStatus, Reason: reason, Message: message, LastTransitionTime: metav1.NewTime(transitionTime)})
	}
	return obj
}

func TemplateToAPI(in *openshellv1.SandboxWorkloadTemplate, namespace string) *api.OpenShellSandboxTemplate {
	if in == nil {
		return nil
	}
	obj := &api.OpenShellSandboxTemplate{TypeMeta: metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellSandboxTemplate"}, ObjectMeta: objectMeta(in.GetMetadata(), namespace)}
	if spec := in.GetSpec(); spec != nil {
		obj.Spec.DriverConfig = structMap(spec.GetDriverConfig())
		if workload := spec.GetWorkload(); workload != nil {
			obj.Spec.Workload = api.SandboxWorkloadConfig{Image: workload.GetImage(), Environment: cloneMap(workload.GetEnvironment())}
			if workload.GetResources() != nil {
				resources := map[string]any{"cpu": workload.GetResources().GetCpu(), "memory": workload.GetResources().GetMemory()}
				if gpu := workload.GetResources().GetGpu(); gpu != nil {
					resources["gpu"] = map[string]any{"count": gpu.GetCount()}
				}
				obj.Spec.Workload.Resources = resources
			}
		}
		if level := spec.GetDesiredServiceLevel(); level != nil {
			obj.Spec.DesiredServiceLevel = map[string]any{}
			if startup := level.GetStartup(); startup != nil {
				startupFields := map[string]any{"maxBurst": startup.GetMaxBurst()}
				if startup.GetReadyWithin() != nil {
					startupFields["readyWithin"] = startup.GetReadyWithin().AsDuration().String()
				}
				obj.Spec.DesiredServiceLevel["startup"] = startupFields
			}
		}
	}
	return obj
}

func ProviderToAPI(in *datamodelv1.Provider, namespace string) *api.OpenShellProvider {
	if in == nil {
		return nil
	}
	return &api.OpenShellProvider{
		TypeMeta:   metav1.TypeMeta{APIVersion: api.SchemeGroupVersion.String(), Kind: "OpenShellProvider"},
		ObjectMeta: objectMeta(in.GetMetadata(), namespace),
		Spec:       api.OpenShellProviderSpec{Type: in.GetType(), Config: cloneMap(in.GetConfig()), ProfileWorkspace: in.GetProfileWorkspace()},
	}
}

func MapError(err error, resource schema.GroupResource, name, namespace string) error {
	if err == nil {
		return nil
	}
	if apierrors.IsNotFound(err) || apierrors.IsAlreadyExists(err) || apierrors.IsServiceUnavailable(err) {
		return err
	}
	grpcStatus, ok := status.FromError(err)
	if !ok {
		return apierrors.NewServiceUnavailable(err.Error())
	}
	switch grpcStatus.Code() {
	case codes.NotFound:
		if resource != WorkspaceResource && strings.Contains(strings.ToLower(grpcStatus.Message()), "workspace") {
			return apierrors.NewNotFound(schema.GroupResource{Group: api.GroupName, Resource: "workspace"}, namespace)
		}
		return apierrors.NewNotFound(resource, name)
	case codes.AlreadyExists:
		return apierrors.NewAlreadyExists(resource, name)
	case codes.Aborted:
		return apierrors.NewConflict(resource, name, fmt.Errorf("%s", grpcStatus.Message()))
	case codes.Unavailable, codes.DeadlineExceeded:
		return apierrors.NewServiceUnavailable("OpenShell gateway is unavailable: " + grpcStatus.Message())
	case codes.InvalidArgument, codes.FailedPrecondition:
		return apierrors.NewBadRequest(grpcStatus.Message())
	case codes.PermissionDenied:
		return apierrors.NewForbidden(resource, name, fmt.Errorf("OpenShell gateway denied the request: %s", grpcStatus.Message()))
	case codes.Unauthenticated:
		return apierrors.NewUnauthorized("OpenShell gateway authentication failed")
	default:
		return apierrors.NewServiceUnavailable("OpenShell gateway request failed: " + grpcStatus.Message())
	}
}

func OpaqueListResourceVersion() string {
	id := uuid.NewUUID()
	return "list-" + string(id)
}

func objectMeta(in *datamodelv1.ObjectMeta, namespace string) metav1.ObjectMeta {
	if in == nil {
		return metav1.ObjectMeta{Namespace: namespace}
	}
	meta := metav1.ObjectMeta{Namespace: namespace, Name: in.GetName(), UID: typesUID(in.GetId()), ResourceVersion: strconv.FormatUint(in.GetResourceVersion(), 10), Labels: cloneMap(in.GetLabels()), Annotations: cloneMap(in.GetAnnotations())}
	if in.GetCreatedTime() != nil {
		// metav1.Time serializes at second precision; normalize before immutable
		// metadata comparisons on the next apply request.
		meta.CreationTimestamp = metav1.NewTime(in.GetCreatedTime().AsTime().Truncate(time.Second))
	}
	if in.GetDeletionTime() != nil {
		t := metav1.NewTime(in.GetDeletionTime().AsTime().Truncate(time.Second))
		meta.DeletionTimestamp = &t
	}
	return meta
}

func workspaceScope(workspace string) *datamodelv1.WorkspaceSelector {
	return &datamodelv1.WorkspaceSelector{Selection: &datamodelv1.WorkspaceSelector_Workspace{Workspace: workspace}}
}
func typesUID(value string) types.UID { return types.UID(value) }
func cloneBool(in *bool) *bool {
	if in == nil {
		return nil
	}
	value := *in
	return &value
}
func cloneMap[K comparable, V any](in map[K]V) map[K]V {
	if in == nil {
		return nil
	}
	out := make(map[K]V, len(in))
	for k, v := range in {
		out[k] = v
	}
	return out
}
func toResourceRequirements(value map[string]any) (*openshellv1.ResourceRequirements, error) {
	for key := range value {
		if key != "gpu" {
			return nil, fmt.Errorf("unsupported resource field %q; gateway SandboxSpec supports gpu only", key)
		}
	}
	gpuValue, ok := value["gpu"]
	if !ok {
		return nil, fmt.Errorf("must contain gpu when resources is specified")
	}
	gpuMap, ok := gpuValue.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("gpu must be an object")
	}
	for key := range gpuMap {
		if key != "count" {
			return nil, fmt.Errorf("unsupported gpu field %q", key)
		}
	}
	requirements := &openshellv1.ResourceRequirements{Gpu: &openshellv1.GpuResourceRequirements{}}
	if rawCount, exists := gpuMap["count"]; exists {
		var count uint64
		switch value := rawCount.(type) {
		case float64:
			if value < 0 || value != float64(uint64(value)) {
				return nil, fmt.Errorf("gpu.count must be a nonnegative integer")
			}
			count = uint64(value)
		case int:
			if value < 0 {
				return nil, fmt.Errorf("gpu.count must be a nonnegative integer")
			}
			count = uint64(value)
		case int64:
			if value < 0 {
				return nil, fmt.Errorf("gpu.count must be a nonnegative integer")
			}
			count = uint64(value)
		case uint32:
			count = uint64(value)
		default:
			return nil, fmt.Errorf("gpu.count must be an integer")
		}
		if count > math.MaxUint32 {
			return nil, fmt.Errorf("gpu.count exceeds uint32 range")
		}
		count32 := uint32(count)
		requirements.Gpu.Count = &count32
	}
	return requirements, nil
}

func toStruct(value map[string]any) (*structpb.Struct, error) {
	if value == nil {
		return nil, nil
	}
	return structpb.NewStruct(value)
}
func structMap(value *structpb.Struct) map[string]any {
	if value == nil {
		return nil
	}
	return value.AsMap()
}
func desiredStateForPhase(phase openshellv1.SandboxPhase) string {
	switch phase {
	case openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPED:
		return "Stopped"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_SUSPENDED:
		return "Suspended"
	default:
		return "Running"
	}
}
func restartPolicyName(value openshellv1.SandboxRestartPolicy) string {
	switch value {
	case openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_NEVER:
		return "Never"
	case openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_ON_FAILURE:
		return "OnFailure"
	case openshellv1.SandboxRestartPolicy_SANDBOX_RESTART_POLICY_ALWAYS:
		return "Always"
	default:
		return ""
	}
}
func phaseName(value openshellv1.SandboxPhase) string {
	switch value {
	case openshellv1.SandboxPhase_SANDBOX_PHASE_PROVISIONING:
		return "Provisioning"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_READY:
		return "Ready"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_ERROR:
		return "Error"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_DELETING:
		return "Deleting"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPING:
		return "Stopping"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_STOPPED:
		return "Stopped"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_STARTING:
		return "Starting"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_COMPLETED:
		return "Completed"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_SUSPENDING:
		return "Suspending"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_SUSPENDED:
		return "Suspended"
	default:
		return "Unknown"
	}
}
func conditionStatus(value string) metav1.ConditionStatus {
	switch strings.ToLower(value) {
	case "true":
		return metav1.ConditionTrue
	case "false":
		return metav1.ConditionFalse
	default:
		return metav1.ConditionUnknown
	}
}
func defaultReason(value string) string {
	if value == "" {
		return "GatewayReported"
	}
	return value
}
func readyCondition(phase openshellv1.SandboxPhase) (metav1.ConditionStatus, string, string) {
	switch phase {
	case openshellv1.SandboxPhase_SANDBOX_PHASE_READY:
		return metav1.ConditionTrue, "SandboxReady", "Sandbox is ready"
	case openshellv1.SandboxPhase_SANDBOX_PHASE_ERROR, openshellv1.SandboxPhase_SANDBOX_PHASE_COMPLETED:
		return metav1.ConditionFalse, "SandboxNotReady", "Sandbox is not ready"
	default:
		return metav1.ConditionUnknown, "SandboxPending", "Sandbox readiness has not been reported"
	}
}
func timestamp(value *timestamppb.Timestamp) time.Time {
	if value == nil {
		return time.Time{}
	}
	return value.AsTime()
}
func invalid(obj *api.OpenShellSandbox, path, message string) error {
	return apierrors.NewInvalid(schema.GroupKind{Group: api.GroupName, Kind: "OpenShellSandbox"}, obj.Name, field.ErrorList{field.Invalid(field.NewPath(path), nil, message)})
}
