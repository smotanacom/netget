//go:build tools

// Package tools pins the unchanged BMP peers built by ../install_peers.py: GoBGP's gobgpd and
// gobgp (a BGP speaker exporting BMP, Go) and gobmp (a BMP collector, Go).
package tools

import (
	_ "github.com/osrg/gobgp/v4/cmd/gobgp"
	_ "github.com/osrg/gobgp/v4/cmd/gobgpd"
	_ "github.com/sbezverk/gobmp/cmd/gobmp"
)
