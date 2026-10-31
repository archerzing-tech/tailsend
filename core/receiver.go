package core

import (
	"bytes"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log"
	"mime"
	"mime/multipart"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

var errTooBig = errors.New("file exceeds the -max limit")

// Peer is one tailnet node as reported by GET /api/peers.
type Peer struct {
	Name   string `json:"name"`
	IP     string `json:"ip"`
	OS     string `json:"os"`
	Online bool   `json:"online"`
}

// PeersInfo is what a Receiver's Peers function supplies: this machine's
// address plus its view of the tailnet.
type PeersInfo struct {
	SelfIP string
	Peers  []Peer
}

// Receiver serves the tailsend receive protocol on any listener. The CLI
// binds the machine's Tailscale IP; the mobile node hands in a tsnet
// listener — the engine is the same.
type Receiver struct {
	Dir     string // directory incoming files are saved into
	Token   string // shared secret senders must present ("" = none)
	Max     int64  // reject single files larger than this (0 = unlimited)
	Host    string // this machine's display name (ping responses, save logs)
	Version string // reported by /api/ping
	Gate    *ConsentGate
	// Peers fills GET /api/peers; nil leaves it answering ok:false (CLI-less
	// callers discover the tailnet some other way).
	Peers func() (PeersInfo, error)
	// Logf receives save events ("file ← dest (size) from sender"); nil falls
	// back to the log package.
	Logf func(format string, a ...any)
	// AdvertiseIP is the address suggested on the "/" landing page.
	AdvertiseIP string

	srv *http.Server
	up  *uploadStore
}

func (rcv *Receiver) logf(format string, a ...any) {
	if rcv.Logf != nil {
		rcv.Logf(format, a...)
		return
	}
	log.Printf(format, a...)
}

func (rcv *Receiver) store() *uploadStore {
	if rcv.up == nil {
		rcv.up = &uploadStore{
			uploads: map[string]*chunkUpload{},
			dir:     rcv.Dir,
			token:   rcv.Token,
			max:     rcv.Max,
			gate:    rcv.Gate,
			logf:    rcv.logf,
		}
	}
	return rcv.up
}

// Handler returns the receiver's HTTP routes.
func (rcv *Receiver) Handler() http.Handler {
	up := rcv.store()
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		fmt.Fprintf(w, "Tailsend receiver %s is running on %s.\nSend files with: tailsend send %s <file-or-directory>\n",
			rcv.Version, rcv.Host, rcv.AdvertiseIP)
	})
	mux.HandleFunc("/api/ping", func(w http.ResponseWriter, r *http.Request) {
		if !authed(r, rcv.Token) {
			writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
			return
		}
		// The caller's address as seen from here; mobile clients use it to
		// learn their own tailnet IP.
		peerIP := r.RemoteAddr
		if h, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
			peerIP = h
		}
		ask := rcv.Gate != nil && rcv.Gate.Ask
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "name": rcv.Host, "version": rcv.Version, "dir": rcv.Dir, "your_ip": peerIP, "confirm": ask})
	})
	// handleRequest dereferences the gate, so never register a nil one.
	gate := rcv.Gate
	if gate == nil {
		gate = NewConsentGate(false, rcv.Token)
	}
	mux.HandleFunc("/api/request", gate.handleRequest)
	mux.HandleFunc("/api/peers", func(w http.ResponseWriter, r *http.Request) {
		if !authed(r, rcv.Token) {
			writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
			return
		}
		// Peer-list exchange: CLI-less devices (phones) discover the tailnet
		// by asking any reachable receiver for this machine's view of it.
		if rcv.Peers == nil {
			writeJSON(w, http.StatusOK, map[string]any{"ok": false, "error": "peer list not available on this receiver", "peers": []any{}})
			return
		}
		info, err := rcv.Peers()
		if err != nil {
			writeJSON(w, http.StatusOK, map[string]any{"ok": false, "error": err.Error(), "peers": []any{}})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{
			"ok": true, "name": rcv.Host, "version": rcv.Version,
			"self_ip": info.SelfIP, "peers": info.Peers,
		})
	})
	mux.HandleFunc("/api/upload", up.handleUpload)
	// Chunked upload (v2): begin/chunk/complete/abort, for senders that
	// parallelize and retry pieces of a file. v1 above stays for old senders.
	mux.HandleFunc("/api/upload/begin", up.handleBegin)
	mux.HandleFunc("/api/upload/chunk", up.handleChunk)
	mux.HandleFunc("/api/upload/complete", up.handleComplete)
	mux.HandleFunc("/api/upload/abort", up.handleAbort)
	return mux
}

// Serve accepts connections on ln until Stop. It sweeps leftover spool files
// from crashed runs (safe here: the registry is still empty) and starts the
// janitor that reaps uploads whose sender went quiet.
func (rcv *Receiver) Serve(ln net.Listener) error {
	sweepSpoolFiles(rcv.Dir)
	up := rcv.store()
	go janitor(up, rcv.Gate)
	rcv.srv = &http.Server{Handler: rcv.Handler(), ReadHeaderTimeout: 15 * time.Second}
	err := rcv.srv.Serve(ln)
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

// Stop shuts the receiver down; Serve returns nil afterwards.
func (rcv *Receiver) Stop() {
	if rcv.srv != nil {
		rcv.srv.Close()
	}
}

// ---- v1 multipart upload ----

func (s *uploadStore) handleUpload(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, s.token) {
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
			// Consent (v3): check before any byte is read from the part.
			// v1 carries no size, so the grant is matched on name alone.
			if !s.gate.Allows(r.Header.Get("X-Tailsend-Request"), rel, 0, false) {
				writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": consentDeniedMsg})
				part.Close()
				return
			}
			dest, err := safeJoin(s.dir, rel)
			if err != nil {
				writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			dest = UniquePath(dest)
			if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
				writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			n, err := saveStream(part, dest, s.max)
			if err != nil {
				code := http.StatusInternalServerError
				if errors.Is(err, errTooBig) {
					code = http.StatusRequestEntityTooLarge
				}
				writeJSON(w, code, map[string]any{"ok": false, "error": err.Error()})
				part.Close()
				return
			}
			relDest, _ := filepath.Rel(s.dir, dest)
			files++
			savedRel, savedSize = relDest, n
			s.logf("%s ← %s (%s) from %s", time.Now().Format("15:04:05"), relDest, Human(n), OrUnknown(sender))
		}
		part.Close()
	}
	if files == 0 {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "no file part in request"})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "saved": savedRel, "size": savedSize})
}

// ---- v2 chunked upload ----

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
	last     time.Time // last chunk arrival; the janitor reaps idle uploads
}

// uploadStore holds every in-flight v2 upload keyed by its opaque id.
type uploadStore struct {
	mu      sync.Mutex
	uploads map[string]*chunkUpload
	dir     string
	token   string
	max     int64
	gate    *ConsentGate
	logf    func(format string, a ...any)
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

// chunkCount is how many chunks a file of size bytes splits into. Written
// overflow-free so an absurd declared size can't wrap `total` negative and
// make complete succeed with zero chunks.
func chunkCount(size, chunk int64) int64 {
	if size <= 0 || chunk <= 0 {
		return 0
	}
	return (size-1)/chunk + 1
}

// maxDeclaredSize is a sanity cap on the size a sender may declare at begin —
// huge declared sizes create sparse spool files that only ever become garbage.
const maxDeclaredSize = 1 << 50 // 1 PiB

// handleBegin validates a v2 upload request, opens its spool file and hands
// back the id the sender quotes on every later call.
func (s *uploadStore) handleBegin(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, s.token) {
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
	if s.max > 0 && req.Size > s.max {
		writeJSON(w, http.StatusRequestEntityTooLarge, map[string]any{"ok": false, "error": fmt.Sprintf("%s (%s)", errTooBig, Human(s.max))})
		return
	}
	if req.Size > maxDeclaredSize {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "declared size is unreasonably large"})
		return
	}
	// Consent (v3): the header must name a grant that covers name+size.
	if !s.gate.Allows(r.Header.Get("X-Tailsend-Request"), req.Name, req.Size, true) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": consentDeniedMsg})
		return
	}
	dest, err := safeJoin(s.dir, req.Name) // same cleaning as the v1 upload
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	// The id doubles as the spool suffix, so every upload gets its own file.
	id := RandomHex(8)
	up := &chunkUpload{
		dest:     dest,
		tmpPath:  dest + ".tspart-" + id,
		size:     req.Size,
		chunk:    DefaultChunkSize,
		sender:   strings.TrimSpace(req.Sender),
		received: map[int64]struct{}{},
		last:     time.Now(),
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
func (s *uploadStore) handleChunk(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, s.token) {
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
	// Read the whole chunk into memory (bounded by MaxBytesReader) before
	// taking the upload's lock: a sender trickling bytes over the wire must
	// not pin the lock and freeze complete/abort/other chunks. It also means
	// an over-long body is rejected without any of it reaching the spool.
	body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, up.chunk+1<<20))
	if err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad chunk body: " + err.Error()})
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
	if int64(len(body)) != room {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": fmt.Sprintf("chunk %d carried %s, expected %s", idx, Human(int64(len(body))), Human(room))})
		return
	}
	if _, err := io.Copy(io.NewOffsetWriter(up.f, off), bytes.NewReader(body)); err != nil {
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	up.received[idx] = struct{}{}
	up.last = time.Now()
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "received": len(up.received)})
}

// handleComplete finishes a v2 upload: fsync, close, rename into place under
// the same collision rule as v1. Incomplete uploads stay registered so the
// sender can retry the missing chunks or abort.
func (s *uploadStore) handleComplete(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, s.token) {
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
		writeJSON(w, http.StatusConflict, map[string]any{"ok": false, "error": fmt.Sprintf("missing %d %s", missing, Plural("chunk", int(missing)))})
		return
	}
	// The spool must be at least the declared size; trim anything past it so
	// the final file is exactly what the sender promised, never more.
	st, err := up.f.Stat()
	if err != nil {
		up.f.Close()
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	if st.Size() < up.size {
		up.f.Close()
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": fmt.Sprintf("spool is %s, expected %s — upload corrupt", Human(st.Size()), Human(up.size))})
		return
	}
	if err := up.f.Truncate(up.size); err != nil {
		up.f.Close()
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
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
	dest := UniquePath(up.dest)
	if err := os.Rename(up.tmpPath, dest); err != nil {
		os.Remove(up.tmpPath)
		writeJSON(w, http.StatusInternalServerError, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	relDest, _ := filepath.Rel(s.dir, dest)
	s.logf("%s ← %s (%s) from %s", time.Now().Format("15:04:05"), relDest, Human(up.size), OrUnknown(up.sender))
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "path": relDest})
}

// handleAbort discards a v2 upload the sender gave up on.
func (s *uploadStore) handleAbort(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, s.token) {
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
	tmp := dest + ".part-" + RandomHex(6)
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
			return n, fmt.Errorf("%w (%s)", errTooBig, Human(max))
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
	// Re-check collisions right before the rename: the check at stream start
	// is stale by upload's end, and two same-name transfers would otherwise
	// silently overwrite each other.
	if err := os.Rename(tmp, UniquePath(dest)); err != nil {
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
			// A Windows drive prefix ("C:/...") makes filepath resolution jump
			// out of the save directory; refuse it everywhere for consistency.
			if isDrivePrefix(seg) {
				return "", fmt.Errorf("refusing suspicious path %q", rel)
			}
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

func isDrivePrefix(seg string) bool {
	return len(seg) >= 2 && seg[1] == ':' &&
		((seg[0] >= 'a' && seg[0] <= 'z') || (seg[0] >= 'A' && seg[0] <= 'Z'))
}

// sweepSpoolFiles deletes .tspart-*/.part-* leftovers from crashed transfers.
// Only safe while the receiver is not serving: the upload registry is empty,
// so no live upload can own one of these files.
func sweepSpoolFiles(dir string) {
	filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return nil // best effort; unreadable entries are skipped
		}
		if isTempName(d.Name()) {
			os.Remove(path)
		}
		return nil
	})
}

// isTempName recognizes the transient names both implementations write:
// v2 spools (.tspart-…), the Go v1 partial (.part-<12 hex>) and the Rust
// v1 partial (.part-tailsend-<seq>). Ordinary "file.part-1" split archives
// don't match any of these.
func isTempName(name string) bool {
	if i := strings.LastIndex(name, ".tspart-"); i >= 0 {
		s := name[i+len(".tspart-"):]
		if isHexLen(s, 16) || isRustUploadID(s) {
			return true
		}
	}
	if i := strings.LastIndex(name, ".part-"); i >= 0 {
		s := name[i+len(".part-"):]
		if isHexLen(s, 12) || strings.HasPrefix(s, "tailsend-") {
			return true
		}
	}
	return false
}

func isHexLen(s string, n int) bool {
	if len(s) != n {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
			return false
		}
	}
	return true
}

// isRustUploadID matches the Rust receiver's ids, "u<pid>-<seq>".
func isRustUploadID(s string) bool {
	a, b, ok := strings.Cut(s, "-")
	return ok && len(a) > 1 && a[0] == 'u' && allDigits(a[1:]) && allDigits(b)
}

func allDigits(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

// uploadIdle is how long an upload may sit without a chunk before the janitor
// reaps it: the sender is assumed gone.
const uploadIdle = time.Hour

// janitor reaps uploads whose sender went quiet — without it a crashed sender
// would leave its spool file and registry entry behind forever.
func janitor(s *uploadStore, gate *ConsentGate) {
	t := time.NewTicker(10 * time.Minute)
	for range t.C {
		gate.Sweep()
		for _, up := range s.stale() {
			up.mu.Lock()
			up.f.Close()
			os.Remove(up.tmpPath)
			up.mu.Unlock()
		}
	}
}

// stale unregisters and returns the uploads idle for longer than uploadIdle.
func (s *uploadStore) stale() []*chunkUpload {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []*chunkUpload
	for id, up := range s.uploads {
		if time.Since(up.last) > uploadIdle {
			delete(s.uploads, id)
			out = append(out, up)
		}
	}
	return out
}
