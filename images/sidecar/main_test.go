package main

import (
	"net"
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
		{"", "github.com"}, // fallback
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
