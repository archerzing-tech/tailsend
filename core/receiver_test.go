package core

import (
	"bytes"
	"encoding/json"
	"mime/multipart"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"
)

// newTestReceiver builds a quiet receiver (no save logs) over dir and returns
// its mux; tests drive real routes through it.
func newTestReceiver(dir string, g *ConsentGate) (*Receiver, http.Handler) {
	rcv := &Receiver{Dir: dir, Host: "t", Version: "test", Gate: g, Logf: func(string, ...any) {}}
	return rcv, rcv.Handler()
}

// do runs a request through h with a fresh recorder.
func do(t *testing.T, h http.Handler, method, url string, headers map[string]string, body []byte) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, url, bytes.NewReader(body))
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	w := httptest.NewRecorder()
	h.ServeHTTP(w, req)
	return w
}

// beginUpload starts a v2 upload of size bytes and returns its id.
func beginUpload(t *testing.T, h http.Handler, name string, size int64) string {
	t.Helper()
	payload, _ := json.Marshal(map[string]any{"name": name, "size": size, "sender": "test"})
	w := do(t, h, http.MethodPost, "/api/upload/begin", nil, payload)
	if w.Code != http.StatusOK {
		t.Fatalf("begin %q: got HTTP %d: %s", name, w.Code, w.Body.String())
	}
	var resp struct {
		ID string `json:"id"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil || resp.ID == "" {
		t.Fatalf("begin %q: bad response %q", name, w.Body.String())
	}
	return resp.ID
}

func sendChunkReq(t *testing.T, h http.Handler, id string, idx int64, body []byte) *httptest.ResponseRecorder {
	t.Helper()
	return do(t, h, http.MethodPost, "/api/upload/chunk", map[string]string{
		"X-Upload-Id":   id,
		"X-Chunk-Index": strconv.FormatInt(idx, 10),
	}, body)
}

func completeReq(t *testing.T, h http.Handler, id string) *httptest.ResponseRecorder {
	t.Helper()
	return do(t, h, http.MethodPost, "/api/upload/complete", nil, []byte(`{"id":"`+id+`"}`))
}

func TestChunkCountNeverWraps(t *testing.T) {
	if got := chunkCount(1<<62, DefaultChunkSize); got <= 0 {
		t.Fatalf("huge size wrapped: %d", got)
	}
	if got := chunkCount(0, DefaultChunkSize); got != 0 {
		t.Fatalf("empty file should need 0 chunks, got %d", got)
	}
	if got := chunkCount(DefaultChunkSize+1, DefaultChunkSize); got != 2 {
		t.Fatalf("want 2 chunks, got %d", got)
	}
}

func TestSafeJoinRefusesTraversalAndDrives(t *testing.T) {
	base := t.TempDir()
	for _, bad := range []string{"../evil", "a/../../evil", "C:/Users/x/evil", "c:\\evil"} {
		if _, err := safeJoin(base, bad); err == nil {
			t.Fatalf("safeJoin(%q) accepted", bad)
		}
	}
	got, err := safeJoin(base, "sub/dir/file.txt")
	if err != nil || got != filepath.Join(base, "sub", "dir", "file.txt") {
		t.Fatalf("safeJoin clean path: %v %q", err, got)
	}
}

func TestUniquePathSuffixes(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "a.txt")
	os.WriteFile(p, []byte("x"), 0o644)
	if got := UniquePath(p); got != filepath.Join(dir, "a (1).txt") {
		t.Fatalf("uniquePath = %q", got)
	}
}

func TestV2UploadLifecycleAndOvershoot(t *testing.T) {
	dir := t.TempDir()
	_, h := newTestReceiver(dir, nil)
	const size = DefaultChunkSize + 10 // two chunks
	id := beginUpload(t, h, "life.bin", size)

	// Chunk 0: correct length → 200.
	if w := sendChunkReq(t, h, id, 0, bytes.Repeat([]byte{7}, DefaultChunkSize)); w.Code != http.StatusOK {
		t.Fatalf("chunk 0: HTTP %d: %s", w.Code, w.Body.String())
	}
	// Chunk 0 again but over-long → 400, and nothing may land in the spool.
	if w := sendChunkReq(t, h, id, 0, bytes.Repeat([]byte{7}, DefaultChunkSize+1<<20)); w.Code != http.StatusBadRequest {
		t.Fatalf("over-long chunk: HTTP %d", w.Code)
	}
	// Short chunk → 400 (a hole would survive as silent zeros).
	if w := sendChunkReq(t, h, id, 0, bytes.Repeat([]byte{7}, 4)); w.Code != http.StatusBadRequest {
		t.Fatalf("short chunk: HTTP %d", w.Code)
	}
	// Out-of-range index → 400.
	if w := sendChunkReq(t, h, id, 99, []byte{1}); w.Code != http.StatusBadRequest {
		t.Fatalf("out-of-range chunk: HTTP %d", w.Code)
	}
	// Complete with a missing tail chunk → 409 and the upload stays usable.
	if w := completeReq(t, h, id); w.Code != http.StatusConflict {
		t.Fatalf("incomplete complete: HTTP %d", w.Code)
	}
	// The tail chunk lands, then complete succeeds.
	tail := bytes.Repeat([]byte{9}, 10)
	if w := sendChunkReq(t, h, id, 1, tail); w.Code != http.StatusOK {
		t.Fatalf("chunk 1: HTTP %d: %s", w.Code, w.Body.String())
	}
	w := completeReq(t, h, id)
	if w.Code != http.StatusOK {
		t.Fatalf("complete: HTTP %d: %s", w.Code, w.Body.String())
	}
	got, err := os.ReadFile(filepath.Join(dir, "life.bin"))
	if err != nil {
		t.Fatalf("final file missing: %v", err)
	}
	if int64(len(got)) != size {
		t.Fatalf("final size = %d, want %d", len(got), size)
	}
	if !bytes.HasSuffix(got, tail) || got[0] != 7 {
		t.Fatalf("final content wrong: first=%d tail=%v", got[0], got[size-1:] == nil)
	}
}

func TestV2CompleteTrimsOversizedSpool(t *testing.T) {
	dir := t.TempDir()
	rcv, h := newTestReceiver(dir, nil)
	const size = 32
	id := beginUpload(t, h, "trim.bin", size)
	if w := sendChunkReq(t, h, id, 0, bytes.Repeat([]byte{5}, size)); w.Code != http.StatusOK {
		t.Fatalf("chunk: HTTP %d", w.Code)
	}
	// Simulate residue past the declared size (e.g. from an older sender).
	up := rcv.store().get(id)
	up.mu.Lock()
	f, _ := os.OpenFile(up.tmpPath, os.O_WRONLY|os.O_APPEND, 0o644)
	f.Write(bytes.Repeat([]byte{0xEE}, 512))
	f.Close()
	up.mu.Unlock()

	if w := completeReq(t, h, id); w.Code != http.StatusOK {
		t.Fatalf("complete: HTTP %d: %s", w.Code, w.Body.String())
	}
	st, err := os.Stat(filepath.Join(dir, "trim.bin"))
	if err != nil {
		t.Fatalf("final file missing: %v", err)
	}
	if st.Size() != size {
		t.Fatalf("final size = %d, want exactly %d (overshoot not trimmed)", st.Size(), size)
	}
}

func TestV2AbortAndUnknownIDs(t *testing.T) {
	dir := t.TempDir()
	_, h := newTestReceiver(dir, nil)
	id := beginUpload(t, h, "bye.bin", 8)
	sendChunkReq(t, h, id, 0, bytes.Repeat([]byte{1}, 8))

	if w := completeReq(t, h, "nope"); w.Code != http.StatusNotFound {
		t.Fatalf("unknown complete: HTTP %d", w.Code)
	}
	if w := do(t, h, http.MethodPost, "/api/upload/abort", nil, []byte(`{"id":"`+id+`"}`)); w.Code != http.StatusOK {
		t.Fatalf("abort: HTTP %d", w.Code)
	}
	if w := completeReq(t, h, id); w.Code != http.StatusNotFound {
		t.Fatalf("complete after abort: HTTP %d", w.Code)
	}
	if _, err := os.Stat(filepath.Join(dir, "bye.bin")); !os.IsNotExist(err) {
		t.Fatalf("aborted upload leaked a file")
	}
}

func TestBeginRejectsAbsurdDeclaredSize(t *testing.T) {
	dir := t.TempDir()
	_, h := newTestReceiver(dir, nil)
	payload, _ := json.Marshal(map[string]any{"name": "big.bin", "size": 1 << 60, "sender": "test"})
	if w := do(t, h, http.MethodPost, "/api/upload/begin", nil, payload); w.Code != http.StatusBadRequest {
		t.Fatalf("1 EiB declaration: HTTP %d", w.Code)
	}
}

func TestJanitorReapsIdleUploads(t *testing.T) {
	dir := t.TempDir()
	rcv, h := newTestReceiver(dir, nil)
	id := beginUpload(t, h, "stale.bin", 8)
	up := rcv.store().get(id)
	up.mu.Lock()
	up.last = time.Now().Add(-2 * time.Hour)
	up.mu.Unlock()

	got := rcv.store().stale()
	if len(got) != 1 || rcv.store().get(id) != nil {
		t.Fatalf("stale() = %d entries, registry still has it: %v", len(got), rcv.store().get(id) != nil)
	}
}

func TestIsTempName(t *testing.T) {
	yes := []string{
		"a.bin.tspart-0123456789abcdef", // Go v2 spool
		"a.bin.tspart-u4242-7",          // Rust v2 spool
		"movie.part-0123456789ab",       // Go v1 partial (RandomHex(6) = 12 hex)
		"song.mp3.part-tailsend-12",     // Rust v1 partial
	}
	no := []string{
		"archive.part-1", // user's split archive
		"archive.part-22",
		"my.tspart-notes.txt",
		"regular.txt",
	}
	for _, n := range yes {
		if !isTempName(n) {
			t.Errorf("isTempName(%q) = false, want true", n)
		}
	}
	for _, n := range no {
		if isTempName(n) {
			t.Errorf("isTempName(%q) = true, want false", n)
		}
	}
}

// ---- consent (protocol v3) ----

func sendRequest(t *testing.T, h http.Handler, body string) *httptest.ResponseRecorder {
	t.Helper()
	return do(t, h, http.MethodPost, "/api/request", nil, []byte(body))
}

func beginGated(t *testing.T, h http.Handler, name string, size int64, reqID string) *httptest.ResponseRecorder {
	t.Helper()
	payload, _ := json.Marshal(map[string]any{"name": name, "size": size, "sender": "t"})
	var hdr map[string]string
	if reqID != "" {
		hdr = map[string]string{"X-Tailsend-Request": reqID}
	}
	return do(t, h, http.MethodPost, "/api/upload/begin", hdr, payload)
}

func TestRequestAutoGrantWhenNotAsking(t *testing.T) {
	dir := t.TempDir()
	g := NewConsentGate(false, "")
	_, h := newTestReceiver(dir, g)
	w := sendRequest(t, h, `{"sender":"mac","count":1,"total":3,"files":[{"name":"a.bin","size":3}]}`)
	if w.Code != http.StatusOK {
		t.Fatalf("request: HTTP %d: %s", w.Code, w.Body.String())
	}
	var rr struct{ ID string }
	json.Unmarshal(w.Body.Bytes(), &rr)
	if rr.ID == "" {
		t.Fatalf("no grant id in %s", w.Body.String())
	}

	// With ask off, begin works with or without the grant header — the
	// compatibility path for senders that predate consent.
	sdir := t.TempDir()
	_, h1 := newTestReceiver(sdir, g)
	if w := beginGated(t, h1, "a.bin", 3, ""); w.Code != http.StatusOK {
		t.Fatalf("begin without header: HTTP %d: %s", w.Code, w.Body.String())
	}
	_, h2 := newTestReceiver(sdir, g)
	if w := beginGated(t, h2, "a.bin", 3, rr.ID); w.Code != http.StatusOK {
		t.Fatalf("begin with header: HTTP %d: %s", w.Code, w.Body.String())
	}
}

func TestAskRequiresGrantForEveryUpload(t *testing.T) {
	dir := t.TempDir()
	g := NewConsentGate(true, "")
	_, h := newTestReceiver(dir, g)
	// Asking with nobody to answer → refused, reason "unattended".
	w := sendRequest(t, h, `{"sender":"mac","count":1,"total":3,"files":[{"name":"a.bin","size":3}]}`)
	if w.Code != http.StatusForbidden {
		t.Fatalf("unattended request: HTTP %d: %s", w.Code, w.Body.String())
	}

	// No grant anywhere → begin and v1 must refuse before bytes flow.
	if w := beginGated(t, h, "a.bin", 3, ""); w.Code != http.StatusForbidden {
		t.Fatalf("begin without grant: HTTP %d", w.Code)
	}

	// An accepted request authorizes exactly its files.
	g.AddGrant("grant1", map[string]int64{"a.bin": 3})
	if w := beginGated(t, h, "a.bin", 3, "grant1"); w.Code != http.StatusOK {
		t.Fatalf("begin with grant: HTTP %d: %s", w.Code, w.Body.String())
	}
	// Wrong id → refused.
	if w := beginGated(t, h, "a.bin", 3, "other"); w.Code != http.StatusForbidden {
		t.Fatalf("begin with wrong id: HTTP %d", w.Code)
	}
	// Right id, wrong size → refused.
	if w := beginGated(t, h, "a.bin", 4, "grant1"); w.Code != http.StatusForbidden {
		t.Fatalf("begin with wrong size: HTTP %d", w.Code)
	}
	// Right id, name outside the grant → refused.
	if w := beginGated(t, h, "b.bin", 3, "grant1"); w.Code != http.StatusForbidden {
		t.Fatalf("begin with unlisted name: HTTP %d", w.Code)
	}
	// Expired grant → refused.
	g.mu.Lock()
	g.grants["grant1"].expires = time.Now().Add(-time.Second)
	g.mu.Unlock()
	if w := beginGated(t, h, "a.bin", 3, "grant1"); w.Code != http.StatusForbidden {
		t.Fatalf("begin with expired grant: HTTP %d", w.Code)
	}
	g.Sweep()
	g.mu.Lock()
	_, alive := g.grants["grant1"]
	g.mu.Unlock()
	if alive {
		t.Fatalf("sweep kept an expired grant")
	}
}

func TestV1UploadRefusedWithoutGrant(t *testing.T) {
	dir := t.TempDir()
	g := NewConsentGate(true, "")
	_, h := newTestReceiver(dir, g)
	send := func(reqID string) *httptest.ResponseRecorder {
		t.Helper()
		body := &bytes.Buffer{}
		mw := multipart.NewWriter(body)
		mw.WriteField("sender", "mac")
		fw, _ := mw.CreateFormFile("file", url.PathEscape("v1.bin"))
		fw.Write([]byte("hello"))
		mw.Close()
		hdr := map[string]string{"Content-Type": mw.FormDataContentType()}
		if reqID != "" {
			hdr["X-Tailsend-Request"] = reqID
		}
		return do(t, h, http.MethodPost, "/api/upload", hdr, body.Bytes())
	}
	if w := send(""); w.Code != http.StatusForbidden {
		t.Fatalf("v1 without grant: HTTP %d: %s", w.Code, w.Body.String())
	}
	if _, err := os.Stat(filepath.Join(dir, "v1.bin")); !os.IsNotExist(err) {
		t.Fatalf("v1 upload landed despite refusal")
	}
	g.AddGrant("g", map[string]int64{"v1.bin": 5})
	if w := send("g"); w.Code != http.StatusOK {
		t.Fatalf("v1 with grant: HTTP %d: %s", w.Code, w.Body.String())
	}
}

func TestRequestDenyAcceptTimeout(t *testing.T) {
	old := ConfirmWait
	ConfirmWait = 80 * time.Millisecond
	defer func() { ConfirmWait = old }()

	// Explicit denial → 403 "denied".
	dir := t.TempDir()
	deny := NewConsentGate(true, "")
	deny.Prompt = func(sender string, count int, total int64, names []string) (bool, string) {
		return false, "denied"
	}
	_, h := newTestReceiver(dir, deny)
	if w := sendRequest(t, h, `{"sender":"mac","count":1,"total":3,"files":[{"name":"a.bin","size":3}]}`); w.Code != http.StatusForbidden {
		t.Fatalf("denied: HTTP %d", w.Code)
	}

	// A prompt that stalls past ConfirmWait → 408 "timeout", and the handler
	// answers before the prompt returns.
	release := make(chan struct{})
	slow := NewConsentGate(true, "")
	slow.Prompt = func(sender string, count int, total int64, names []string) (bool, string) {
		<-release
		return false, "denied"
	}
	_, hslow := newTestReceiver(dir, slow)
	start := time.Now()
	w := sendRequest(t, hslow, `{"sender":"mac","count":1,"total":3,"files":[{"name":"a.bin","size":3}]}`)
	if el := time.Since(start); el > 300*time.Millisecond {
		t.Fatalf("timeout path waited %v for the prompt", el)
	}
	if w.Code != http.StatusRequestTimeout {
		t.Fatalf("timeout: HTTP %d: %s", w.Code, w.Body.String())
	}
	close(release)

	// Acceptance returns a grant the upload can use right away.
	yes := NewConsentGate(true, "")
	yes.Prompt = func(sender string, count int, total int64, names []string) (bool, string) {
		if sender != "mac" || count != 1 || total != 3 || len(names) != 1 || names[0] != "a.bin" {
			t.Fatalf("prompt got sender=%q count=%d total=%d names=%v", sender, count, total, names)
		}
		return true, ""
	}
	_, hyes := newTestReceiver(dir, yes)
	w = sendRequest(t, hyes, `{"sender":"mac","count":1,"total":3,"files":[{"name":"a.bin","size":3}]}`)
	if w.Code != http.StatusOK {
		t.Fatalf("accept: HTTP %d: %s", w.Code, w.Body.String())
	}
	var rr struct{ ID string }
	json.Unmarshal(w.Body.Bytes(), &rr)
	if w := beginGated(t, hyes, "a.bin", 3, rr.ID); w.Code != http.StatusOK {
		t.Fatalf("begin after accept: HTTP %d: %s", w.Code, w.Body.String())
	}
}
