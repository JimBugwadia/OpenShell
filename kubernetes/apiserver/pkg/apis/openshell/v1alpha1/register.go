// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
)

var SchemeGroupVersion = schema.GroupVersion{Group: GroupName, Version: Version}

func AddToScheme(scheme *runtime.Scheme) error {
	scheme.AddKnownTypes(SchemeGroupVersion,
		&OpenShellSandbox{}, &OpenShellSandboxList{},
		&OpenShellSandboxTemplate{}, &OpenShellSandboxTemplateList{},
		&OpenShellProvider{}, &OpenShellProviderList{},
		&OpenShellWorkspace{}, &OpenShellWorkspaceList{},
	)
	scheme.AddKnownTypes(schema.GroupVersion{Group: GroupName, Version: runtime.APIVersionInternal},
		&OpenShellSandbox{}, &OpenShellSandboxList{},
		&OpenShellSandboxTemplate{}, &OpenShellSandboxTemplateList{},
		&OpenShellProvider{}, &OpenShellProviderList{},
		&OpenShellWorkspace{}, &OpenShellWorkspaceList{},
	)
	metav1.AddToGroupVersion(scheme, SchemeGroupVersion)
	return nil
}
