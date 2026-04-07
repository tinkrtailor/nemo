package main

import (
	"net"
	"net/http"
	"net/http/httptest"
	"testing"
)

func parseIP(s string) net.IP {
	return net.ParseIP(s)
}

func TestExtractGitHost(t *testing.T) {
	tests := []struct {
		input    string
		expected string
	}{
		{"git@github.com:user/repo.git", "github.com"},
		{"https://github.com/user/repo.git", "github.com"},
		{"git@gitlab.com:group/project.git", "gitlab.com"},
		{"https://gitlab.com/group/project.git", "gitlab.com"},
		{"", ""}, // empty input = empty host (validated at startup)
	}

	for _, tt := range tests {
		result := extractGitHost(tt.input)
		if result != tt.expected {
			t.Errorf("extractGitHost(%q) = %q, want %q", tt.input, result, tt.expected)
		}
	}
}

func TestIsPrivateIP(t *testing.T) {
	tests := []struct {
		ip       string
		expected bool
	}{
		{"10.0.0.1", true},
		{"172.16.0.1", true},
		{"192.168.1.1", true},
		{"127.0.0.1", true},
		{"169.254.1.1", true},
		{"8.8.8.8", false},
		{"1.1.1.1", false},
		{"140.82.121.4", false}, // github.com
	}

	for _, tt := range tests {
		ip := parseIP(tt.ip)
		if ip == nil {
			t.Fatalf("failed to parse IP: %s", tt.ip)
		}
		result := isPrivateIP(ip)
		if result != tt.expected {
			t.Errorf("isPrivateIP(%s) = %v, want %v", tt.ip, result, tt.expected)
		}
	}
}

func TestParseCIDR(t *testing.T) {
	// Should not panic
	_ = parseCIDR("10.0.0.0/8")
	_ = parseCIDR("172.16.0.0/12")
	_ = parseCIDR("192.168.0.0/16")
}

func TestParseCIDRPanic(t *testing.T) {
	defer func() {
		if r := recover(); r == nil {
			t.Error("expected panic for invalid CIDR")
		}
	}()
	parseCIDR("invalid")
}

// FR-22: /healthz must gate on the global `ready` flag, not just whether
// the health server has bound. The K8s native sidecar startupProbe targets
// /healthz, so returning 200 too early would let the agent container start
// before all proxy ports are listening (issue #53 follow-up).
func TestHealthHandlerGatesOnReadyFlag(t *testing.T) {
	// Save and restore global state.
	prev := ready.Load()
	defer ready.Store(prev)

	// Not ready -> 503
	ready.Store(false)
	rr := httptest.NewRecorder()
	healthHandler(rr, httptest.NewRequest(http.MethodGet, "/healthz", nil))
	if rr.Code != http.StatusServiceUnavailable {
		t.Errorf("expected 503 when not ready, got %d", rr.Code)
	}
	if body := rr.Body.String(); body != `{"status":"starting"}` {
		t.Errorf("expected starting body, got %q", body)
	}

	// Ready -> 200
	ready.Store(true)
	rr = httptest.NewRecorder()
	healthHandler(rr, httptest.NewRequest(http.MethodGet, "/healthz", nil))
	if rr.Code != http.StatusOK {
		t.Errorf("expected 200 when ready, got %d", rr.Code)
	}
	if body := rr.Body.String(); body != `{"status":"ok"}` {
		t.Errorf("expected ok body, got %q", body)
	}
}
