package mobile

import "testing"

func TestExtractAuthURL(t *testing.T) {
	yes := []string{
		"To start this tsnet server, restart with TS_AUTHKEY set, or go to: https://login.tailscale.com/a/394974101b240",
		"some prefix go to: https://login.tailscale.com/a/xyz?footok=1  ",
	}
	no := []string{
		"AuthLoop: state is Running; done",
		"go to: http://insecure.example/a/1",
		"noise",
	}
	for _, l := range yes {
		if u, ok := extractAuthURL(l); !ok || u == "" {
			t.Errorf("extractAuthURL(%q) missed %q", l, u)
		}
	}
	for _, l := range no {
		if _, ok := extractAuthURL(l); ok {
			t.Errorf("extractAuthURL(%q) claimed a URL", l)
		}
	}
}

func TestStatusShape(t *testing.T) {
	n := &Node{Hostname: "phone", state: "starting"}
	st := n.Status()
	if st == "" || st[0] != '{' {
		t.Fatalf("Status not JSON: %q", st)
	}
	// A node with no server must not panic in Status/StopReceiver/LoginURL.
	if got := n.LoginURL(); got != "" {
		t.Fatalf("LoginURL = %q, want empty", got)
	}
	n.StopReceiver() // no receiver yet: must be a no-op
}
