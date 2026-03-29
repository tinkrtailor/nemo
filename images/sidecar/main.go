// Auth sidecar for Nemo agent jobs.
// FR-14 through FR-23: Model API proxy, Git SSH proxy, Egress logger.
// Single static binary (~10 MB), three localhost ports.
package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
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

	"golang.org/x/crypto/ssh"
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

// Track active SSH sessions for graceful shutdown.
var sshWg sync.WaitGroup

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
	// Route to the correct upstream based on path prefix
	var targetHost, credFile, authHeader string

	if strings.HasPrefix(r.URL.Path, "/openai/") || r.URL.Path == "/openai" {
		targetHost = "api.openai.com"
		credFile = "/secrets/model-credentials/openai"
		authHeader = "Bearer"
		r.URL.Path = strings.TrimPrefix(r.URL.Path, "/openai")
	} else if strings.HasPrefix(r.URL.Path, "/anthropic/") || r.URL.Path == "/anthropic" {
		targetHost = "api.anthropic.com"
		credFile = "/secrets/model-credentials/anthropic"
		authHeader = "x-api-key" // Anthropic uses x-api-key header, not Bearer
		r.URL.Path = strings.TrimPrefix(r.URL.Path, "/anthropic")
	} else {
		http.Error(w, `{"error":"only /openai/* and /anthropic/* routes are supported"}`, http.StatusForbidden)
		return
	}

	targetPath := r.URL.Path
	if targetPath == "" {
		targetPath = "/"
	}

	targetURL := fmt.Sprintf("https://%s%s", targetHost, targetPath)
	if r.URL.RawQuery != "" {
		targetURL += "?" + r.URL.RawQuery
	}

	// SSRF protection: resolve and check destination IP (FR-15)
	ips, err := net.LookupIP(targetHost)
	if err == nil {
		for _, ip := range ips {
			if isPrivateIP(ip) {
				http.Error(w, `{"error":"SSRF: destination resolves to private IP"}`, http.StatusForbidden)
				return
			}
		}
	}

	// FR-21: Re-read credentials on each request
	apiKey, err := readCredentialFile(credFile)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to read credentials from %s: %v", credFile, err))
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

	// Inject auth: OpenAI uses Bearer token, Anthropic uses x-api-key header
	if authHeader == "x-api-key" {
		proxyReq.Header.Set("x-api-key", apiKey)
		// Anthropic also requires anthropic-version header
		if proxyReq.Header.Get("anthropic-version") == "" {
			proxyReq.Header.Set("anthropic-version", "2023-06-01")
		}
	} else {
		proxyReq.Header.Set("Authorization", fmt.Sprintf("Bearer %s", apiKey))
	}

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

	// SSRF protection: prevent pivoting into internal services
	hostname := strings.Split(destHost, ":")[0]
	ips, err := net.LookupIP(hostname)
	if err == nil {
		for _, ip := range ips {
			if isPrivateIP(ip) {
				logJSON("NEMO_SIDECAR", "warn", fmt.Sprintf("blocked CONNECT to private IP: %s -> %v", destHost, ip))
				http.Error(w, "CONNECT to private/internal addresses is blocked", http.StatusForbidden)
				return
			}
		}
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

	// SSRF protection: block requests to private/internal addresses
	hostname := strings.Split(r.URL.Host, ":")[0]
	if ips, err := net.LookupIP(hostname); err == nil {
		for _, ip := range ips {
			if isPrivateIP(ip) {
				logJSON("NEMO_SIDECAR", "warn", fmt.Sprintf("blocked HTTP to private IP: %s -> %v", r.URL.Host, ip))
				http.Error(w, "Requests to private/internal addresses are blocked", http.StatusForbidden)
				return
			}
		}
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
// Accepts SSH connections from the agent container, validates that only
// git-upload-pack and git-receive-pack are executed, authenticates with
// the mounted SSH key, and proxies the operation to the configured git remote.
// Port forwarding, remote exec, environment passing, and PTY are disabled.

// Allowed git SSH commands (FR-18).
var allowedGitCommands = map[string]bool{
	"git-upload-pack":  true,
	"git-receive-pack": true,
}

func startGitProxy(ctx context.Context, gitRemoteHost string, allowedRepoPath string) error {
	// Generate an ephemeral host key for the local SSH server.
	_, hostPriv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return fmt.Errorf("failed to generate host key: %w", err)
	}
	hostSigner, err := ssh.NewSignerFromKey(hostPriv)
	if err != nil {
		return fmt.Errorf("failed to create host signer: %w", err)
	}

	config := &ssh.ServerConfig{
		NoClientAuth: true, // Agent on same pod, no auth needed for local proxy
	}
	config.AddHostKey(hostSigner)

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
			go handleSSHConnection(conn, config, gitRemoteHost, allowedRepoPath)
		}
	}()

	return nil
}

func handleSSHConnection(nConn net.Conn, config *ssh.ServerConfig, gitRemoteHost string, allowedRepoPath string) {
	sshWg.Add(1)
	defer sshWg.Done()
	defer nConn.Close()

	// Perform SSH handshake with the agent
	sshConn, chans, reqs, err := ssh.NewServerConn(nConn, config)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("SSH handshake failed: %v", err))
		return
	}
	defer sshConn.Close()

	// FR-18: Reject all global requests (port forwarding, etc.)
	go ssh.DiscardRequests(reqs)

	for newChan := range chans {
		if newChan.ChannelType() != "session" {
			newChan.Reject(ssh.UnknownChannelType, "only session channels allowed")
			continue
		}

		channel, requests, err := newChan.Accept()
		if err != nil {
			logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to accept channel: %v", err))
			continue
		}

		go handleSSHSession(channel, requests, gitRemoteHost, allowedRepoPath)
	}
}

func handleSSHSession(channel ssh.Channel, requests <-chan *ssh.Request, gitRemoteHost string, allowedRepoPath string) {
	defer channel.Close()

	for req := range requests {
		switch req.Type {
		case "exec":
			// Parse the command from the exec request
			if len(req.Payload) < 4 {
				req.Reply(false, nil)
				continue
			}
			cmdLen := int(req.Payload[0])<<24 | int(req.Payload[1])<<16 | int(req.Payload[2])<<8 | int(req.Payload[3])
			if cmdLen+4 > len(req.Payload) {
				req.Reply(false, nil)
				continue
			}
			fullCmd := string(req.Payload[4 : 4+cmdLen])

			// FR-18: Only allow git-upload-pack and git-receive-pack
			cmdParts := strings.SplitN(fullCmd, " ", 2)
			cmdName := cmdParts[0]

			if !allowedGitCommands[cmdName] {
				logJSON("NEMO_SIDECAR", "warn", fmt.Sprintf("rejected SSH command: %s", cmdName))
				req.Reply(false, nil)
				channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
				continue
			}

			// Finding 6: Validate repo path matches configured GIT_REPO_URL
			if allowedRepoPath != "" && len(cmdParts) == 2 {
				requestedRepo := strings.Trim(cmdParts[1], "' \"")
				requestedRepo = strings.TrimPrefix(requestedRepo, "/")
				normalizedAllowed := strings.TrimPrefix(allowedRepoPath, "/")
				if requestedRepo != normalizedAllowed {
					logJSON("NEMO_SIDECAR", "warn", fmt.Sprintf(
						"rejected git command: repo path %q does not match allowed %q",
						requestedRepo, normalizedAllowed))
					req.Reply(false, nil)
					channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
					continue
				}
			}

			logJSON("NEMO_SIDECAR", "info", fmt.Sprintf("proxying git command: %s to %s", cmdName, gitRemoteHost))
			req.Reply(true, nil)

			// Proxy the command to the actual git remote via SSH
			proxyGitCommand(channel, fullCmd, gitRemoteHost)
			return

		case "env":
			// FR-18: Reject environment variable passing
			req.Reply(false, nil)

		case "pty-req":
			// FR-18: Reject PTY allocation
			req.Reply(false, nil)

		case "subsystem":
			// Reject subsystem requests
			req.Reply(false, nil)

		default:
			req.Reply(false, nil)
		}
	}
}

func proxyGitCommand(channel ssh.Channel, command string, gitRemoteHost string) {
	// Read SSH key for authenticating to the real remote
	sshKeyPath := filepath.Join("/secrets", "ssh-key", "id_ed25519")
	keyData, err := os.ReadFile(sshKeyPath)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to read SSH key: %v", err))
		channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
		return
	}

	signer, err := ssh.ParsePrivateKey(keyData)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to parse SSH key: %v", err))
		channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
		return
	}

	// Connect to the real git remote
	// Use known_hosts file for host key verification — no InsecureIgnoreHostKey fallback.
	var hostKeyCallback ssh.HostKeyCallback
	knownHostsPath := "/secrets/ssh-known-hosts/known_hosts"
	if _, err := os.Stat(knownHostsPath); err == nil {
		// Parse known_hosts manually for host key verification
		khData, readErr := os.ReadFile(knownHostsPath)
		if readErr == nil && len(khData) > 0 {
			hostKeyCallback = func(hostname string, remote net.Addr, key ssh.PublicKey) error {
				// Parse each line of known_hosts
				remaining := khData
				for len(remaining) > 0 {
					_, hosts, pubKey, _, rest, parseErr := ssh.ParseKnownHosts(remaining)
					if parseErr != nil {
						remaining = rest
						continue
					}
					remaining = rest
					for _, h := range hosts {
						// Match hostname (with or without port)
						if h == hostname || h == strings.Split(hostname, ":")[0] {
							if key.Type() == pubKey.Type() && string(key.Marshal()) == string(pubKey.Marshal()) {
								return nil
							}
						}
					}
				}
				return fmt.Errorf("host key verification failed for %s", hostname)
			}
		} else {
			logJSON("NEMO_SIDECAR", "error", "known_hosts file is empty or unreadable — refusing to connect without host key verification")
			channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
			return
		}
	} else {
		logJSON("NEMO_SIDECAR", "error", "known_hosts file not found at /secrets/ssh-known-hosts/known_hosts — refusing to connect")
		channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
		return
	}

	clientConfig := &ssh.ClientConfig{
		User: "git",
		Auth: []ssh.AuthMethod{
			ssh.PublicKeys(signer),
		},
		HostKeyCallback: hostKeyCallback,
		Timeout:         10 * time.Second,
	}

	destAddr := gitRemoteHost // host:port already formatted by caller
	client, err := ssh.Dial("tcp", destAddr, clientConfig)
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to connect to git remote %s: %v", destAddr, err))
		channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
		return
	}
	defer client.Close()

	session, err := client.NewSession()
	if err != nil {
		logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("failed to create SSH session: %v", err))
		channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{1}))
		return
	}
	defer session.Close()

	// Pipe stdin/stdout/stderr between agent channel and remote session
	session.Stdin = channel
	session.Stdout = channel
	session.Stderr = channel.Stderr()

	err = session.Run(command)
	exitCode := uint32(0)
	if err != nil {
		if exitErr, ok := err.(*ssh.ExitError); ok {
			exitCode = uint32(exitErr.ExitStatus())
		} else {
			logJSON("NEMO_SIDECAR", "error", fmt.Sprintf("git command error: %v", err))
			exitCode = 1
		}
	}

	channel.SendRequest("exit-status", false, ssh.Marshal(struct{ Status uint32 }{exitCode}))
}

// gitRemoteInfo holds parsed git remote details for SSH proxy validation.
type gitRemoteInfo struct {
	host     string
	port     string // SSH port, defaults to "22"
	repoPath string // e.g., "user/repo.git" or "/user/repo.git"
}

// extractGitRemote parses host, port, and repo path from a git URL.
func extractGitRemote(gitURL string) gitRemoteInfo {
	// Handle SSH-style URLs: git@github.com:user/repo.git
	// or with port: ssh://git@github.com:2222/user/repo.git
	if strings.HasPrefix(gitURL, "ssh://") {
		parsed, err := url.Parse(gitURL)
		if err == nil && parsed.Host != "" {
			port := parsed.Port()
			if port == "" {
				port = "22"
			}
			return gitRemoteInfo{
				host:     parsed.Hostname(),
				port:     port,
				repoPath: strings.TrimPrefix(parsed.Path, "/"),
			}
		}
	}

	if strings.Contains(gitURL, "@") && strings.Contains(gitURL, ":") && !strings.Contains(gitURL, "://") {
		parts := strings.SplitN(gitURL, "@", 2)
		if len(parts) == 2 {
			hostAndPath := strings.SplitN(parts[1], ":", 2)
			host := hostAndPath[0]
			repoPath := ""
			if len(hostAndPath) == 2 {
				repoPath = hostAndPath[1]
			}
			return gitRemoteInfo{host: host, port: "22", repoPath: repoPath}
		}
	}

	// Handle HTTPS URLs
	parsed, err := url.Parse(gitURL)
	if err == nil && parsed.Host != "" {
		port := parsed.Port()
		if port == "" {
			port = "22"
		}
		return gitRemoteInfo{
			host:     parsed.Hostname(),
			port:     port,
			repoPath: strings.TrimPrefix(parsed.Path, "/"),
		}
	}

	return gitRemoteInfo{} // empty — caller must validate
}

// extractGitHost returns just the host portion (backward compat).
func extractGitHost(gitURL string) string {
	return extractGitRemote(gitURL).host
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

	// Extract git remote host and repo path from environment
	gitRepoURL := os.Getenv("GIT_REPO_URL")
	if gitRepoURL == "" {
		log.Fatalf("GIT_REPO_URL environment variable is required")
	}
	remote := extractGitRemote(gitRepoURL)
	if remote.host == "" {
		log.Fatalf("Failed to parse git host from GIT_REPO_URL: %s", gitRepoURL)
	}
	logJSON("NEMO_SIDECAR", "info", fmt.Sprintf("git remote host: %s, allowed repo: %s", remote.host, remote.repoPath))

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
	gitHostAddr := fmt.Sprintf("%s:%s", remote.host, remote.port)
	if err := startGitProxy(ctx, gitHostAddr, remote.repoPath); err != nil {
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

	// FR-22: Wait until all ports are listening, then write readiness file.
	// Fail hard if any port doesn't bind within timeout.
	ports := []string{"127.0.0.1:9090", "127.0.0.1:9091", "127.0.0.1:9092", "127.0.0.1:9093"}
	for _, addr := range ports {
		bound := false
		for i := 0; i < 100; i++ {
			conn, err := net.DialTimeout("tcp", addr, 100*time.Millisecond)
			if err == nil {
				conn.Close()
				bound = true
				break
			}
			time.Sleep(20 * time.Millisecond)
		}
		if !bound {
			log.Fatalf("sidecar port %s failed to bind within 2s", addr)
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

	// Stop accepting new git proxy connections first
	cancel()

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

	// Wait for active SSH sessions with timeout — don't hang indefinitely.
	sshDone := make(chan struct{})
	go func() {
		sshWg.Wait()
		close(sshDone)
	}()
	select {
	case <-sshDone:
	case <-shutdownCtx.Done():
		logJSON("NEMO_SIDECAR", "warn", "SSH session drain timed out, proceeding with shutdown")
	}

	wg.Wait()
	logJSON("NEMO_SIDECAR", "info", "shutdown complete")
}
