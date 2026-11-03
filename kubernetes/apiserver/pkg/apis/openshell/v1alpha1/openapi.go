// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package v1alpha1

import (
	"encoding/json"
	"reflect"
	"strings"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/kube-openapi/pkg/common"
	"k8s.io/kube-openapi/pkg/validation/spec"
)

// GetOpenAPIDefinitions describes the typed API, keeping only explicit JSON
// payloads and driver envelopes opaque.
func GetOpenAPIDefinitions(_ common.ReferenceCallback) map[string]common.OpenAPIDefinition {
	definitions := map[string]common.OpenAPIDefinition{}
	for _, value := range []any{
		OpenShellSandbox{}, OpenShellSandboxList{},
		OpenShellSandboxTemplate{}, OpenShellSandboxTemplateList{},
		OpenShellProvider{}, OpenShellProviderList{},
		OpenShellWorkspace{}, OpenShellWorkspaceList{},
		metav1.APIGroup{}, metav1.APIGroupList{}, metav1.APIResourceList{}, metav1.APIVersions{},
		metav1.Status{}, metav1.WatchEvent{}, metav1.DeleteOptions{}, metav1.CreateOptions{},
		metav1.UpdateOptions{}, metav1.PatchOptions{}, metav1.ListOptions{}, metav1.Patch{},
	} {
		t := reflect.TypeOf(value)
		schema := schemaFor(t)
		if t.PkgPath() == reflect.TypeOf(OpenShellSandbox{}).PkgPath() {
			schema.Extensions = spec.Extensions{"x-kubernetes-group-version-kind": []any{
				map[string]any{"group": GroupName, "version": Version, "kind": t.Name()},
			}}
		}
		definitions[t.PkgPath()+"."+t.Name()] = common.OpenAPIDefinition{Schema: schema}
	}
	return definitions
}

func schemaFor(t reflect.Type) spec.Schema {
	if t.Kind() == reflect.Pointer {
		return schemaFor(t.Elem())
	}
	if t == reflect.TypeOf(metav1.Time{}) || t == reflect.TypeOf(metav1.MicroTime{}) {
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"string"}, Format: "date-time"}}
	}
	if t == reflect.TypeOf(json.RawMessage{}) || t == reflect.TypeOf(metav1.FieldsV1{}) {
		return preservedObjectSchema()
	}
	switch t.Kind() {
	case reflect.Struct:
		properties := map[string]spec.Schema{}
		var required []string
		for i := 0; i < t.NumField(); i++ {
			field := t.Field(i)
			if !field.IsExported() {
				continue
			}
			parts := strings.Split(field.Tag.Get("json"), ",")
			if parts[0] == "-" {
				continue
			}
			if parts[0] == "" && field.Anonymous {
				for name, schema := range schemaFor(field.Type).Properties {
					properties[name] = schema
				}
				continue
			}
			if parts[0] == "" {
				continue
			}
			properties[parts[0]] = schemaFor(field.Type)
			if !strings.Contains(field.Tag.Get("json"), "omitempty") {
				required = append(required, parts[0])
			}
		}
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"object"}, Properties: properties, Required: required}}
	case reflect.Map:
		if t.Elem().Kind() == reflect.Interface {
			return preservedObjectSchema()
		}
		value := schemaFor(t.Elem())
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"object"}, AdditionalProperties: &spec.SchemaOrBool{Allows: true, Schema: &value}}}
	case reflect.Slice:
		element := schemaFor(t.Elem())
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"array"}, Items: &spec.SchemaOrArray{Schema: &element}}}
	case reflect.Bool:
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"boolean"}}}
	case reflect.Int, reflect.Int32, reflect.Int64, reflect.Uint32:
		return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"integer"}, Format: "int64"}}
	default:
		return stringSchema()
	}
}

func stringSchema() spec.Schema {
	return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"string"}}}
}
func preservedObjectSchema() spec.Schema {
	return spec.Schema{SchemaProps: spec.SchemaProps{Type: []string{"object"}}, VendorExtensible: spec.VendorExtensible{Extensions: spec.Extensions{"x-kubernetes-preserve-unknown-fields": true}}}
}
