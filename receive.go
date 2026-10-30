package main

import (
	"crypto/subtle"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"mime"
	"mime/multipart"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

var errTooBig = errors.New("file exceeds the -max limit")

func cmdReceive(args []string) {
	fs := flag.NewFlagSet("receive", flag.ExitOnError)
	port := fs.Int("port", defaultPort, "port to listen on")
	dir := fs.String("dir", "", "directory for incoming files (default: ~/Downloads/Tailsend)")
	bind := fs.String("bind", "", "address to bind (default: this machine's Tailscale IPv4)")
	token := fs.String("token", "", "shared secret senders must pass via -token")
	max := fs.Int64("max", 0, "reject single files larger than this many bytes (0 = unlimited)")
	fs.Parse(args)

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
	fmt.Printf(`Tailsend receiver v%s

  this machine : %s
  saving to    : %s
  auth         : %s
  reachable at :
`, version, host, saveDir, authNote)
	for _, ip := range advertise {
		fmt.Printf("                 http://%s\n", net.JoinHostPort(ip, strconv.Itoa(*port)))
	}
	fmt.Println()
	fmt.Println("Waiting for files… (Ctrl-C to stop)")

	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		fmt.Fprintf(w, "Tailsend receiver %s is running on %s.\nSend files with: tailsend send %s <file-or-directory>\n",
			version, host, primaryIP(advertise))
	})
	mux.HandleFunc("/api/ping", func(w http.ResponseWriter, r *http.Request) {
		if !authed(r, *token) {
			writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
			return
		}
		// The caller's address as seen from here; mobile clients use it to
		// learn their own tailnet IP.
		peerIP := r.RemoteAddr
		if h, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
			peerIP = h
		}
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "name": host, "version": version, "dir": saveDir, "your_ip": peerIP})
	})
	mux.HandleFunc("/api/upload", func(w http.ResponseWriter, r *http.Request) {
		handleUpload(w, r, saveDir, *token, *max)
	})

	srv := &http.Server{Handler: mux, ReadHeaderTimeout: 15 * time.Second}
	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
		<-sig
		fmt.Println("\nShutting down.")
		srv.Close()
	}()
	if err := srv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		fatal("%v", err)
	}
}

func handleUpload(w http.ResponseWriter, r *http.Request, saveDir, token string, max int64) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	mediaType, params, err := mime.ParseMediaType(r.Header.Get("Content-Type"))
	if err != nil || !strings.HasPrefix(mediaType, "multipart/") {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "expected multipart/form-data"})
		return
	}
	mr := multipart.NewReader(r.Body, params["boundary"])
	sender, savedRel, savedSize, files := "", "", int64(0), 0
	for {
		part, err := mr.NextPart()
		if err == io.EOF {
			break
		}
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad multipart stream"})
			return
		}
		switch part.FormName() {
		case "sender":
			b, _ := io.ReadAll(io.LimitReader(part, 256))
			sender = strings.TrimSpace(string(b))
		case "file":
			// Filenames travel percent-encoded so UTF-8 and "/" survive headers.
			rel, uerr := url.PathUnescape(part.FileName())
			if uerr != nil || rel == "" {
				rel = part.FileName()
			}
			dest, err := safeJoin(saveDir, rel)
			if err != nil {
				writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			dest = uniquePath(dest)
			if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
				writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			n, err := saveStream(part, dest, max)
			if err != nil {
				code := http.StatusInternalServerError
				if errors.Is(err, errTooBig) {
					code = http.StatusRequestEntityTooLarge
				}
				writeJSON(w, code, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			relDest, _ := filepath.Rel(saveDir, dest)
			files++
			savedRel, savedSize = relDest, n
			fmt.Printf("%s ← %s (%s) from %s\n", time.Now().Format("15:04:05"), relDest, human(n), orUnknown(sender))
		}
		part.Close()
	}
	if files == 0 {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "no file part in request"})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "saved": savedRel, "size": savedSize})
}

// saveStream writes to dest.part and renames on success, so a dropped
// connection never leaves a half-written file at the final name.
func saveStream(r io.Reader, dest string, max int64) (int64, error) {
	tmp := dest + ".part"
	f, err := os.Create(tmp)
	if err != nil {
		return 0, err
	}
	var n int64
	if max > 0 {
		n, err = io.Copy(f, io.LimitReader(r, max+1))
		if err == nil && n > max {
			f.Close()
			os.Remove(tmp)
			return n, fmt.Errorf("%w (%s)", errTooBig, human(max))
		}
	} else {
		n, err = io.Copy(f, r)
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		os.Remove(tmp)
		return n, err
	}
	if err := os.Rename(tmp, dest); err != nil {
		os.Remove(tmp)
		return n, err
	}
	return n, nil
}

// safeJoin joins rel under base, refusing traversal outside it.
func safeJoin(base, rel string) (string, error) {
	rel = filepath.ToSlash(strings.TrimSpace(rel))
	var parts []string
	for _, seg := range strings.Split(rel, "/") {
		switch seg {
		case "", ".":
			continue
		case "..":
			return "", fmt.Errorf("refusing suspicious path %q", rel)
		default:
			parts = append(parts, seg)
		}
	}
	if len(parts) == 0 {
		return "", fmt.Errorf("empty file name")
	}
	return filepath.Join(append([]string{base}, parts...)...), nil
}

// authed checks the optional shared secret.
func authed(r *http.Request, token string) bool {
	if token == "" {
		return true
	}
	return subtle.ConstantTimeCompare([]byte(r.Header.Get("X-Tailsend-Token")), []byte(token)) == 1
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func orUnknown(s string) string {
	if s == "" {
		return "(unknown)"
	}
	return s
}

func defaultSaveDir() string {
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		if _, err := os.Stat(filepath.Join(home, "Downloads")); err == nil {
			return filepath.Join(home, "Downloads", "Tailsend")
		}
	}
	return "tailsend-inbox"
}
