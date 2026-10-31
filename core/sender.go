package core

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
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
	"sync"
	"time"
)

// PingResult is what PingReceiver reports about a receiver.
type PingResult struct {
	OK      bool   `json:"ok"`
	Name    string `json:"name"`
	Version string `json:"version"`
	Dir     string `json:"dir"`
	Confirm bool   `json:"confirm"` // receiver requires per-transfer approval (v3)
	Error   string `json:"error"`
}

type uploadResult struct {
	OK    bool   `json:"ok"`
	Saved string `json:"saved"`
	Size  int64  `json:"size"`
	Error string `json:"error"`
}

type beginResult struct {
	OK    bool   `json:"ok"`
	ID    string `json:"id"`
	Chunk int64  `json:"chunk"`
	Error string `json:"error"`
}

type chunkResult struct {
	OK       bool   `json:"ok"`
	Received int64  `json:"received"`
	Error    string `json:"error"`
}

type completeResult struct {
	OK    bool   `json:"ok"`
	Path  string `json:"path"`
	Error string `json:"error"`
}

type idRequest struct {
	ID string `json:"id"`
}

// chunkSpan is one piece of a file: chunk idx covering [off, off+n).
type chunkSpan struct {
	idx int64
	off int64
	n   int64
}

// Chunked upload (v2) tuning.
const (
	chunkConcurrent = 4 // parallel chunk uploads per file
	chunkAttempts   = 3 // tries per chunk before the file fails
)

// errOldReceiver tells UploadFile to retry the file over the v1 multipart
// path, for receivers that predate the chunked protocol.
var errOldReceiver = errors.New("receiver does not support chunked upload")

// httpClient bounds stalls without capping total transfer time: dial and
// response-header deadlines apply to every request, and chunk uploads add a
// per-attempt context timeout (4 MiB in 2 min ≈ a 35 KB/s floor). The bare
// http.DefaultClient would hang forever on a receiver that stalls mid-send.
// dialHook routes sender traffic through an embedded node's userspace
// WireGuard when set (mobile/tsnet); nil means the OS network stack. The
// receiver never needs it — its listener is injected directly.
var (
	dialMu sync.RWMutex
	dialFn func(ctx context.Context, network, addr string) (net.Conn, error)
)

// SetDialContext points every sender HTTP client at fn. Call once at node
// startup, before the first send; pass nil to restore the OS stack.
func SetDialContext(fn func(ctx context.Context, network, addr string) (net.Conn, error)) {
	dialMu.Lock()
	dialFn = fn
	dialMu.Unlock()
}

func dialContext(ctx context.Context, network, addr string) (net.Conn, error) {
	dialMu.RLock()
	fn := dialFn
	dialMu.RUnlock()
	if fn != nil {
		return fn(ctx, network, addr)
	}
	return (&net.Dialer{Timeout: 10 * time.Second, KeepAlive: 30 * time.Second}).DialContext(ctx, network, addr)
}

var httpClient = &http.Client{
	Transport: &http.Transport{
		Proxy:                 http.ProxyFromEnvironment,
		DialContext:           dialContext,
		ResponseHeaderTimeout: 30 * time.Second,
		IdleConnTimeout:       90 * time.Second,
		MaxIdleConnsPerHost:   chunkConcurrent,
	},
}

// chunkTimeout bounds one chunk upload attempt, connect to response.
const chunkTimeout = 2 * time.Minute

// Progress receives upload milestones: done bytes of total sent. It is
// rate-limited by the sender; nil disables progress reporting.
type Progress func(done, total int64)

// CollectFiles expands the given paths; directories are walked recursively
// and each file keeps its path relative to the directory's base name.
func CollectFiles(paths []string) ([]File, error) {
	var out []File
	seen := map[string]bool{}
	for _, p := range paths {
		info, err := os.Stat(p)
		if err != nil {
			return nil, err
		}
		if !info.IsDir() {
			if !seen[p] {
				seen[p] = true
				out = append(out, File{Src: p, Rel: filepath.Base(p), Size: info.Size()})
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
				out = append(out, File{Src: path, Rel: filepath.Join(prefix, rel), Size: fi.Size()})
			}
			return nil
		})
		if err != nil {
			return nil, err
		}
	}
	return out, nil
}

func PingReceiver(addr, token string) (*PingResult, error) {
	client := &http.Client{Timeout: 5 * time.Second, Transport: &http.Transport{DialContext: dialContext}}
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
	var pr PingResult
	if err := json.NewDecoder(resp.Body).Decode(&pr); err != nil {
		return nil, fmt.Errorf("http://%s answered but is not tailsend (HTTP %d)", addr, resp.StatusCode)
	}
	return &pr, nil
}

// RequestTransfer asks the receiver for permission to start sending
// (protocol v3). It returns the grant id to attach to every upload, or ""
// when the receiver predates consent (no such route) and sending starts
// as before.
func RequestTransfer(addr, token, sender string, files []File) (string, error) {
	list := make([]map[string]any, 0, len(files))
	var total int64
	for _, f := range files {
		total += f.Size
		list = append(list, map[string]any{"name": f.Rel, "size": f.Size})
	}
	payload := map[string]any{"sender": sender, "count": len(files), "total": total, "files": list}

	// The receiver may hold the request open up to ConfirmWait while a human
	// decides; give the round trip a little headroom.
	client := &http.Client{Timeout: ConfirmWait + 10*time.Second, Transport: &http.Transport{DialContext: dialContext}}
	resp, err := postJSONClient("http://"+addr+"/api/request", token, payload, client)
	if err != nil {
		return "", fmt.Errorf("could not ask the receiver for permission: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound || resp.StatusCode == http.StatusNotImplemented {
		return "", nil // old receiver: no consent, send as before
	}
	var rr struct {
		OK     bool   `json:"ok"`
		ID     string `json:"id"`
		Reason string `json:"reason"`
	}
	derr := json.NewDecoder(resp.Body).Decode(&rr)
	if resp.StatusCode == http.StatusOK && derr == nil && rr.OK {
		return rr.ID, nil
	}
	switch rr.Reason {
	case "timeout":
		return "", fmt.Errorf("receiver did not confirm in time — transfer cancelled")
	case "unattended":
		return "", fmt.Errorf("receiver wants to confirm transfers but nobody is there to answer")
	}
	return "", fmt.Errorf("receiver declined the transfer")
}

// UploadFile sends one file over the chunked v2 protocol, falling back to the
// v1 multipart upload when the receiver doesn't answer /api/upload/begin.
func UploadFile(addr, token, sender, reqID string, f File, progress Progress) error {
	err := uploadOneChunked(addr, token, sender, reqID, f, progress)
	if errors.Is(err, errOldReceiver) {
		return uploadOne(addr, token, sender, reqID, f, progress)
	}
	return err
}

// uploadOneChunked sends f as begin + parallel chunks + complete. The sender
// keeps at most chunkConcurrent chunks in flight and retries each one
// chunkAttempts times; a chunk that never lands fails the whole file.
func uploadOneChunked(addr, token, sender, reqID string, f File, progress Progress) error {
	src, err := os.Open(f.Src)
	if err != nil {
		return err
	}
	defer src.Close()

	// Consent (v3): uploads must name the grant from /api/request.
	var hdr map[string]string
	if reqID != "" {
		hdr = map[string]string{"X-Tailsend-Request": reqID}
	}
	resp, err := postJSONHeaders("http://"+addr+"/api/upload/begin", token,
		map[string]any{"name": f.Rel, "size": f.Size, "sender": sender}, hdr)
	if err != nil {
		return fmt.Errorf("upload failed: %v", err)
	}
	var br beginResult
	derr := json.NewDecoder(resp.Body).Decode(&br)
	resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound || resp.StatusCode == http.StatusNotImplemented {
		return errOldReceiver // an old receiver: no such route
	}
	if derr != nil || !br.OK || resp.StatusCode != http.StatusOK {
		if br.Error != "" {
			return fmt.Errorf("receiver said: %s", br.Error)
		}
		return fmt.Errorf("unexpected receiver response (HTTP %d)", resp.StatusCode)
	}
	chunk := br.Chunk
	if chunk <= 0 {
		chunk = DefaultChunkSize // server's answer always wins when it gives one
	}
	total := chunkCount(f.Size, chunk)

	// Bounded worker pool over the chunk spans; progress advances as whole
	// chunks complete.
	jobs := make(chan chunkSpan)
	var wg sync.WaitGroup
	var mu sync.Mutex
	var firstErr error
	done, next := int64(0), time.Time{}
	advance := func(n int64) {
		mu.Lock()
		done += n
		now := time.Now()
		call := progress != nil && now.After(next)
		if call {
			next = now.Add(200 * time.Millisecond)
		}
		sent := done
		mu.Unlock()
		if call {
			progress(sent, f.Size) // outside the lock: callbacks may be slow
		}
	}
	fail := func(err error) {
		mu.Lock()
		defer mu.Unlock()
		if firstErr == nil {
			firstErr = err
		}
	}
	for w := 0; w < chunkConcurrent; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := range jobs {
				mu.Lock()
				give := firstErr == nil
				mu.Unlock()
				if !give {
					continue // a chunk already failed; just drain the queue
				}
				if err := sendChunk(addr, token, br.ID, src, j); err != nil {
					fail(err)
					continue
				}
				advance(j.n)
			}
		}()
	}
	for i := int64(0); i < total; i++ {
		off := i * chunk
		n := chunk
		if rem := f.Size - off; rem < n {
			n = rem
		}
		jobs <- chunkSpan{idx: i, off: off, n: n}
	}
	close(jobs)
	wg.Wait()

	if firstErr != nil {
		abortUpload(addr, token, br.ID) // best effort; the receiver cleans up
		return firstErr
	}
	if progress != nil && f.Size > 0 {
		progress(f.Size, f.Size)
	}

	resp, err = postJSON("http://"+addr+"/api/upload/complete", token, idRequest{ID: br.ID})
	if err != nil {
		abortUpload(addr, token, br.ID)
		return fmt.Errorf("upload failed: %v", err)
	}
	var cr completeResult
	derr = json.NewDecoder(resp.Body).Decode(&cr)
	resp.Body.Close()
	if derr != nil || !cr.OK || resp.StatusCode != http.StatusOK {
		abortUpload(addr, token, br.ID)
		if cr.Error != "" {
			return fmt.Errorf("receiver said: %s", cr.Error)
		}
		return fmt.Errorf("unexpected receiver response (HTTP %d)", resp.StatusCode)
	}
	return nil
}

// sendChunk uploads one byte range straight from the file, retrying
// chunkAttempts times for anything transient.
func sendChunk(addr, token, id string, src *os.File, j chunkSpan) error {
	var err error
	for attempt := 0; attempt < chunkAttempts; attempt++ {
		if attempt > 0 {
			time.Sleep(time.Duration(attempt) * 500 * time.Millisecond)
		}
		ctx, cancel := context.WithTimeout(context.Background(), chunkTimeout)
		req, rerr := http.NewRequestWithContext(ctx, "POST", "http://"+addr+"/api/upload/chunk", io.NewSectionReader(src, j.off, j.n))
		if rerr != nil {
			cancel()
			return rerr // a bad request never gets better by retrying
		}
		req.ContentLength = j.n
		req.Header.Set("X-Upload-Id", id)
		req.Header.Set("X-Chunk-Index", strconv.FormatInt(j.idx, 10))
		if token != "" {
			req.Header.Set("X-Tailsend-Token", token)
		}
		resp, rerr := httpClient.Do(req)
		if rerr != nil {
			cancel()
			err = rerr
			continue
		}
		var cr chunkResult
		derr := json.NewDecoder(resp.Body).Decode(&cr)
		resp.Body.Close()
		cancel()
		if resp.StatusCode == http.StatusOK && derr == nil && cr.OK {
			return nil
		}
		if derr == nil && cr.Error != "" {
			err = fmt.Errorf("chunk %d: %s", j.idx, cr.Error)
		} else if derr != nil {
			err = fmt.Errorf("chunk %d: unexpected receiver response (HTTP %d)", j.idx, resp.StatusCode)
		} else {
			err = fmt.Errorf("chunk %d failed (HTTP %d)", j.idx, resp.StatusCode)
		}
	}
	return err
}

// abortUpload tells the receiver to discard a partial upload. Best effort by
// design: if it fails there is nothing more the sender can do.
func abortUpload(addr, token, id string) {
	if resp, err := postJSON("http://"+addr+"/api/upload/abort", token, idRequest{ID: id}); err == nil {
		resp.Body.Close()
	}
}

// postJSON POSTs v as JSON with the shared-secret header, if one is set.
func postJSON(url_, token string, v any) (*http.Response, error) {
	return postJSONVia(httpClient, url_, token, v, nil)
}

// postJSONHeaders is postJSON with extra headers (e.g. X-Tailsend-Request).
func postJSONHeaders(url_, token string, v any, hdr map[string]string) (*http.Response, error) {
	return postJSONVia(httpClient, url_, token, v, hdr)
}

// postJSONClient is postJSONHeaders through a caller-supplied client (the
// consent round trip needs its own, longer timeout).
func postJSONClient(url_, token string, v any, client *http.Client) (*http.Response, error) {
	return postJSONVia(client, url_, token, v, nil)
}

func postJSONVia(client *http.Client, url_, token string, v any, hdr map[string]string) (*http.Response, error) {
	b, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequest("POST", url_, bytes.NewReader(b))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if token != "" {
		req.Header.Set("X-Tailsend-Token", token)
	}
	for k, val := range hdr {
		req.Header.Set(k, val)
	}
	return client.Do(req)
}

// uploadOne streams one file as a multipart POST. The body is produced by a
// goroutine writing into an io.Pipe, so nothing is buffered in memory.
func uploadOne(addr, token, sender, reqID string, f File, progress Progress) error {
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
		if progress != nil {
			r = &progressReader{r: src, total: f.Size, cb: progress}
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
	if reqID != "" {
		req.Header.Set("X-Tailsend-Request", reqID)
	}

	resp, err := httpClient.Do(req)
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

// progressReader feeds the Progress callback as bytes stream through,
// rate-limited to one call per 200 ms.
type progressReader struct {
	r     io.Reader
	total int64
	done  int64
	cb    Progress
	next  time.Time
}

func (p *progressReader) Read(b []byte) (int, error) {
	n, err := p.r.Read(b)
	p.done += int64(n)
	now := time.Now()
	if err != nil || now.After(p.next) {
		p.next = now.Add(200 * time.Millisecond)
		p.cb(p.done, p.total)
	}
	return n, err
}
