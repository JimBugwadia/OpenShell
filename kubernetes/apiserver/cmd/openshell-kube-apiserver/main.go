// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/apiserver"
	"github.com/NVIDIA/OpenShell/kubernetes/apiserver/pkg/gateway"
	"github.com/spf13/pflag"
	"k8s.io/apiserver/pkg/server/options"
)

func main() {
	flags := pflag.NewFlagSet("openshell-kube-apiserver", pflag.ExitOnError)
	recommended := options.NewRecommendedOptions("/registry/openshell.ai", nil)
	recommended.Etcd = nil
	recommended.Authentication.RemoteKubeConfigFileOptional = true
	recommended.Authorization.RemoteKubeConfigFileOptional = true
	recommended.SecureServing.SecureServingOptions.AddFlags(flags)
	recommended.Authentication.AddFlags(flags)
	recommended.Authorization.AddFlags(flags)

	gatewayAddress := flags.String("gateway-address", "127.0.0.1:8080", "OpenShell gateway gRPC address")
	gatewayCA := flags.String("gateway-ca-file", "", "CA certificate for the gateway connection")
	gatewayCert := flags.String("gateway-client-cert-file", "", "Client certificate for gateway mTLS")
	gatewayKey := flags.String("gateway-client-key-file", "", "Client key for gateway mTLS")
	gatewayToken := flags.String("gateway-token-file", "", "File containing the gateway bearer token")
	fakeGateway := flags.Bool("fake-gateway", false, "Use an in-memory gateway for local development only")
	if err := flags.Parse(os.Args[1:]); err != nil {
		log.Fatal(err)
	}

	var gatewayClient gateway.Interface
	var closeGateway func() error
	if *fakeGateway {
		fake := gateway.NewFake()
		fake.Workspaces["*"] = true
		gatewayClient = fake
	} else {
		client, err := gateway.Dial(context.Background(), *gatewayAddress, gateway.TLSOptions{CAFile: *gatewayCA, CertFile: *gatewayCert, KeyFile: *gatewayKey}, *gatewayToken)
		if err != nil {
			log.Fatal(err)
		}
		gatewayClient = client
		closeGateway = client.Close
	}
	if closeGateway != nil {
		defer func() {
			if err := closeGateway(); err != nil {
				log.Printf("close gateway client: %v", err)
			}
		}()
	}

	apiServer, err := apiserver.New(gatewayClient, recommended)
	if err != nil {
		log.Fatal(err)
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	if err := apiServer.PrepareRun().RunWithContext(ctx); err != nil {
		log.Fatal(err)
	}
}
