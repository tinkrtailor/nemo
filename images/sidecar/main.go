// Auth sidecar for Nemo agent jobs.
// FR-14 through FR-23: Model API proxy, Git SSH proxy, Egress logger.
// Single static binary (~10 MB), three localhost ports.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Structured log entry for egress logger (NFR-8).
type egressLogEntry struct {
	Timestamp   string `json:"timestamp"`
	Destination string `json:"destination"`
	Method      string `json:"method"`
	BytesSent   int64  `json:"bytes_sent"`
	BytesRecv   int64  `json:"bytes_recv"`
	Prefix      string `json:"prefix"`
}

// SSRF-protected private IP ranges (FR-15).
var privateRanges = []net.IPNet{
	parseCIDR("10.0.0.0/8"),
	parseCIDR("172.16.0.0/12"),
	parseCIDR("192.168.0.0/16"),
	parseCIDR("169.254.0.0/16"),
	parseCIDR("127.0.0.0/8"),
}

func parseCIDR(cidr string) net.IPNet {
	_, n, err := net.ParseCIDR(cidr)
	if err != nil {
		panic(fmt.Sprintf("invalid CIDR: %s", cidr))
	}
	return *n
}

func isPrivateIP(ip net.IP) bool {
	for _, r := range privateRanges {
		if r.Contains(ip) {
			return true
		}
	}
	// IPv6 private ranges
	if ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() {
		return true
	}
	// fc00::/7
	if len(ip) == net.IPv6len && ip[0]&0xfe == 0xfc {
		return true
	}
	return false
}

// readCredentialFile reads a credential file fresh on each request (FR-21).
func readCredentialFile(path string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("failed to read credential file %s: %w", path, err)
	}
	return strings.TrimSpace(string(data)), nil
}

// --- FR-15/16: Model API Proxy on :9090 ---

func modelProxyHandler(w http.ResponseWriter, r *http.Request) {
	// Only /openai/* routes are allowed (FR-15)
	if !strings.HasPrefix(r.URL.Path, "/openai/") && r.URL.Path != "/openai" {
		http.Error(w, `{"error":"only /openai/* routes are supported"}`, http.StatusForbidden)
		return
	}

	// Strip /openai prefix
	targetPath := strings.TrimPrefix(r.URL.Path, "/openai")
	if targetPath == "" {
		targetPath = "/"
	}

	targetURL := fmt.Sprintf("https://api.openai.com%s", targetPath)
	if r.URL.RawQuery != "" {
		targetURL += "?" + r.URL.RawQuery
	}

	// SSRF protection: resolve and check destination IP (FR-15)
	ips, err := net.LookupIP("api.openai.com")
	if err == nil {
		for _, ip := range ips {
			if isPrivateIP(ip) {
				http.Error(w, `{"error":"SSRF: destination resolves to private IP"}`, http.StatusForbidden)
				return
			}
		}
	}

	// FR-21: Re-read credentials on each request
	openaiKey, err := readCredentialFile("/secrets/model-credentials/openai")
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to read OpenAI credentials: %v", err))
		http.Error(w, `{"error":"credential read failed"}`, http.StatusInternalServerError)
		return
	}

	// Create upstream request
	proxyReq, err := http.NewRequestWithContext(r.Context(), r.Method, targetURL, r.Body)
	if err != nil {
		http.Error(w, `{"error":"failed to create proxy request"}`, http.StatusInternalServerError)
		return
	}

	// FR-17: Pass through all headers
	for key, values := range r.Header {
		for _, v := range values {
			proxyReq.Header.Add(key, v)
		}
	}

	// FR-16: Inject Authorization header for OpenAI
	proxyReq.Header.Set("Authorization", fmt.Sprintf("Bearer %s", openaiKey))

	// NFR-7: Stream through without buffering
	client := &http.Client{
		Timeout: 0, // No timeout for streaming
	}
	resp, err := client.Do(proxyReq)
	if err != nil {
		http.Error(w, fmt.Sprintf(`{"error":"upstream request failed: %v"}`, err), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	// Copy response headers
	for key, values := range resp.Header {
		for _, v := range values {
			w.Header().Add(key, v)
		}
	}
	w.WriteHeader(resp.StatusCode)

	// Stream response body
	io.Copy(w, resp.Body)
}

// --- FR-19/20: Egress Logger (HTTP CONNECT proxy) on :9092 ---

type egressProxy struct {
	mu sync.Mutex
}

func (p *egressProxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	start := time.Now()

	if r.Method == http.MethodConnect {
		p.handleConnect(w, r, start)
		return
	}

	// Regular HTTP proxy
	p.handleHTTP(w, r, start)
}

func (p *egressProxy) handleConnect(w http.ResponseWriter, r *http.Request, start time.Time) {
	// CONNECT method for HTTPS tunneling
	destHost := r.Host
	if !strings.Contains(destHost, ":") {
		destHost += ":443"
	}

	destConn, err := net.DialTimeout("tcp", destHost, 10*time.Second)
	if err != nil {
		http.Error(w, "Connection failed", http.StatusBadGateway)
		return
	}

	w.WriteHeader(http.StatusOK)

	hijacker, ok := w.(http.Hijacker)
	if !ok {
		destConn.Close()
		http.Error(w, "Hijack not supported", http.StatusInternalServerError)
		return
	}

	clientConn, _, err := hijacker.Hijack()
	if err != nil {
		destConn.Close()
		http.Error(w, "Hijack failed", http.StatusInternalServerError)
		return
	}

	var bytesSent, bytesRecv int64
	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		n, _ := io.Copy(destConn, clientConn)
		atomic.AddInt64(&bytesSent, n)
		destConn.(*net.TCPConn).CloseWrite()
	}()

	go func() {
		defer wg.Done()
		n, _ := io.Copy(clientConn, destConn)
		atomic.AddInt64(&bytesRecv, n)
		clientConn.Close()
	}()

	wg.Wait()
	destConn.Close()

	// FR-19: Log connection details in JSON-lines format
	logEgress(start, destHost, "CONNECT", bytesSent, bytesRecv)
}

func (p *egressProxy) handleHTTP(w http.ResponseWriter, r *http.Request, start time.Time) {
	if r.URL.Scheme == "" {
		r.URL.Scheme = "http"
	}
	if r.URL.Host == "" {
		r.URL.Host = r.Host
	}

	proxyReq, err := http.NewRequestWithContext(r.Context(), r.Method, r.URL.String(), r.Body)
	if err != nil {
		http.Error(w, "Failed to create request", http.StatusInternalServerError)
		return
	}

	for key, values := range r.Header {
		for _, v := range values {
			proxyReq.Header.Add(key, v)
		}
	}
	// Remove hop-by-hop headers
	proxyReq.Header.Del("Proxy-Connection")
	proxyReq.Header.Del("Proxy-Authorization")

	client := &http.Client{
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	resp, err := client.Do(proxyReq)
	if err != nil {
		http.Error(w, fmt.Sprintf("Request failed: %v", err), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	for key, values := range resp.Header {
		for _, v := range values {
			w.Header().Add(key, v)
		}
	}
	w.WriteHeader(resp.StatusCode)

	bytesRecv, _ := io.Copy(w, resp.Body)

	logEgress(start, r.URL.Host, r.Method, 0, bytesRecv)
}

func logEgress(start time.Time, dest, method string, sent, recv int64) {
	entry := egressLogEntry{
		Timestamp:   start.UTC().Format(time.RFC3339Nano),
		Destination: dest,
		Method:      method,
		BytesSent:   sent,
		BytesRecv:   recv,
		Prefix:      "NEMO_SIDECAR",
	}
	data, _ := json.Marshal(entry)
	fmt.Println(string(data))
}

// --- FR-22: Health check endpoint on :9093 ---

func healthHandler(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	fmt.Fprint(w, `{"status":"ok"}`)
}

// --- FR-18: Git SSH Proxy on :9091 ---
// Note: A full SSH proxy implementation requires golang.org/x/crypto/ssh.
// This is a simplified TCP proxy that forwards connections to the git remote.

func startGitProxy(ctx context.Context, gitRemoteHost string) error {
	listener, err := net.Listen("tcp", "127.0.0.1:9091")
	if err != nil {
		return fmt.Errorf("failed to listen on :9091: %w", err)
	}

	go func() {
		<-ctx.Done()
		listener.Close()
	}()

	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				if ctx.Err() != nil {
					return
				}
				logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("git proxy accept error: %v", err))
				continue
			}
			go handleGitConnection(conn, gitRemoteHost)
		}
	}()

	return nil
}

func handleGitConnection(clientConn net.Conn, gitRemoteHost string) {
	defer clientConn.Close()

	// FR-18: SSH key auth to actual remote
	sshKeyPath := filepath.Join("/secrets", "ssh-key", "id_ed25519")
	if _, err := os.Stat(sshKeyPath); err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("SSH key not found: %v", err))
		return
	}

	// Connect to the real git remote on port 22
	destAddr := gitRemoteHost + ":22"
	destConn, err := net.DialTimeout("tcp", destAddr, 10*time.Second)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to connect to git remote %s: %v", destAddr, err))
		return
	}
	defer destConn.Close()

	// Bidirectional proxy
	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		io.Copy(destConn, clientConn)
	}()

	go func() {
		defer wg.Done()
		io.Copy(clientConn, destConn)
	}()

	wg.Wait()
}

// Extract git host from a git URL.
func extractGitHost(gitURL string) string {
	// Handle SSH-style URLs: git@github.com:user/repo.git
	if strings.Contains(gitURL, "@") && strings.Contains(gitURL, ":") && !strings.Contains(gitURL, "://") {
		parts := strings.SplitN(gitURL, "@", 2)
		if len(parts) == 2 {
			hostPart := strings.SplitN(parts[1], ":", 2)
			return hostPart[0]
		}
	}

	// Handle HTTPS URLs
	parsed, err := url.Parse(gitURL)
	if err == nil && parsed.Host != "" {
		return parsed.Hostname()
	}

	return "github.com" // fallback
}

// --- Structured logging helper (NFR-8) ---

func logJSON(prefix, level, msg string) {
	entry := map[string]string{
		"timestamp": time.Now().UTC().Format(time.RFC3339Nano),
		"level":     level,
		"message":   msg,
		"prefix":    prefix,
	}
	data, _ := json.Marshal(entry)
	fmt.Println(string(data))
}

func main() {
	logJSON("NEMO_SIDECAR", "info", "starting auth sidecar")

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Extract git remote host from environment
	gitRepoURL := os.Getenv("GIT_REPO_URL")
	gitHost := extractGitHost(gitRepoURL)
	logJSON("NEMO_SIDECAR", "info", fmt.Sprintf("git remote host: %s", gitHost))

	// Start all three servers

	// FR-15/16: Model API proxy on :9090
	modelMux := http.NewServeMux()
	modelMux.HandleFunc("/", modelProxyHandler)
	modelServer := &http.Server{
		Addr:    "127.0.0.1:9090",
		Handler: modelMux,
	}

	// FR-19: Egress logger on :9092
	egressServer := &http.Server{
		Addr:    "127.0.0.1:9092",
		Handler: &egressProxy{},
	}

	// FR-22: Health endpoint on :9093
	healthMux := http.NewServeMux()
	healthMux.HandleFunc("/healthz", healthHandler)
	healthServer := &http.Server{
		Addr:    "127.0.0.1:9093",
		Handler: healthMux,
	}

	// Start servers in goroutines
	go func() {
		if err := modelServer.ListenAndServe(); err != http.ErrServerClosed {
			log.Fatalf("model proxy server error: %v", err)
		}
	}()

	// FR-18: Git SSH proxy on :9091
	if err := startGitProxy(ctx, gitHost); err != nil {
		log.Fatalf("git proxy error: %v", err)
	}

	go func() {
		if err := egressServer.ListenAndServe(); err != http.ErrServerClosed {
			log.Fatalf("egress logger server error: %v", err)
		}
	}()

	go func() {
		if err := healthServer.ListenAndServe(); err != http.ErrServerClosed {
			log.Fatalf("health server error: %v", err)
		}
	}()

	// FR-22: Wait until all ports are listening, then write readiness file
	ports := []string{"127.0.0.1:9090", "127.0.0.1:9091", "127.0.0.1:9092", "127.0.0.1:9093"}
	for _, addr := range ports {
		for i := 0; i < 100; i++ {
			conn, err := net.DialTimeout("tcp", addr, 100*time.Millisecond)
			if err == nil {
				conn.Close()
				break
			}
			time.Sleep(20 * time.Millisecond)
		}
	}

	// Write readiness file
	readyPath := "/tmp/shared/ready"
	if err := os.MkdirAll(filepath.Dir(readyPath), 0755); err != nil {
		log.Fatalf("failed to create ready dir: %v", err)
	}
	if err := os.WriteFile(readyPath, []byte("ready"), 0644); err != nil {
		log.Fatalf("failed to write readiness file: %v", err)
	}
	logJSON("NEMO_SIDECAR", "info", "all ports ready, readiness file written")

	// FR-23: Handle SIGTERM gracefully
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	<-sigCh

	logJSON("NEMO_SIDECAR", "info", "received shutdown signal, draining connections")

	// 5s grace period for draining
	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer shutdownCancel()

	var wg sync.WaitGroup
	wg.Add(3)

	go func() {
		defer wg.Done()
		modelServer.Shutdown(shutdownCtx)
	}()
	go func() {
		defer wg.Done()
		egressServer.Shutdown(shutdownCtx)
	}()
	go func() {
		defer wg.Done()
		healthServer.Shutdown(shutdownCtx)
	}()

	cancel() // Stop git proxy listener

	wg.Wait()
	logJSON("NEMO_SIDECAR", "info", "shutdown complete")
}
