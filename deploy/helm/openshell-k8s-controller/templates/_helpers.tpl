{{/*
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
*/}}

{{- define "openshell-k8s-controller.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "openshell-k8s-controller.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "openshell-k8s-controller.selectorLabels" -}}
app.kubernetes.io/name: {{ include "openshell-k8s-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "openshell-k8s-controller.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "openshell-k8s-controller.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "openshell-k8s-controller.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "openshell-k8s-controller.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- required "serviceAccount.name is required when serviceAccount.create is false" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "openshell-k8s-controller.watchNamespace" -}}
{{- default .Release.Namespace .Values.watchNamespace }}
{{- end }}

{{- define "openshell-k8s-controller.validate" -}}
{{- $_ := required "gateway.url is required" .Values.gateway.url }}
{{- $_ := required "gateway.workspace is required" .Values.gateway.workspace }}
{{- if and (gt (int .Values.replicaCount) 1) (not .Values.leaderElection.enabled) }}
{{- fail "replicaCount above 1 requires leaderElection.enabled" }}
{{- end }}
{{- if and (not .Values.gateway.tls.secretName) (not .Values.gateway.token.secretName) }}
{{- fail "set gateway.tls.secretName, gateway.token.secretName, or both" }}
{{- end }}
{{- end }}
