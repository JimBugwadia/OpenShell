// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package pagination

import (
	"testing"

	metainternalversion "k8s.io/apimachinery/pkg/apis/meta/internalversion"
)

func TestPageContinuation(t *testing.T) {
	items := []string{"a", "b", "c"}
	first, meta, err := Page(items, "team-a", "app=agent", &metainternalversion.ListOptions{Limit: 2}, "list-rv")
	if err != nil {
		t.Fatal(err)
	}
	if len(first) != 2 || first[0] != "a" || first[1] != "b" || meta.Continue == "" {
		t.Fatalf("first page = %#v, metadata = %#v", first, meta)
	}
	second, next, err := Page(items, "team-a", "app=agent", &metainternalversion.ListOptions{Continue: meta.Continue, Limit: 2}, "ignored-rv")
	if err != nil {
		t.Fatal(err)
	}
	if len(second) != 1 || second[0] != "c" || next.Continue != "" || next.ResourceVersion != meta.ResourceVersion {
		t.Fatalf("second page = %#v, metadata = %#v", second, next)
	}
	if _, _, err := Page(items, "other", "app=agent", &metainternalversion.ListOptions{Continue: meta.Continue}, "new-rv"); err == nil {
		t.Fatal("continue token was accepted for another namespace")
	}
}
