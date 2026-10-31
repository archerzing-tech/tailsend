// Package core holds the tailsend transfer protocol: the receiver engine and
// the sender client. The CLI (repo root) and the embedded tsnet node (mobile/)
// both drive it; the wire format is defined here and must stay compatible
// with the Rust implementation in app/src-tauri.
package core

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// DefaultChunkSize is the v2 chunk size both sides agree on; the receiver's
// begin reply may override it.
const DefaultChunkSize = 4 << 20 // 4 MiB

// File is one file queued for sending: Src on this machine, Rel the path it
// should take on the receiver.
type File struct {
	Src  string
	Rel  string
	Size int64
}

func Human(n int64) string {
	const k, m, g = 1 << 10, 1 << 20, 1 << 30
	switch {
	case n >= g:
		return fmt.Sprintf("%.2f GB", float64(n)/g)
	case n >= m:
		return fmt.Sprintf("%.1f MB", float64(n)/m)
	case n >= k:
		return fmt.Sprintf("%.1f KB", float64(n)/k)
	default:
		return fmt.Sprintf("%d B", n)
	}
}

func Plural(word string, n int) string {
	if n == 1 {
		return word
	}
	return word + "s"
}

func OrUnknown(s string) string {
	if s == "" {
		return "(unknown)"
	}
	return s
}

// UniquePath returns dest, or dest (1), dest (2), ... whichever is free.
func UniquePath(dest string) string {
	if _, err := os.Lstat(dest); os.IsNotExist(err) {
		return dest
	}
	dir, name := filepath.Split(dest)
	ext := filepath.Ext(name)
	base := strings.TrimSuffix(name, ext)
	for i := 1; ; i++ {
		cand := filepath.Join(dir, fmt.Sprintf("%s (%d)%s", base, i, ext))
		if _, err := os.Lstat(cand); os.IsNotExist(err) {
			return cand
		}
	}
}

// RandomHex returns n random bytes as a lowercase hex string, used for upload
// ids and unique temp-file suffixes.
func RandomHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		// crypto/rand can only fail if the OS entropy source is broken; a
		// silent all-zero id would collide between concurrent uploads.
		panic("crypto/rand unavailable: " + err.Error())
	}
	return hex.EncodeToString(b)
}
