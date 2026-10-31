// Interface fallback for Android devices whose SELinux policy denies app
// UIDs the NETLINK_ROUTE socket (Huawei/HarmonyOS at least; stock Android
// has been moving this way too — tailscale#2293, golang/go#40569). With
// enumeration denied, netmon cannot start and tsnet dies with
// "route ip+net: netlinkrib: permission denied" before it can dial.
//
// tailscale ships this exact workaround as feature/androidbin for raw
// binaries; its registration is build-gated to non-cgo builds, so tsnet
// apps must opt in themselves. Logic below mirrors androidbin's
// buildSynthetic: one synthetic interface carrying the kernel's chosen
// source addresses, discovered by dialing UDP (which only makes the
// kernel pick a route — no packets are sent, and the sandbox allows it).
// Good enough for magicsock to learn its local endpoints; it cannot see
// the full multi-interface picture.

package mobile

import (
	"errors"
	"net"
	"net/netip"
	"runtime"
	"time"

	"tailscale.com/net/netmon"
)

func init() {
	if runtime.GOOS == "android" {
		netmon.HookInterfacesFallback.Set(fallbackInterfaces)
	}
}

// fallbackInterfaces runs only after net.Interfaces has already failed.
func fallbackInterfaces() ([]netmon.Interface, error) {
	var addrs []net.Addr
	if ip, ok := outboundIP("udp4", "8.8.8.8:53"); ok {
		addrs = append(addrs, &net.IPNet{IP: ip.AsSlice(), Mask: net.CIDRMask(32, 32)})
	}
	if ip, ok := outboundIP("udp6", "[2001:4860:4860::8888]:53"); ok {
		addrs = append(addrs, &net.IPNet{IP: ip.AsSlice(), Mask: net.CIDRMask(128, 128)})
	}
	if len(addrs) == 0 {
		return nil, errors.New("node: no outbound routes found")
	}
	return []netmon.Interface{{
		Interface: &net.Interface{
			MTU:   1500,
			Name:  "android",
			Flags: net.FlagUp | net.FlagRunning,
		},
		AltAddrs: addrs,
	}}, nil
}

// outboundIP reports the local source address the kernel picks for an
// outbound socket to addr. Dialing UDP performs no I/O, so this works
// where interface enumeration is denied.
func outboundIP(network, addr string) (netip.Addr, bool) {
	d := net.Dialer{Timeout: 2 * time.Second}
	c, err := d.Dial(network, addr)
	if err != nil {
		return netip.Addr{}, false
	}
	defer c.Close()
	ua, ok := c.LocalAddr().(*net.UDPAddr)
	if !ok {
		return netip.Addr{}, false
	}
	ip, ok := netip.AddrFromSlice(ua.IP)
	if !ok {
		return netip.Addr{}, false
	}
	ip = ip.Unmap()
	if ip.IsLoopback() || ip.IsUnspecified() {
		return netip.Addr{}, false
	}
	return ip, true
}
