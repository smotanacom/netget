//go:build tools

// Package tools pins the unchanged ACME peers built by ../install_peers.py: lego (client, Go)
// and Pebble with pebble-challtestsrv (server and its test DNS, Go).
package tools

import (
	_ "github.com/go-acme/lego/v4/cmd/lego"
	_ "github.com/letsencrypt/pebble/v2/cmd/pebble"
	_ "github.com/letsencrypt/pebble/v2/cmd/pebble-challtestsrv"
)
