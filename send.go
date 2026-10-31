package main

import (
	"bufio"
	"flag"
	"fmt"
	"net"
	"os"
	"strconv"
	"strings"
	"time"

	"tailsend/core"
)

func cmdSend(args []string) {
	fs := flag.NewFlagSet("send", flag.ExitOnError)
	port := fs.Int("port", defaultPort, "receiver port")
	token := fs.String("token", "", "shared secret (must match the receiver)")
	fs.Parse(args)
	rest := fs.Args()

	var peerArg string
	var pathArgs []string
	switch {
	case len(rest) >= 2:
		peerArg, pathArgs = rest[0], rest[1:]
	case len(rest) == 1:
		if _, err := os.Stat(rest[0]); err == nil {
			pathArgs = rest // a single path: pick the peer interactively
		} else {
			fatal("%q is not a file or directory here\nusage: tailsend send [peer] <path>...", rest[0])
		}
	default:
		fatal("nothing to send — pass at least one file or directory")
	}

	files, err := core.CollectFiles(pathArgs)
	if err != nil {
		fatal("%v", err)
	}
	if len(files) == 0 {
		fatal("no files to send")
	}
	var total int64
	for _, f := range files {
		total += f.Size
	}

	// Work out the target: an explicit argument, or an interactive pick.
	var target *Node
	targetPort := 0
	if peerArg != "" {
		target, targetPort, err = resolvePeer(nil, peerArg) // handles ip / ip:port
		if err != nil {
			st, serr := tailscaleStatus()
			if serr != nil {
				fatal("%v", serr)
			}
			target, targetPort, err = resolvePeer(peerNodes(st), peerArg)
			if err != nil {
				fatal("%v", err)
			}
		}
	} else {
		if !stdinIsTTY() {
			fatal("no peer given — pass one (e.g. tailsend send vps files...) or run from a terminal to pick from a list")
		}
		st, serr := tailscaleStatus()
		if serr != nil {
			fatal("%v", serr)
		}
		target, err = pickPeer(peerNodes(st))
		if err != nil {
			fatal("%v", err)
		}
	}

	if !target.Online {
		fmt.Fprintf(os.Stderr, "warning: %s is marked offline; trying anyway\n", target.HostName)
	}
	p := targetPort
	if p == 0 {
		p = *port
	}
	addr := net.JoinHostPort(primaryIP(target.IPs), strconv.Itoa(p))

	fmt.Printf("Sending %d %s (%s) to %s:%d\n", len(files), core.Plural("file", len(files)), core.Human(total), target.HostName, p)
	ping, err := core.PingReceiver(addr, *token)
	if err != nil {
		fatal("%v", err)
	}
	if ping.Dir != "" {
		fmt.Printf("Receiver %q saves into %s\n", ping.Name, ping.Dir)
	}

	// Consent (protocol v3): ask before the first byte moves. Receivers that
	// predate consent answer 404 and the batch proceeds as before.
	me, _ := os.Hostname()
	reqID, err := core.RequestTransfer(addr, *token, me, files)
	if err != nil {
		fatal("%v", err)
	}
	if reqID == "" {
		fmt.Printf("Receiver %q is an older version; sending without the confirmation step\n", ping.Name)
	}

	tty := stdoutIsTTY()
	printer := newProgressPrinter()
	sent, failed := 0, 0
	started := time.Now()
	for i, f := range files {
		fmt.Printf("[%d/%d] %s (%s)\n", i+1, len(files), f.Rel, core.Human(f.Size))
		err := core.UploadFile(addr, *token, me, reqID, f, printer)
		if tty {
			fmt.Println()
		}
		if err != nil {
			fmt.Fprintf(os.Stderr, "  failed: %v\n", err)
			failed++
			continue
		}
		sent++
	}
	fmt.Printf("Done: %d/%d %s sent in %s",
		sent, len(files), core.Plural("file", len(files)), time.Since(started).Round(time.Millisecond))
	if failed > 0 {
		fmt.Printf(", %d failed", failed)
	}
	fmt.Println()
	if failed > 0 {
		os.Exit(1)
	}
}

// pickPeer shows the numbered node list and reads a choice from stdin.
func pickPeer(nodes []*Node) (*Node, error) {
	if len(nodes) == 0 {
		return nil, fmt.Errorf("no other nodes in this tailnet")
	}
	fmt.Println("Peers:")
	for i, n := range nodes {
		mark, state := statusOnline, "online"
		if !n.Online {
			mark, state = statusOffline, "offline"
		}
		fmt.Printf("  %2d) %-18s %-16s %-8s %s %s\n", i+1, n.HostName, primaryIP(n.IPs), n.OS, mark, state)
	}
	fmt.Print("Send to number: ")
	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil && line == "" {
		return nil, fmt.Errorf("cancelled")
	}
	i, err := strconv.Atoi(strings.TrimSpace(line))
	if err != nil || i < 1 || i > len(nodes) {
		return nil, fmt.Errorf("invalid selection: %q", strings.TrimSpace(line))
	}
	return nodes[i-1], nil
}

// newProgressPrinter renders the single-line upload meter from the core
// sender's progress callbacks. Rate over the last sample window, so the
// number moves the way a speedometer does rather than a batch average.
func newProgressPrinter() core.Progress {
	var lastN int64
	lastT := time.Now()
	return func(done, total int64) {
		line := "\r  ↑ " + core.Human(done)
		if total > 0 {
			line += fmt.Sprintf(" / %s (%3.0f%%)", core.Human(total), 100*float64(done)/float64(total))
		}
		if el := time.Since(lastT).Seconds(); el > 0.2 {
			line += fmt.Sprintf("  %.1f MB/s", float64(done-lastN)/1e6/el)
		}
		lastN, lastT = done, time.Now()
		fmt.Print(line + "  ")
	}
}
