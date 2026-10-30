package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"mime/multipart"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

type fileToSend struct {
	Src  string // path on this machine
	Rel  string // path it should take on the receiver
	Size int64
}

type pingResult struct {
	OK      bool   `json:"ok"`
	Name    string `json:"name"`
	Version string `json:"version"`
	Dir     string `json:"dir"`
	Error   string `json:"error"`
}

type uploadResult struct {
	OK    bool   `json:"ok"`
	Saved string `json:"saved"`
	Size  int64  `json:"size"`
	Error string `json:"error"`
}

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

	files, err := collectFiles(pathArgs)
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

	fmt.Printf("Sending %d %s (%s) to %s:%d\n", len(files), plural("file", len(files)), human(total), target.HostName, p)
	ping, err := pingReceiver(addr, *token)
	if err != nil {
		fatal("%v", err)
	}
	if ping.Dir != "" {
		fmt.Printf("Receiver %q saves into %s\n", ping.Name, ping.Dir)
	}

	me, _ := os.Hostname()
	tty := stdoutIsTTY()
	sent, failed := 0, 0
	started := time.Now()
	for i, f := range files {
		fmt.Printf("[%d/%d] %s (%s)\n", i+1, len(files), f.Rel, human(f.Size))
		err := uploadOne(addr, *token, me, f, tty)
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
		sent, len(files), plural("file", len(files)), time.Since(started).Round(time.Millisecond))
	if failed > 0 {
		fmt.Printf(", %d failed", failed)
	}
	fmt.Println()
	if failed > 0 {
		os.Exit(1)
	}
}

// collectFiles expands the given paths; directories are walked recursively
// and each file keeps its path relative to the directory's base name.
func collectFiles(paths []string) ([]fileToSend, error) {
	var out []fileToSend
	seen := map[string]bool{}
	for _, p := range paths {
		info, err := os.Stat(p)
		if err != nil {
			return nil, err
		}
		if !info.IsDir() {
			if !seen[p] {
				seen[p] = true
				out = append(out, fileToSend{Src: p, Rel: filepath.Base(p), Size: info.Size()})
			}
			continue
		}
		prefix := filepath.Base(p)
		err = filepath.WalkDir(p, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if d.IsDir() {
				return nil
			}
			rel, err := filepath.Rel(p, path)
			if err != nil {
				return err
			}
			fi, err := d.Info()
			if err != nil {
				return err
			}
			if !seen[path] {
				seen[path] = true
				out = append(out, fileToSend{Src: path, Rel: filepath.Join(prefix, rel), Size: fi.Size()})
			}
			return nil
		})
		if err != nil {
			return nil, err
		}
	}
	return out, nil
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

func pingReceiver(addr, token string) (*pingResult, error) {
	client := &http.Client{Timeout: 5 * time.Second}
	req, err := http.NewRequest("GET", "http://"+addr+"/api/ping", nil)
	if err != nil {
		return nil, err
	}
	if token != "" {
		req.Header.Set("X-Tailsend-Token", token)
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("no tailsend receiver at http://%s — run `tailsend receive` on that machine first\n(%v)", addr, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusForbidden {
		return nil, fmt.Errorf("receiver at %s requires a token; pass -token", addr)
	}
	var pr pingResult
	if err := json.NewDecoder(resp.Body).Decode(&pr); err != nil {
		return nil, fmt.Errorf("http://%s answered but is not tailsend (HTTP %d)", addr, resp.StatusCode)
	}
	return &pr, nil
}

// uploadOne streams one file as a multipart POST. The body is produced by a
// goroutine writing into an io.Pipe, so nothing is buffered in memory.
func uploadOne(addr, token, sender string, f fileToSend, showProgress bool) error {
	src, err := os.Open(f.Src)
	if err != nil {
		return err
	}
	defer src.Close()

	pr, pw := io.Pipe()
	mw := multipart.NewWriter(pw)
	go func() {
		var err error
		defer func() { pw.CloseWithError(err) }()
		if err = mw.WriteField("sender", sender); err != nil {
			return
		}
		var fw io.Writer
		if fw, err = mw.CreateFormFile("file", url.PathEscape(f.Rel)); err != nil {
			return
		}
		var r io.Reader = src
		if showProgress {
			r = &progress{r: src, total: f.Size, start: time.Now()}
		}
		if _, err = io.Copy(fw, r); err != nil {
			return
		}
		err = mw.Close()
	}()

	req, err := http.NewRequest("POST", "http://"+addr+"/api/upload", pr)
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "multipart/form-data; boundary="+mw.Boundary())
	if token != "" {
		req.Header.Set("X-Tailsend-Token", token)
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return fmt.Errorf("upload failed: %v", err)
	}
	defer resp.Body.Close()
	var res uploadResult
	if err := json.NewDecoder(resp.Body).Decode(&res); err != nil {
		return fmt.Errorf("unexpected receiver response (HTTP %d)", resp.StatusCode)
	}
	if !res.OK || resp.StatusCode != http.StatusOK {
		return fmt.Errorf("receiver said: %s", res.Error)
	}
	return nil
}

// progress prints a rate-limited single-line meter when stdout is a terminal.
type progress struct {
	r     io.Reader
	total int64
	done  int64
	start time.Time
	next  time.Time
}

func (p *progress) Read(b []byte) (int, error) {
	n, err := p.r.Read(b)
	p.done += int64(n)
	now := time.Now()
	if now.After(p.next) || err != nil {
		p.next = now.Add(200 * time.Millisecond)
		line := "\r  ↑ " + human(p.done)
		if p.total > 0 {
			line += fmt.Sprintf(" / %s (%3.0f%%)", human(p.total), 100*float64(p.done)/float64(p.total))
		}
		if e := now.Sub(p.start).Seconds(); e > 0.2 {
			line += fmt.Sprintf("  %.1f MB/s", float64(p.done)/1e6/e)
		}
		fmt.Print(line + "  ")
	}
	return n, err
}
