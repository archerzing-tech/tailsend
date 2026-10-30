package main

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// defaultChunkSize is the v2 chunk size both sides agree on; the receiver's
// begin reply may override it.
const defaultChunkSize = 4 << 20 // 4 MiB

func fatal(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "tailsend: "+format+"\n", a...)
	os.Exit(1)
}

func human(n int64) string {
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

func humanAgo(t time.Time) string {
	if t.IsZero() {
		return ""
	}
	d := time.Since(t)
	switch {
	case d < time.Minute:
		return "just now"
	case d < time.Hour:
		return fmt.Sprintf("%dm ago", int(d.Minutes()))
	case d < 24*time.Hour:
		return fmt.Sprintf("%dh ago", int(d.Hours()))
	default:
		return fmt.Sprintf("%dd ago", int(d.Hours()/24))
	}
}

func plural(word string, n int) string {
	if n == 1 {
		return word
	}
	return word + "s"
}

func stdoutIsTTY() bool {
	fi, err := os.Stdout.Stat()
	return err == nil && fi.Mode()&os.ModeCharDevice != 0
}

func stdinIsTTY() bool {
	fi, err := os.Stdin.Stat()
	return err == nil && fi.Mode()&os.ModeCharDevice != 0
}

// uniquePath returns dest, or dest (1), dest (2), ... whichever is free.
func uniquePath(dest string) string {
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

// randomHex returns n random bytes as a lowercase hex string, used for upload
// ids and unique temp-file suffixes.
func randomHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		// crypto/rand can only fail if the OS entropy source is broken; a
		// silent all-zero id would collide between concurrent uploads.
		panic("crypto/rand unavailable: " + err.Error())
	}
	return hex.EncodeToString(b)
}
