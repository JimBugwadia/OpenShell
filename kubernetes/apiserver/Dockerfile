# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

FROM golang:1.27-alpine AS build
WORKDIR /src
COPY sdk/go ./sdk/go
COPY kubernetes/apiserver ./kubernetes/apiserver
WORKDIR /src/kubernetes/apiserver
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/openshell-kube-apiserver ./cmd/openshell-kube-apiserver

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/openshell-kube-apiserver /openshell-kube-apiserver
USER nonroot:nonroot
ENTRYPOINT ["/openshell-kube-apiserver"]
