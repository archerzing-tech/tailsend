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
	"sync"
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

	// Chunked upload (v2): begin/chunk/complete/abort, for senders that
	// parallelize and retry pieces of a file. v1 above stays for old senders.
	uploads := &uploadStore{uploads: map[string]*chunkUpload{}}
	mux.HandleFunc("/api/upload/begin", func(w http.ResponseWriter, r *http.Request) {
		uploads.handleBegin(w, r, saveDir, *token, *max)
	})
	mux.HandleFunc("/api/upload/chunk", func(w http.ResponseWriter, r *http.Request) {
		uploads.handleChunk(w, r, *token)
	})
	mux.HandleFunc("/api/upload/complete", func(w http.ResponseWriter, r *http.Request) {
		uploads.handleComplete(w, r, saveDir, *token)
	})
	mux.HandleFunc("/api/upload/abort", func(w http.ResponseWriter, r *http.Request) {
		uploads.handleAbort(w, r, *token)
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

// chunkUpload is one in-flight v2 upload: a spool file next to the final
// destination plus the set of chunk offsets that have arrived so far.
type chunkUpload struct {
	dest     string // absolute path the file takes when complete
	tmpPath  string // spool file, same directory as dest so renames stay atomic
	size     int64
	chunk    int64
	sender   string
	mu       sync.Mutex // serializes writes to one upload; uploads don't share it
	f        *os.File
	received map[int64]struct{}
}

// uploadStore holds every in-flight v2 upload keyed by its opaque id.
type uploadStore struct {
	mu      sync.Mutex
	uploads map[string]*chunkUpload
}

func (s *uploadStore) get(id string) *chunkUpload {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.uploads[id]
}

func (s *uploadStore) put(id string, up *chunkUpload) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, dup := s.uploads[id]; dup {
		return false
	}
	s.uploads[id] = up
	return true
}

// drop unregisters the upload and returns it, or nil if the id is unknown.
func (s *uploadStore) drop(id string) *chunkUpload {
	s.mu.Lock()
	defer s.mu.Unlock()
	up := s.uploads[id]
	delete(s.uploads, id)
	return up
}

// chunkCount is how many chunks a file of size bytes splits into.
func chunkCount(size, chunk int64) int64 {
	return (size + chunk - 1) / chunk
}

// handleBegin validates a v2 upload request, opens its spool file and hands
// back the id the sender quotes on every later call.
func (s *uploadStore) handleBegin(w http.ResponseWriter, r *http.Request, saveDir, token string, max int64) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	var req struct {
		Name   string `json:"name"`
		Size   int64  `json:"size"`
		Sender string `json:"sender"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad JSON body"})
		return
	}
	if req.Size < 0 {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "size must not be negative"})
		return
	}
	if max > 0 && req.Size > max {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]any{"ok": false, "error": fmt.Sprintf("%s (%s)", errTooBig, human(max))})
		return
	}
	dest, err := safeJoin(saveDir, req.Name) // same cleaning as the v1 upload
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	// The id doubles as the spool suffix, so every upload gets its own file.
	id := randomHex(8)
	up := &chunkUpload{
		dest:     dest,
		tmpPath:  dest + ".tspart-" + id,
		size:     req.Size,
		chunk:    defaultChunkSize,
		sender:   strings.TrimSpace(req.Sender),
		received: map[int64]struct{}{},
	}
	f, err := os.OpenFile(up.tmpPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
	if err != nil {
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	up.f = f
	if !s.put(id, up) {
		f.Close()
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": "could not allocate an upload id"})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "id": id, "chunk": up.chunk})
}

// handleChunk stores one chunk at its offset. Chunks arrive in parallel, so
// each upload serializes its own writes; different uploads never share a lock.
func (s *uploadStore) handleChunk(w http.ResponseWriter, r *http.Request, token string) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	id := r.Header.Get("X-Upload-Id")
	idx, err := strconv.ParseInt(r.Header.Get("X-Chunk-Index"), 10, 64)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad X-Chunk-Index header"})
		return
	}
	up := s.get(id)
	if up == nil {
		writeJSON(w, http.StatusNotFound, map[string]any{"ok": false, "error": "unknown upload id"})
		return
	}
	up.mu.Lock()
	defer up.mu.Unlock()
	total := chunkCount(up.size, up.chunk)
	if idx < 0 || idx >= total {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": fmt.Sprintf("chunk %d out of range (file has %d)", idx, total)})
		return
	}
	off := idx * up.chunk
	room := up.chunk
	if rem := up.size - off; rem < room {
		room = rem // short final chunk
	}
	// Anything over the chunk's room is rejected — and so is anything short of
	// it, which would leave a hole in the spool file.
	n, err := io.Copy(io.NewOffsetWriter(up.f, off), http.MaxBytesReader(w, r.Body, up.chunk+1<<20))
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad chunk body: " + err.Error()})
		return
	}
	if n != room {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": fmt.Sprintf("chunk %d carried %s, expected %s", idx, human(n), human(room))})
		return
	}
	up.received[idx] = struct{}{}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "received": len(up.received)})
}

// handleComplete finishes a v2 upload: fsync, close, rename into place under
// the same collision rule as v1. Incomplete uploads stay registered so the
// sender can retry the missing chunks or abort.
func (s *uploadStore) handleComplete(w http.ResponseWriter, r *http.Request, saveDir, token string) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	var req struct {
		ID string `json:"id"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad JSON body"})
		return
	}
	up := s.drop(req.ID)
	if up == nil {
		writeJSON(w, http.StatusNotFound, map[string]any{"ok": false, "error": "unknown upload id"})
		return
	}
	up.mu.Lock()
	defer up.mu.Unlock()
	if missing := chunkCount(up.size, up.chunk) - int64(len(up.received)); missing > 0 {
		s.put(req.ID, up) // keep it registered for retry
		writeJSON(w, http.StatusConflict, map[string]any{"ok": false, "error": fmt.Sprintf("missing %d %s", missing, plural("chunk", int(missing)))})
		return
	}
	if err := up.f.Sync(); err != nil {
		up.f.Close()
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	if err := up.f.Close(); err != nil {
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	dest := uniquePath(up.dest)
	if err := os.Rename(up.tmpPath, dest); err != nil {
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	relDest, _ := filepath.Rel(saveDir, dest)
	fmt.Printf("%s ← %s (%s) from %s\n", time.Now().Format("15:04:05"), relDest, human(up.size), orUnknown(up.sender))
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "path": relDest})
}

// handleAbort discards a v2 upload the sender gave up on.
func (s *uploadStore) handleAbort(w http.ResponseWriter, r *http.Request, token string) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	var req struct {
		ID string `json:"id"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&req); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad JSON body"})
		return
	}
	up := s.drop(req.ID)
	if up == nil {
		writeJSON(w, http.StatusNotFound, map[string]any{"ok": false, "error": "unknown upload id"})
		return
	}
	up.mu.Lock()
	up.f.Close()
	os.Remove(up.tmpPath)
	up.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

// saveStream writes to a unique dest.part-<random> file and renames on
// success, so a dropped connection never leaves a half-written file at the
// final name — and two simultaneous receives of the same name can't end up
// sharing (and corrupting) one temp file.
func saveStream(r io.Reader, dest string, max int64) (int64, error) {
	tmp := dest + ".part-" + randomHex(6)
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
