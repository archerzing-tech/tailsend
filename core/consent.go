package core

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sync"
	"time"
)

const (
	// grantTTL is how long an accepted request authorizes its files.
	grantTTL = 10 * time.Minute
	// maxRequestFiles caps one batch. The request carries every name so the
	// begin check can be exact; beyond this the sender must split the batch.
	maxRequestFiles = 5000
)

// ConfirmWait is how long a receiver holds an unanswered transfer request
// open for a human to answer. A var so tests can shorten it.
var ConfirmWait = 30 * time.Second

// consentDeniedMsg goes to senders whose upload names no valid grant.
const consentDeniedMsg = "receiver requires approval for transfers — update the sender or turn off confirmation there"

type grant struct {
	files   map[string]int64
	expires time.Time
}

// Prompt asks a human whether to accept a transfer and returns (accepted,
// reason). It must give up within ConfirmWait.
type Prompt func(sender string, count int, total int64, names []string) (bool, string)

// ConsentGate decides which transfers may start. With Ask off (the default for
// headless receivers) everything is allowed exactly as before protocol v3;
// with Ask on, begin/upload must carry X-Tailsend-Request naming a grant
// created by an accepted /api/request.
type ConsentGate struct {
	Ask   bool
	Token string
	// Prompt, when set, is asked on every transfer while Ask is on. Leave it
	// nil for a receiver where nobody can answer (unattended → auto-refuse).
	Prompt Prompt

	mu     sync.Mutex
	grants map[string]*grant
}

func NewConsentGate(ask bool, token string) *ConsentGate {
	return &ConsentGate{Ask: ask, Token: token, grants: map[string]*grant{}}
}

func (g *ConsentGate) AddGrant(id string, files map[string]int64) {
	g.mu.Lock()
	defer g.mu.Unlock()
	g.grants[id] = &grant{files: files, expires: time.Now().Add(grantTTL)}
}

// Allows reports whether a transfer of name/size may start under reqID.
// sizeKnown is false for v1 uploads, whose size arrives with the bytes.
func (g *ConsentGate) Allows(reqID, name string, size int64, sizeKnown bool) bool {
	if g == nil || !g.Ask {
		return true
	}
	g.mu.Lock()
	defer g.mu.Unlock()
	gr := g.grants[reqID]
	if gr == nil || time.Now().After(gr.expires) {
		return false
	}
	want, ok := gr.files[name]
	return ok && (!sizeKnown || want == size)
}

func (g *ConsentGate) Sweep() {
	g.mu.Lock()
	defer g.mu.Unlock()
	now := time.Now()
	for id, gr := range g.grants {
		if now.After(gr.expires) {
			delete(g.grants, id)
		}
	}
}

// handleRequest answers POST /api/request with a grant id, or a refusal.
func (g *ConsentGate) handleRequest(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "error": "POST only"})
		return
	}
	if !authed(r, g.Token) {
		writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "error": "invalid or missing token"})
		return
	}
	var req struct {
		Sender string `json:"sender"`
		Count  int    `json:"count"`
		Total  int64  `json:"total"`
		Files  []struct {
			Name string `json:"name"`
			Size int64  `json:"size"`
		} `json:"files"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 8<<20)).Decode(&req); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": "bad JSON body"})
		return
	}
	if len(req.Files) == 0 || len(req.Files) > maxRequestFiles {
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "error": fmt.Sprintf("batch must carry 1..%d files", maxRequestFiles)})
		return
	}
	files := make(map[string]int64, len(req.Files))
	names := make([]string, 0, len(req.Files))
	for _, f := range req.Files {
		files[f.Name] = f.Size
		names = append(names, f.Name)
	}

	var accept bool
	var reason string
	switch {
	case !g.Ask: // unattended default: accept automatically, as before v3
		accept = true
	case g.Prompt != nil:
		// The gate owns the deadline: a prompt that never answers (a GUI
		// gone, a user walking away) must not hold the request forever.
		type decision struct {
			accept bool
			reason string
		}
		ch := make(chan decision, 1)
		go func() {
			a, rsn := g.Prompt(OrUnknown(req.Sender), req.Count, req.Total, names)
			ch <- decision{a, rsn}
		}()
		select {
		case d := <-ch:
			accept, reason = d.accept, d.reason
		case <-time.After(ConfirmWait):
			accept, reason = false, "timeout"
		}
	default: // asking, but nobody can answer
		accept, reason = false, "unattended"
	}
	if !accept {
		code := http.StatusForbidden
		if reason == "timeout" {
			code = http.StatusRequestTimeout
		}
		writeJSON(w, code, map[string]any{"ok": false, "error": "transfer not accepted", "reason": reason})
		return
	}
	id := RandomHex(8)
	g.AddGrant(id, files)
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "id": id})
}
