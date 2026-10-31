package main

import (
	"bufio"
	"flag"
	"fmt"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"tailsend/core"
)

func cmdReceive(args []string) {
	fs := flag.NewFlagSet("receive", flag.ExitOnError)
	port := fs.Int("port", defaultPort, "port to listen on")
	dir := fs.String("dir", "", "directory for incoming files (default: ~/Downloads/Tailsend)")
	bind := fs.String("bind", "", "address to bind (default: this machine's Tailscale IPv4)")
	token := fs.String("token", "", "shared secret senders must pass via -token")
	max := fs.Int64("max", 0, "reject single files larger than this many bytes (0 = unlimited)")
	ask := fs.Bool("ask", false, "ask before accepting each transfer (default: accept automatically)")
	fs.Parse(args)

	// Consent gate (protocol v3): with -ask, incoming transfers wait for a
	// human yes; without it, files are accepted automatically exactly as
	// before, which is what unattended NAS receivers want.
	gate := core.NewConsentGate(*ask, *token)
	if *ask && stdinIsTTY() {
		gate.Prompt = promptAllow
	}

	saveDir := *dir
	if saveDir == "" {
		saveDir = defaultSaveDir()
	}
	if abs, err := filepath.Abs(saveDir); err == nil {
		saveDir = abs
	}
	if err := os.MkdirAll(saveDir, 0o755); err != nil {
		fatal("cannot create %s: %v", saveDir, err)
	}

	// Default bind is the tailnet IP: unreachable from the LAN, no TLS needed.
	bindAddr := *bind
	advertise := []string{}
	switch {
	case bindAddr == "":
		st, err := tailscaleStatus()
		if err != nil {
			fatal("cannot detect this machine's Tailscale address: %v\nStart with -bind 0.0.0.0 (less private) or an explicit address instead.", err)
		}
		self := selfNode(st)
		bindAddr = primaryIP(self.IPs)
		if bindAddr == "" {
			fatal("no Tailscale IP found — is the tailnet up?")
		}
		advertise = self.IPs
	case bindAddr == "0.0.0.0" || bindAddr == "::":
		advertise = []string{bindAddr}
		if st, err := tailscaleStatus(); err == nil {
			if self := selfNode(st); self != nil && len(self.IPs) > 0 {
				advertise = self.IPs
			}
		}
	default:
		advertise = []string{bindAddr}
	}

	ln, err := net.Listen("tcp", net.JoinHostPort(bindAddr, strconv.Itoa(*port)))
	if err != nil {
		fatal("cannot listen on %s: %v", net.JoinHostPort(bindAddr, strconv.Itoa(*port)), err)
	}

	host, _ := os.Hostname()
	authNote := "none — every node your Tailscale ACLs allow can send (add -token to require a secret)"
	if *token != "" {
		authNote = "token required"
	}
	confirmNote := "off — files are accepted automatically"
	if *ask {
		confirmNote = "on — every transfer waits for confirmation"
	}
	fmt.Printf(`Tailsend receiver v%s

  this machine : %s
  saving to    : %s
  auth         : %s
  confirm      : %s
  reachable at :
`, version, host, saveDir, authNote, confirmNote)
	for _, ip := range advertise {
		fmt.Printf("                 http://%s\n", net.JoinHostPort(ip, strconv.Itoa(*port)))
	}
	fmt.Println()
	fmt.Println("Waiting for files… (Ctrl-C to stop)")

	rcv := &core.Receiver{
		Dir:     saveDir,
		Token:   *token,
		Max:     *max,
		Host:    host,
		Version: version,
		Gate:    gate,
		Peers:   cliPeers,
		Logf:    func(format string, a ...any) { fmt.Printf(format+"\n", a...) },
	}
	// The landing-page hint prefers a real tailnet IP over 0.0.0.0.
	rcv.AdvertiseIP = primaryIP(advertise)

	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
		<-sig
		fmt.Println("\nShutting down.")
		rcv.Stop()
	}()
	if err := rcv.Serve(ln); err != nil {
		fatal("%v", err)
	}
}

// cliPeers adapts `tailscale status` to the /api/peers view.
func cliPeers() (core.PeersInfo, error) {
	st, err := tailscaleStatus()
	if err != nil {
		return core.PeersInfo{}, err
	}
	info := core.PeersInfo{SelfIP: primaryIP(selfNode(st).IPs)}
	for _, p := range peerNodes(st) {
		ip := ""
		if len(p.IPs) > 0 {
			ip = p.IPs[0]
		}
		info.Peers = append(info.Peers, core.Peer{Name: p.HostName, IP: ip, OS: p.OS, Online: p.Online})
	}
	return info, nil
}

// promptAllow asks at the TTY whether to accept a transfer, giving up after
// ConfirmWait. An answer typed after the timeout still lands in stdin; the
// next prompt may consume it — acceptable for a line-oriented CLI.
func promptAllow(sender string, count int, total int64, names []string) (bool, string) {
	if !stdinIsTTY() {
		return false, "unattended"
	}
	list := names
	if len(list) > 5 {
		list = list[:5]
	}
	fmt.Printf("\nIncoming transfer from %s: %d %s (%s)\n", sender, count, core.Plural("file", count), core.Human(total))
	for _, n := range list {
		fmt.Printf("  · %s\n", n)
	}
	if len(names) > 5 {
		fmt.Printf("  … and %d more\n", count-len(list))
	}
	fmt.Printf("Accept? [y/N] (%ds to answer, then declined) ", int(core.ConfirmWait.Seconds()))
	answers := make(chan string, 1)
	go func() {
		sc := bufio.NewScanner(os.Stdin)
		if sc.Scan() {
			answers <- strings.TrimSpace(strings.ToLower(sc.Text()))
		} else {
			answers <- "" // EOF: stdin closed — nobody is answering
		}
	}()
	select {
	case line := <-answers:
		fmt.Println()
		if line == "" {
			return false, "unattended"
		}
		if line == "y" || line == "yes" {
			return true, ""
		}
		return false, "denied"
	case <-time.After(core.ConfirmWait):
		fmt.Println("- timed out, declined")
		return false, "timeout"
	}
}

func defaultSaveDir() string {
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		if _, err := os.Stat(filepath.Join(home, "Downloads")); err == nil {
			return filepath.Join(home, "Downloads", "Tailsend")
		}
	}
	return "tailsend-inbox"
}
