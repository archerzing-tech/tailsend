// Phase-0 spike (DESIGN.md §3): run a tailsend-compatible receiver inside an
// embedded tsnet node — userspace WireGuard, no VPN/TUN, no official client.
// Verifies the three things the all-in-one mode depends on:
//  1. login (browser approval, state persisted in -dir for later runs)
//  2. inbound: the tailnet can reach a receiver served on s.Listen
//  3. outbound: s.Dial reaches another node's receiver through the netstack
//
// Product code (handlers, send logic) is intentionally NOT reused here —
// this only proves the transport. It lives in cmd/ so `go build .` for the
// CLI stays tsnet-free.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"

	"tailscale.com/tsnet"
)

func main() {
	hostname := flag.String("hostname", "tailsend-spike", "node name in the tailnet")
	dir := flag.String("dir", "", "tsnet state dir (default ~/.tailsend/tsnet-spike; login persists there)")
	port := flag.Int("port", 4782, "tailnet-side listen port")
	peer := flag.String("ping", "", "ip:port of a receiver to probe outbound through the netstack")
	flag.Parse()

	if *dir == "" {
		home, _ := os.UserHomeDir()
		*dir = filepath.Join(home, ".tailsend", "tsnet-spike")
	}
	if err := os.MkdirAll(*dir, 0o700); err != nil {
		log.Fatalf("state dir: %v", err)
	}

	s := &tsnet.Server{
		Hostname: *hostname,
		Dir:      *dir,
		// tsnet swallows its internal net/disco/derp logs into logtail unless
		// Logf is set — without this a dial failure is completely opaque.
		Logf:     log.Printf,
		UserLogf: log.Printf,
	}
	ctx := context.Background()

	// Start blocks until the node is configured. Without an auth key it
	// prints a login URL in a loop until someone approves it in a browser.
	if err := s.Start(); err != nil {
		log.Fatalf("start: %v", err)
	}
	st, err := s.Up(ctx)
	if err != nil {
		log.Fatalf("up: %v", err)
	}
	ip4, ip6 := s.TailscaleIPs()
	fmt.Printf("SPIKE node up: %s  ipv4=%s ipv6=%s  tailnet=%s\n",
		st.Self.HostName, ip4, ip6, st.CurrentTailnet.Name)

	// ---- inbound: a tailsend-flavored receiver on the tailnet side ----
	mux := http.NewServeMux()
	mux.HandleFunc("/api/ping", func(w http.ResponseWriter, r *http.Request) {
		peerIP := r.RemoteAddr
		if h, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
			peerIP = h
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"ok": true, "name": *hostname, "version": "spike", "confirm": false, "your_ip": peerIP,
		})
	})
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, "tsnet spike node %s is up; this receiver rode userspace WireGuard.\n", *hostname)
	})
	ln, err := s.Listen("tcp", fmt.Sprintf(":%d", *port))
	if err != nil {
		log.Fatalf("listen: %v", err)
	}
	go http.Serve(ln, mux) //nolint:errcheck — spike
	fmt.Printf("SPIKE serving on http://%s:%d (tailnet side)\n", ip4, *port)

	// ---- outbound: dial a real receiver through the netstack ----
	// A fresh node may need several seconds to hole-punch or fall back to a
	// relay, so allow generous attempts before declaring the path dead.
	if *peer != "" {
		var conn net.Conn
		var err error
		for attempt := 1; attempt <= 3 && conn == nil; attempt++ {
			pctx, cancel := context.WithTimeout(ctx, 30*time.Second)
			fmt.Printf("SPIKE outbound attempt %d -> %s\n", attempt, *peer)
			conn, err = s.Dial(pctx, "tcp", *peer)
			cancel()
			if err != nil && attempt < 3 {
				time.Sleep(2 * time.Second)
			}
		}
		if err != nil {
			fmt.Printf("SPIKE outbound dial %s FAILED: %v\n", *peer, err)
		} else {
			client := &http.Client{
				Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
					return conn, nil
				}},
				Timeout: 6 * time.Second,
			}
			resp, err := client.Get("http://" + *peer + "/api/ping")
			if err != nil {
				fmt.Printf("SPIKE outbound ping %s FAILED: %v\n", *peer, err)
			} else {
				var pr map[string]any
				json.NewDecoder(resp.Body).Decode(&pr) //nolint:errcheck — spike
				resp.Body.Close()
				fmt.Printf("SPIKE outbound ping %s OK: %v\n", *peer, pr)
			}
			conn.Close()
		}
	}

	fmt.Println("SPIKE running — Ctrl-C to stop. (Login state kept in " + *dir + ")")
	select {}
}
