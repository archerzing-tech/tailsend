// Package mobile embeds a tailsend node (tsnet) plus the tailsend receiver
// and sender engines for mobile platforms via gomobile. It is the seam
// between the Kotlin foreground service (which owns this Node) and the
// Tauri bridge; everything here uses gomobile-bindable types — primitives,
// []string, and JSON strings for structured results.
//
// Lifecycle: NewNode → Start (async; watch Status for needs_login/running)
// → LoginURL shown in a browser → StartReceiver → Shutdown.
package mobile

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"strings"
	"sync"

	"tailscale.com/tsnet"
	"tailsend/core"
)

// listenPort is the tailsend port every implementation agrees on.
const listenPort = 4782

// Prompt asks the human whether to accept a transfer (protocol v3). It is
// implemented by the platform layer (Kotlin bridges it to the GUI dialog).
// Ask must return within 30 seconds; returning an empty or unknown reason is
// treated as a denial.
type Prompt interface {
	// Ask returns "accept" to grant, or a refusal reason such as "denied".
	Ask(sender string, count int64, total int64, namesJSON string) string
}

// Node is one embedded tailscale node carrying a tailsend receiver. Create
// one per app process; methods are safe for concurrent use.
type Node struct {
	Hostname string
	Dir      string
	Version  string

	mu           sync.Mutex
	srv          *tsnet.Server
	ln           net.Listener
	rcv          *core.Receiver
	prompt       Prompt
	recvStarting bool
	state        string // starting | needs_login | running | error
	loginURL     string
	ip           string
	tailnet      string
	lastErr      string
}

// NewNode creates a node named hostname whose login state persists in dir.
func NewNode(hostname, dir, version string) (*Node, error) {
	n := &Node{Hostname: hostname, Dir: dir, Version: version, state: "starting"}
	if err := n.startAsync(); err != nil {
		return nil, err
	}
	return n, nil
}

// startAsync launches the tsnet startup in the background; Status reports
// progress and LoginURL surfaces the browser authorization link.
func (n *Node) startAsync() error {
	s := &tsnet.Server{
		Hostname: n.Hostname,
		Dir:      n.Dir,
		// tsnet swallows its internal logs into logtail unless Logf is set;
		// routing them through logf also lets us catch the login URL.
		Logf:     n.logf,
		UserLogf: n.logf,
	}
	n.srv = s

	go func() {
		ctx := context.Background()
		if err := s.Start(); err != nil {
			n.setError("start: %v", err)
			return
		}
		st, err := s.Up(ctx)
		if err != nil {
			n.setError("up: %v", err)
			return
		}
		ip4, _ := s.TailscaleIPs()
		n.mu.Lock()
		n.state = "running"
		n.ip = ip4.String()
		if st.CurrentTailnet != nil {
			n.tailnet = st.CurrentTailnet.Name
		}
		n.loginURL = ""
		n.mu.Unlock()
		n.logf("tailsend node %s up at %s", n.Hostname, n.ip)
	}()
	return nil
}

// logf is the tsnet log sink: state transitions land here, and the login
// URL (printed by tsnet's auth loop while NeedsLogin) is captured for
// LoginURL(). Line shape: "…go to: https://login.tailscale.com/a/…".
func (n *Node) logf(format string, a ...any) {
	line := fmt.Sprintf(format, a...)
	if u, ok := extractAuthURL(line); ok {
		n.mu.Lock()
		n.state = "needs_login"
		n.loginURL = u
		n.mu.Unlock()
	}
}

// Status returns a JSON snapshot for UI polling:
// {state, login_url, ip, hostname, tailnet, error, receiver}
func (n *Node) Status() string {
	n.mu.Lock()
	defer n.mu.Unlock()
	st := struct {
		State    string `json:"state"`
		LoginURL string `json:"login_url,omitempty"`
		IP       string `json:"ip,omitempty"`
		Hostname string `json:"hostname"`
		Tailnet  string `json:"tailnet,omitempty"`
		Error    string `json:"error,omitempty"`
		Receiver string `json:"receiver,omitempty"` // "on" once StartReceiver succeeds
	}{
		State:    n.state,
		LoginURL: n.loginURL,
		Hostname: n.Hostname,
		Tailnet:  n.tailnet,
		Error:    n.lastErr,
	}
	if n.rcv != nil && n.ln != nil {
		st.Receiver = "on"
	}
	b, _ := json.Marshal(st)
	return string(b)
}

// LoginURL returns the browser authorization link while the node waits for
// login, or "" when none is needed.
func (n *Node) LoginURL() string {
	n.mu.Lock()
	defer n.mu.Unlock()
	return n.loginURL
}

// SetPrompt registers the human-confirmation callback used by StartReceiver.
func (n *Node) SetPrompt(p Prompt) {
	n.mu.Lock()
	n.prompt = p
	n.mu.Unlock()
}

// StartReceiver serves the tailsend receiver on the node's tailnet address.
// token may be "" (any tailnet node may send); confirm requires an accepted
// transfer request (via prompt, if set) before bytes flow. Idempotent.
func (n *Node) StartReceiver(token string, confirm bool) error {
	n.mu.Lock()
	s := n.srv
	prompt := n.prompt
	switch {
	case s == nil:
		n.mu.Unlock()
		return errors.New("node not started")
	case n.rcv != nil || n.recvStarting:
		n.mu.Unlock()
		return nil // already serving (or a call is mid-flight)
	}
	n.recvStarting = true
	n.mu.Unlock()

	ln, err := s.Listen("tcp", fmt.Sprintf(":%d", listenPort))
	if err != nil {
		n.mu.Lock()
		n.recvStarting = false
		n.mu.Unlock()
		return fmt.Errorf("listen: %v", err)
	}
	gate := core.NewConsentGate(confirm, token)
	if confirm && prompt != nil {
		gate.Prompt = func(sender string, count int, total int64, names []string) (bool, string) {
			nj, _ := json.Marshal(names)
			ans := prompt.Ask(sender, int64(count), total, string(nj))
			switch strings.ToLower(strings.TrimSpace(ans)) {
			case "accept", "yes", "true", "ok":
				return true, ""
			case "timeout":
				return false, "timeout"
			default:
				return false, "denied"
			}
		}
	}
	r := &core.Receiver{
		Dir:         receiverDir(n.Dir),
		Token:       token,
		Host:        n.Hostname,
		Version:     n.Version,
		Gate:        gate,
		AdvertiseIP: n.IP(),
	}
	n.mu.Lock()
	n.recvStarting = false
	n.rcv = r
	n.ln = ln
	n.mu.Unlock()
	go func() {
		if err := r.Serve(ln); err != nil {
			n.logf("receiver stopped: %v", err)
		}
		n.mu.Lock()
		n.rcv = nil
		n.ln = nil
		n.mu.Unlock()
	}()
	return nil
}

// StopReceiver shuts the receiver down; the node keeps running.
func (n *Node) StopReceiver() {
	n.mu.Lock()
	rcv := n.rcv
	n.mu.Unlock()
	if rcv != nil {
		rcv.Stop()
	}
}

// Shutdown tears the whole node down (login state stays on disk).
func (n *Node) Shutdown() {
	n.StopReceiver()
	n.mu.Lock()
	s := n.srv
	n.mu.Unlock()
	if s != nil {
		s.Close()
	}
}

// IP returns the node's tailnet IPv4 once running, else "".
func (n *Node) IP() string {
	n.mu.Lock()
	defer n.mu.Unlock()
	return n.ip
}

func (n *Node) setError(format string, a ...any) {
	n.mu.Lock()
	n.state = "error"
	n.lastErr = fmt.Sprintf(format, a...)
	n.mu.Unlock()
}

// receiverDir is where incoming files land: <state dir>/inbox.
func receiverDir(dir string) string {
	if dir == "" {
		return "tailsend-inbox"
	}
	return dir + "/inbox"
}

// extractAuthURL finds tsnet's printed login link.
func extractAuthURL(line string) (string, bool) {
	const marker = "go to: "
	if i := strings.Index(line, marker); i >= 0 {
		u := strings.TrimSpace(line[i+len(marker):])
		if strings.HasPrefix(u, "https://") {
			return u, true
		}
	}
	return "", false
}

// PingTarget reports whether a peer's receiver answers, as JSON
// {ok, name, version, dir, confirm, error}. The GUI uses `confirm` to show
// a "the other side will ask" hint before sending.
func (n *Node) PingTarget(addr, token string) string {
	pr, err := core.PingReceiver(addr, token)
	if err != nil {
		b, _ := json.Marshal(map[string]any{"ok": false, "error": err.Error()})
		return string(b)
	}
	b, _ := json.Marshal(pr)
	return string(b)
}

// SendFiles asks addr for consent (protocol v3) then uploads every path,
// returning JSON {sent, failed, results:[{name, ok, error}]}.
// files may name files or directories (directories are sent recursively).
// It blocks until the batch finishes — call from a background thread.
func (n *Node) SendFiles(addr, token, sender string, files []string) string {
	res := struct {
		Sent    int `json:"sent"`
		Failed  int `json:"failed"`
		Results []struct {
			Name  string `json:"name"`
			OK    bool   `json:"ok"`
			Error string `json:"error,omitempty"`
		} `json:"results"`
	}{}
	list, err := core.CollectFiles(files)
	if err != nil {
		b, _ := json.Marshal(map[string]any{"error": err.Error()})
		return string(b)
	}
	if len(list) == 0 {
		b, _ := json.Marshal(map[string]any{"error": "no files to send"})
		return string(b)
	}
	reqID, err := core.RequestTransfer(addr, token, sender, list)
	if err != nil {
		b, _ := json.Marshal(map[string]any{"error": err.Error()})
		return string(b)
	}
	for _, f := range list {
		err := core.UploadFile(addr, token, sender, reqID, f, nil)
		r := struct {
			Name  string `json:"name"`
			OK    bool   `json:"ok"`
			Error string `json:"error,omitempty"`
		}{Name: f.Rel, OK: err == nil}
		if err != nil {
			r.Error = err.Error()
			res.Failed++
		} else {
			res.Sent++
		}
		res.Results = append(res.Results, r)
	}
	b, _ := json.Marshal(res)
	return string(b)
}
