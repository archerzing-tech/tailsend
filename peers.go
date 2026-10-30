package main

import (
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"sort"
	"strconv"
	"strings"
	"text/tabwriter"
	"time"
)

const (
	statusOnline  = "●"
	statusOffline = "○"
)

// Node is one machine seen through `tailscale status`.
type Node struct {
	HostName string
	FQDN     string
	IPs      []string
	Online   bool
	OS       string
	LastSeen time.Time // zero for online nodes
	Self     bool
}

type tsStatus struct {
	BackendState string             `json:"BackendState"`
	Self         *tsPeer            `json:"Self"`
	Peer         map[string]*tsPeer `json:"Peer"`
}

type tsPeer struct {
	HostName     string   `json:"HostName"`
	DNSName      string   `json:"DNSName"`
	OS           string   `json:"OS"`
	TailscaleIPs []string `json:"TailscaleIPs"`
	Online       bool     `json:"Online"`
	LastSeen     string   `json:"LastSeen"`
}

// tailscaleBin finds the CLI: App Store installs on macOS don't put it on PATH.
func tailscaleBin() string {
	if p, err := exec.LookPath("tailscale"); err == nil {
		return p
	}
	for _, c := range []string{
		"/usr/local/bin/tailscale",
		"/opt/homebrew/bin/tailscale",
		"/Applications/Tailscale.app/Contents/MacOS/Tailscale",
		"/usr/bin/tailscale",
	} {
		if fi, err := os.Stat(c); err == nil && !fi.IsDir() {
			return c
		}
	}
	return "tailscale"
}

func tailscaleStatus() (*tsStatus, error) {
	out, err := exec.Command(tailscaleBin(), "status", "--json").Output()
	if err != nil {
		if ee, ok := err.(*exec.ExitError); ok && len(ee.Stderr) > 0 {
			return nil, fmt.Errorf("tailscale status failed: %s", strings.TrimSpace(string(ee.Stderr)))
		}
		return nil, fmt.Errorf("cannot run %q: is Tailscale installed and running?", tailscaleBin())
	}
	var st tsStatus
	if err := json.Unmarshal(out, &st); err != nil {
		return nil, fmt.Errorf("cannot parse `tailscale status --json` output: %v", err)
	}
	return &st, nil
}

func nodeFrom(p *tsPeer, self bool) *Node {
	if p == nil {
		return nil
	}
	n := &Node{
		HostName: p.HostName,
		FQDN:     strings.TrimSuffix(p.DNSName, "."),
		IPs:      p.TailscaleIPs,
		Online:   p.Online,
		OS:       p.OS,
		Self:     self,
	}
	if t, err := time.Parse(time.RFC3339, p.LastSeen); err == nil {
		n.LastSeen = t
	}
	if n.HostName == "" && n.FQDN != "" {
		n.HostName = strings.SplitN(n.FQDN, ".", 2)[0]
	}
	return n
}

func selfNode(st *tsStatus) *Node { return nodeFrom(st.Self, true) }

// peerNodes lists every node except this machine, online ones first.
func peerNodes(st *tsStatus) []*Node {
	var nodes []*Node
	for _, p := range st.Peer {
		if n := nodeFrom(p, false); n != nil {
			nodes = append(nodes, n)
		}
	}
	sort.Slice(nodes, func(i, j int) bool {
		a, b := nodes[i], nodes[j]
		if a.Online != b.Online {
			return a.Online
		}
		return strings.ToLower(a.HostName) < strings.ToLower(b.HostName)
	})
	return nodes
}

// primaryIP prefers the IPv4 tailscale address.
func primaryIP(ips []string) string {
	for _, ip := range ips {
		if !strings.Contains(ip, ":") {
			return ip
		}
	}
	if len(ips) > 0 {
		return ips[0]
	}
	return ""
}

// resolvePeer matches a user-supplied target against the node list.
// An "ip" or "ip:port" literal is accepted even if absent from the list,
// which also makes loopback testing possible; in that case the returned
// port is non-zero and overrides the -port flag.
func resolvePeer(nodes []*Node, target string) (*Node, int, error) {
	port := 0
	q := target
	if h, p, err := net.SplitHostPort(target); err == nil {
		if pi, err := strconv.Atoi(p); err == nil && pi > 0 && pi < 65536 {
			q, port = h, pi
		}
	}
	if ip := net.ParseIP(q); ip != nil {
		return &Node{HostName: q, IPs: []string{q}, Online: true}, port, nil
	}
	q = strings.ToLower(strings.TrimSuffix(q, "."))
	for _, n := range nodes {
		if strings.EqualFold(n.HostName, q) || strings.EqualFold(n.FQDN, q) {
			return n, port, nil
		}
	}
	var hits []*Node
	for _, n := range nodes {
		if strings.Contains(strings.ToLower(n.HostName), q) ||
			strings.Contains(strings.ToLower(n.FQDN), q) {
			hits = append(hits, n)
		}
	}
	switch len(hits) {
	case 1:
		return hits[0], port, nil
	case 0:
		return nil, 0, fmt.Errorf("no node matches %q — run `tailsend list` to see the available peers", target)
	default:
		names := make([]string, len(hits))
		for i, n := range hits {
			names[i] = n.HostName
		}
		return nil, 0, fmt.Errorf("%q is ambiguous: %s", target, strings.Join(names, ", "))
	}
}

func cmdList(args []string) {
	st, err := tailscaleStatus()
	if err != nil {
		fatal("%v", err)
	}
	self := selfNode(st)
	if self == nil {
		fatal("tailscale reported no self node — are you logged in?")
	}
	fmt.Printf("This machine: %s", self.HostName)
	if ip := primaryIP(self.IPs); ip != "" {
		fmt.Printf(" (%s)", ip)
	}
	fmt.Println()
	if st.BackendState != "" && st.BackendState != "Running" {
		fmt.Printf("warning: tailscale backend state is %q\n", st.BackendState)
	}
	peers := peerNodes(st)
	if len(peers) == 0 {
		fmt.Println("No other nodes in this tailnet.")
		return
	}
	w := tabwriter.NewWriter(os.Stdout, 2, 4, 2, ' ', 0)
	fmt.Fprintln(w, "  STATUS\tADDRESS\tHOSTNAME\tOS\tLAST SEEN")
	for _, n := range peers {
		status, last := statusOnline+" online", ""
		if !n.Online {
			status, last = statusOffline+" offline", humanAgo(n.LastSeen)
		}
		fmt.Fprintf(w, "  %s\t%s\t%s\t%s\t%s\n", status, primaryIP(n.IPs), n.HostName, n.OS, last)
	}
	w.Flush()
	fmt.Printf("\nSend with: tailsend send <hostname> <file-or-directory>\n")
}
