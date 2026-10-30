// Tailsend — send files between your machines over Tailscale.
package main

import (
	"fmt"
	"os"
)

const version = "0.0.1"

// defaultPort is arbitrary but registered nowhere; both sides must agree.
const defaultPort = 4782

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	switch os.Args[1] {
	case "list", "ls", "peers":
		cmdList(os.Args[2:])
	case "send":
		cmdSend(os.Args[2:])
	case "receive", "recv":
		cmdReceive(os.Args[2:])
	case "version", "-v", "--version":
		fmt.Printf("tailsend %s\n", version)
	case "help", "-h", "--help":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "tailsend: unknown command %q\n\n", os.Args[1])
		usage()
		os.Exit(2)
	}
}

func usage() {
	fmt.Printf(`tailsend %s — send files between your machines over Tailscale.

Usage:
  tailsend list                       Discover Tailscale nodes (your send targets)
  tailsend send [flags] [peer] <path>...
  tailsend receive [flags]            Start the receiver and wait for files

Commands:
  list      Show this machine and every other node in the tailnet.
  send      Send files or directories to a peer. Omit <peer> to pick one
            from an interactive list. Directories are sent recursively
            and keep their relative structure on the receiving side.
  receive   Run on every machine you want to receive on. Files are
            streamed to disk as they arrive, into ~/Downloads/Tailsend
            by default.

Flags (send):
  -port N      Receiver port (default %d)
  -token STR   Shared secret, if the receiver demands one
  Peer may be a hostname, a (partial) Tailscale name, or an IP. With an
  IP you may append ":port" to override -port.

Flags (receive):
  -port N      Port to listen on (default %d)
  -dir PATH    Save incoming files here (default: ~/Downloads/Tailsend)
  -bind ADDR   Bind address (default: this machine's Tailscale IPv4)
  -token STR   Require this shared secret from senders
  -max BYTES   Reject single files larger than this (default: unlimited)

Examples:
  tailsend list
  tailsend send vps ~/report.pdf ~/photos
  tailsend send                       # pick a peer, paths are positional
  tailsend send 100.64.0.12:4782 backup.tar.gz
  tailsend receive -token "our-secret"
`, version, defaultPort, defaultPort)
}
