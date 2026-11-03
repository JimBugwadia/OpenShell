// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package v1alpha1

import (
	"encoding/json"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
)

const (
	GroupName = "openshell.ai"
	Version   = "v1alpha1"
)

// OpenShellSandbox describes a gateway-owned sandbox.
type OpenShellSandbox struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              OpenShellSandboxSpec   `json:"spec,omitempty"`
	Status            OpenShellSandboxStatus `json:"status,omitempty"`
}

type OpenShellSandboxSpec struct {
	WorkloadTemplate string            `json:"workloadTemplate,omitempty"`
	Template         *SandboxTemplate  `json:"template,omitempty"`
	Command          []string          `json:"command,omitempty"`
	TTY              bool              `json:"tty,omitempty"`
	RestartPolicy    string            `json:"restartPolicy,omitempty"`
	Environment      map[string]string `json:"environment,omitempty"`
	LogLevel         string            `json:"logLevel,omitempty"`
	Providers        []string          `json:"providers,omitempty"`
	Policy           json.RawMessage   `json:"policy,omitempty"`
	Resources        map[string]any    `json:"resources,omitempty"`
	DesiredState     string            `json:"desiredState,omitempty"`
}

type SandboxTemplate struct {
	Image            string            `json:"image,omitempty"`
	RuntimeClassName string            `json:"runtimeClassName,omitempty"`
	AgentSocket      string            `json:"agentSocket,omitempty"`
	Labels           map[string]string `json:"labels,omitempty"`
	Annotations      map[string]string `json:"annotations,omitempty"`
	Environment      map[string]string `json:"environment,omitempty"`
	Resources        map[string]any    `json:"resources,omitempty"`
	DriverConfig     map[string]any    `json:"driverConfig,omitempty"`
	UserNamespaces   *bool             `json:"userNamespaces,omitempty"`
}

type OpenShellSandboxStatus struct {
	Phase                string             `json:"phase,omitempty"`
	Conditions           []metav1.Condition `json:"conditions,omitempty"`
	ObservedGeneration   int64              `json:"observedGeneration,omitempty"`
	AgentPod             string             `json:"agentPod,omitempty"`
	CurrentPolicyVersion uint32             `json:"currentPolicyVersion,omitempty"`
}

type OpenShellSandboxList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []OpenShellSandbox `json:"items"`
}

type OpenShellSandboxTemplate struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              SandboxWorkloadTemplateSpec `json:"spec,omitempty"`
}

type SandboxWorkloadTemplateSpec struct {
	Workload            SandboxWorkloadConfig `json:"workload,omitempty"`
	DriverConfig        map[string]any        `json:"driverConfig,omitempty"`
	DesiredServiceLevel map[string]any        `json:"desiredServiceLevel,omitempty"`
}

type SandboxWorkloadConfig struct {
	Image       string            `json:"image,omitempty"`
	Environment map[string]string `json:"environment,omitempty"`
	Resources   map[string]any    `json:"resources,omitempty"`
}

type OpenShellSandboxTemplateList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []OpenShellSandboxTemplate `json:"items"`
}

type OpenShellProvider struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              OpenShellProviderSpec `json:"spec,omitempty"`
}

type OpenShellProviderSpec struct {
	Type             string            `json:"type,omitempty"`
	Config           map[string]string `json:"config,omitempty"`
	ProfileWorkspace string            `json:"profileWorkspace,omitempty"`
}

type OpenShellProviderList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []OpenShellProvider `json:"items"`
}

type OpenShellWorkspace struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Status            OpenShellWorkspaceStatus `json:"status,omitempty"`
}

type OpenShellWorkspaceStatus struct {
	Phase string `json:"phase,omitempty"`
}

type OpenShellWorkspaceList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []OpenShellWorkspace `json:"items"`
}

func (in *OpenShellWorkspace) DeepCopyObject() runtime.Object     { return deepCopy(in) }
func (in *OpenShellWorkspaceList) DeepCopyObject() runtime.Object { return deepCopy(in) }

func (in *OpenShellSandbox) DeepCopyObject() runtime.Object             { return deepCopy(in) }
func (in *OpenShellSandboxList) DeepCopyObject() runtime.Object         { return deepCopy(in) }
func (in *OpenShellSandboxTemplate) DeepCopyObject() runtime.Object     { return deepCopy(in) }
func (in *OpenShellSandboxTemplateList) DeepCopyObject() runtime.Object { return deepCopy(in) }
func (in *OpenShellProvider) DeepCopyObject() runtime.Object            { return deepCopy(in) }
func (in *OpenShellProviderList) DeepCopyObject() runtime.Object        { return deepCopy(in) }

func deepCopy[T any](in *T) *T {
	if in == nil {
		return nil
	}
	out := new(T)
	data, _ := json.Marshal(in)
	_ = json.Unmarshal(data, out)
	return out
}
