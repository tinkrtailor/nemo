# Agent Runtime Layer

## Overview

Runtime infrastructure for Nemo agent jobs: the container image agents execute in, the auth sidecar that isolates credentials, the K8s Job template that composes them, the prompt templates that drive each stage, and the Terraform module that provisions the cluster. This spec covers Lane C of the implementation plan.

## Dependencies

- **Requires:** [Design doc](../docs/design.md) (architecture, resource model, loop logic, verdict schema). Note: Postgres (not SQLite) was decided during eng review; see design doc update.
- **Required by:** Control plane loop engine (dispatches jobs defined here), CLI (submits specs that trigger these jobs)

## Requirements

### Functional Requirements

#### Base Agent Image

- FR-1: The base image shall include git, curl, jq, tomlq (from `pip install yq`, provides TOML-to-JSON conversion for parsing `nemo.toml`), build-essential, Node.js 22 LTS, and Python 3.12 runtime
- FR-2: The base image shall install claude-code via `npm install -g @anthropic-ai/claude-code`
- FR-3: The base image shall install opencode from `ghcr.io/anomalyco/opencode` (binary copy from their published image)
- FR-4: The image entrypoint (`/usr/local/bin/nemo-agent-entry`) shall read `$STAGE` and dispatch to the correct CLI tool per the table below. The entrypoint shall use `exec` to replace the shell with the CLI tool process (or use `tini` as PID 1) to ensure correct signal handling.

  | `$STAGE` value | Command |
  |-------|---------|
  | `implement` | `exec claude -p "$(cat $PROMPT_FILE)" --output-format stream-json --dangerously-skip-permissions --resume "$SESSION_ID"` (omit `--resume` if round 1) |
  | `revise` | same as `implement` |
  | `review` | `exec opencode run --format json --prompt "$(cat $PROMPT_FILE)" -s "$SESSION_ID"` (omit `-s` if round 1) |
  | `audit` | same as `review` |
  | `test` | `exec bash -c 'TOML_JSON=$(tomlq -r . /work/nemo.toml); for svc in $(echo $AFFECTED_SERVICES | jq -r ".[]"); do cmd=$(echo "$TOML_JSON" | jq -r ".services[\"$svc\"].test"); eval "$cmd"; done'` |

  The `$STAGE` environment variable uses short names (`implement`, `test`, `review`, `audit`, `revise`) -- not DB enum values.

  Note: `nemo.toml` is TOML, not JSON. The entrypoint uses `tomlq` (included in the base image, see FR-1) to convert TOML to JSON before extracting service test commands with `jq`. `tomlq` is part of the `yq` package (`pip install yq`) which provides a `jq` wrapper for TOML files.

  On error, the entrypoint shall write to stderr in the format: `NEMO_ERROR: <stage>: <message>` (one line, no stack traces).
- FR-5: For IMPLEMENT and REVISE stages, the entrypoint shall invoke `claude -p --output-format stream-json --dangerously-skip-permissions` with the prompt assembled from template + spec + feedback. The default implement.md template MUST include the directive: "You must implement all functionality fully. Mock implementations, placeholder functions, TODO stubs, and fake data stores are forbidden. Every code path must be real and complete."
- FR-6: For REVIEW and AUDIT stages, the entrypoint shall invoke `opencode run --format json` with the prompt assembled from template + spec + diff context. The review stage entrypoint shall configure opencode with permission restrictions: `{ "edit": "deny", "bash": "deny", "read": "allow" }` to ensure the reviewer is read-only. The worktree volume shall be mounted read-only for REVIEW and AUDIT stages.
- FR-7: For round > 1, the entrypoint shall pass `--resume $SESSION_ID` (claude) or `-s $SESSION_ID` (opencode) to continue the prior session
- FR-8: The entrypoint shall configure proxy environment variables so all outbound traffic routes through the sidecar egress logger: `HTTP_PROXY=http://localhost:9092`, `HTTPS_PROXY=http://localhost:9092`, `http_proxy=http://localhost:9092`, `https_proxy=http://localhost:9092`, `NO_PROXY=localhost,127.0.0.1,::1`, `no_proxy=localhost,127.0.0.1,::1`. Both upper- and lower-case variants are required because different tools respect different conventions. `NO_PROXY` prevents double-proxying when the agent calls localhost services (model API on :9090, git proxy on :9091, egress logger on :9092). **V1 network enforcement model:** The HTTP_PROXY environment variables are the primary enforcement mechanism. All standard tools (curl, npm, pip, cargo, wget, git-over-https) respect `HTTP_PROXY`/`HTTPS_PROXY`. Raw TCP connections that bypass the proxy are NOT logged in V1. The iptables rules (FR-41a) serve as defense-in-depth to drop non-TCP egress (UDP/ICMP) and disable IPv6, but do NOT redirect TCP to the proxy (REDIRECT to an HTTP CONNECT proxy does not work for raw TCP). This is an accepted V1 limitation documented in Out of Scope.
- FR-9: The entrypoint shall configure `OPENAI_BASE_URL=http://localhost:9090/openai` so OpenAI model API calls route through the sidecar auth proxy. The sidecar routes `/openai/*` → `https://api.openai.com/*` (strips prefix). **Claude Code does NOT use the sidecar for model auth.** `ANTHROPIC_BASE_URL` is NOT set. Claude Code connects directly to `api.anthropic.com` using its session auth from the mounted `~/.claude/` directory (see FR-15, FR-25b).
- FR-10: The entrypoint shall set `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` from environment variables
- FR-11: The entrypoint shall set `GIT_SSH_COMMAND` to a script that connects to `localhost:9091` instead of the real remote. The sidecar runs a local SSH server on `:9091` that authenticates with the mounted SSH key and proxies the push to the actual git remote.
- FR-12: Per-monorepo images shall extend the base via `Dockerfile.nemo` in the repo root (e.g., `FROM ghcr.io/nemo/agent-base:latest`)
- FR-13: On exit, the entrypoint shall write the result as a single JSON line prefixed with `NEMO_RESULT:` to stdout. Format: `NEMO_RESULT:{"stage":"implement","data":{...}}`. **This is the typed output contract between agent and control plane.** The control plane reads pod logs, finds the line starting with `NEMO_RESULT:`, strips the prefix, and parses the remainder as the stage output JSON. This is how Lane A's `parse_output()` trait method works: it reads pod logs (via kube-rs pod/log API), scans for the `NEMO_RESULT:` line, and deserializes into the stage-specific output type. Pod logs are the authoritative output channel. Sidecar log lines use prefix `NEMO_SIDECAR:`. Model streaming output uses prefix `NEMO_MODEL:`. The control plane parses pod logs by prefix to avoid interleaving ambiguity. The result is also written to `/output/result.json` (for the agent's own use during execution, NOT read by the control plane). Result envelope: `{ "stage": "implement|test|review|audit|revise", "data": { ...stage-specific fields... } }`. The `stage` field uses short names (not DB enum values). The control plane dispatches parsing based on the `stage` field. Stage-specific `data` fields: IMPLEMENT: `new_sha`, `token_usage`, `exit_code`, `session_id`; TEST: see FR-42d; REVIEW/AUDIT: `verdict`, `token_usage`, `exit_code`, `session_id`; REVISE: `revised_spec_path`, `new_sha`, `token_usage`, `exit_code`, `session_id`. The revise entrypoint commits the revised spec and includes the new commit SHA in the result envelope.

#### Auth Sidecar

- FR-14: The sidecar shall be a single static binary (Go, ~10 MB) listening on three localhost ports
- FR-15: Model API proxy (`:9090`): used for OpenAI API calls only. Route requests by path prefix: `/openai/*` → `https://api.openai.com/*` (strip prefix, inject `Authorization: Bearer` header). Credentials read from K8s Secret mounted at `/secrets/model-credentials/openai`. The proxy shall ONLY accept requests to `api.openai.com`. Requests to any other destination shall be rejected with HTTP 403. The proxy shall reject requests whose resolved destination is a private/internal IP (169.254.0.0/16, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8, ::1/128, fc00::/7, fe80::/10) to prevent SSRF. **Claude auth is NOT proxied through :9090.** Claude Code reads its own auth from `~/.claude/` (session directory mounted from the K8s Secret into the agent container's HOME at `/work/home/.claude/`). Claude Code connects directly to `api.anthropic.com` using its session auth. The sidecar model proxy is only for OpenAI (opencode).
- FR-16: Model API proxy shall inject `Authorization: Bearer $KEY` for OpenAI requests. Anthropic header injection (`x-api-key`) is not needed in V1 because Claude Code uses its own session auth directly (not the sidecar proxy).
- FR-17: Model API proxy shall pass through all other headers and body unmodified
- FR-18: Git SSH proxy (`:9091`): run a local SSH server that accepts connections from the agent container. The proxy shall ONLY connect to the single git remote host extracted from `git_repo_url` in the environment (e.g., `github.com`). Connections to any other host shall be rejected. The proxy shall only permit the git SSH commands `git-upload-pack` and `git-receive-pack`; all other commands shall be rejected. Port forwarding (`-L`, `-R`, `-D`), remote exec, environment variable passing, and PTY allocation shall be disabled. On receiving an allowed git SSH operation, authenticate with the SSH private key mounted at `/secrets/ssh-key`, open a connection to the configured git remote, and proxy the operation through. The agent's `GIT_SSH_COMMAND` points to a wrapper script that connects to `localhost:9091`.
- FR-19: Egress logger (`:9092`): transparent HTTP/HTTPS CONNECT proxy that logs every outbound connection (timestamp, destination host:port, method, bytes sent, bytes received) to stdout in JSON-lines format
- FR-20: Egress logger shall NOT block or filter any traffic (agents need open internet for tool installs, documentation, etc.)
- FR-21: The sidecar shall read credentials from files mounted into its container only. The sidecar shall re-read credential files from disk on each request (not cache at startup) so that K8s Secret volume updates propagate without restart. Secret file layout: `/secrets/model-credentials/openai` (contains opencode auth data). The K8s Secret `nemo-creds-{engineer}` is a single secret per engineer with keys `claude`, `openai`, and `ssh`. The sidecar mounts only the `openai` key at `/secrets/model-credentials/openai` for the model proxy. **Exception to "no credentials in agent container":** The `claude` key is mounted into the agent container at `/work/home/.claude/` for IMPLEMENT/REVISE stages only (see FR-25b). Claude Code reads its own session auth; the sidecar does not proxy Claude API calls. SSH key layout: `/secrets/ssh-key/` mounted from the **same** `nemo-creds-{engineer}` Secret's `ssh` key (using `items` projection: `{ key: "ssh", path: "id_ed25519" }`), producing `/secrets/ssh-key/id_ed25519` (PEM private key, mode 0600, mounted via `defaultMode: 0600` on the Secret volume). The SSH key is uploaded by the engineer via `nemo auth --ssh` (reads from `~/.ssh/id_ed25519` or `[identity] ssh_key_path` in `~/.nemo/config.toml`).
- FR-22: On startup, the sidecar shall wait until all three ports are listening, then write a readiness file to `/tmp/shared/ready` (shared emptyDir volume) AND expose a K8s readiness probe on `:9093/healthz` (for kubelet) AND expose a K8s liveness probe on `:9093/healthz` (same endpoint, different K8s probe config with `initialDelaySeconds: 5, periodSeconds: 10`). The readiness file is the mechanism the agent entrypoint polls; the HTTP probes are for K8s. If the sidecar dies mid-job, the agent will encounter connection refused on localhost proxy ports; the entrypoint shall detect this and exit with code 111 (sidecar connection failure) so the control plane can distinguish sidecar crashes from agent failures.
- FR-23: On SIGTERM, the sidecar shall drain active connections (5s grace) then exit

#### Stage Name Mapping

Job names, API query parameters, log labels, and prompt template filenames use **short stage names**: `implement`, `test`, `review`, `audit`, `revise`. The Postgres `loop_stage` enum stores **full names**: `implementing`, `testing`, `reviewing`, `spec_audit`, `spec_revise`. Full mapping:

| Short name (jobs, API, logs) | DB enum value | Prompt template filename |
|------------------------------|---------------|--------------------------|
| `implement` | `implementing` | `implement.md` |
| `test` | `testing` | `test.md` |
| `review` | `reviewing` | `review.md` |
| `audit` | `spec_audit` | `spec-audit.md` |
| `revise` | `spec_revise` | `spec-revise.md` |

Short names (`audit`, `revise`) are used everywhere except the DB enum. The `spec_` prefix appears ONLY in Postgres `loop_stage` enum values. Config references, job names, API parameters, and log labels always use `audit`/`revise` (no `spec_` prefix).

#### K8s Job Template

- FR-24: Each agent job shall be a K8s Job with `restartPolicy: Never` and two containers: `agent` and `auth-sidecar`. If the `nemo-registry-creds` Secret exists (created by Terraform when `image_pull_secret_dockerconfigjson` is provided, see FR-52), the Job template shall include `imagePullSecrets: [{ name: nemo-registry-creds }]`. Otherwise, no `imagePullSecrets` (public images or pre-pulled).
- FR-25: The agent container shall mount: worktree volume (from bare repo PVC, path `/work`; mounted read-only for REVIEW and AUDIT stages), session state PVC (path `/sessions`), output volume (emptyDir, path `/output`), shared readiness volume (emptyDir, path `/tmp/shared`), a writable tmpdir (emptyDir, path `/tmp`), and a writable home directory (emptyDir, path `/work/home`). **No separate `/specs` mount.** All stage config (`nemo.toml`, `.nemo/prompts/`) is read from the worktree at `/work/`. The worktree IS the repo at the correct branch/SHA, so `/work/nemo.toml` and `/work/.nemo/prompts/{stage}.md` are always available. The agent container shall set `securityContext: { runAsNonRoot: true, runAsUser: 1000, readOnlyRootFilesystem: true }`. Writable paths (all emptyDir or PVC mounts): `/work`, `/work/home`, `/output`, `/sessions`, `/tmp`, `/tmp/shared`. Environment variables for writable paths: `HOME=/work/home`, `XDG_CONFIG_HOME=/work/home/.config`, `XDG_CACHE_HOME=/work/home/.cache`, `TMPDIR=/tmp`. `/work/home` is needed because claude-code writes to `$HOME/.claude/` and opencode writes to `$HOME/.config/opencode/`.
- FR-25b: For IMPLEMENT and REVISE stages (claude-code), the agent container shall additionally mount the `~/.claude/` session directory from the engineer's K8s Secret (`nemo-creds-{engineer}`, key `claude`) at `/work/home/.claude/`. This allows Claude Code to read its own session auth directly. The mount is read-only. This is the ONLY credential mounted into the agent container; all other credentials remain sidecar-only. For REVIEW and AUDIT stages (opencode), this mount is not present (opencode uses the sidecar proxy on :9090 for OpenAI auth).
- FR-26: The sidecar container shall mount: model credentials Secret (path `/secrets/model-credentials`), SSH key Secret (path `/secrets/ssh-key`), shared readiness volume (emptyDir, path `/tmp/shared`). The Secret volumes shall NOT be mounted in the agent container.
- FR-27: The Job shall set these environment variables on the agent container: `STAGE`, `SPEC_PATH`, `FEEDBACK_PATH`, `SESSION_ID`, `BRANCH`, `SHA`, `MODEL`, `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL`, `ROUND`, `MAX_ROUNDS`, `LOOP_ID`, `HOME=/work/home`, `XDG_CONFIG_HOME=/work/home/.config`, `XDG_CACHE_HOME=/work/home/.cache`, `TMPDIR=/tmp`. Note: `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` must match `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` (both set from the engineer's identity) to ensure commits are fully attributed to the engineer. The job builder reads the engineer's name and email from the `engineers` table (populated by `nemo auth` via `POST /credentials`, which reads from `~/.nemo/config.toml` `[identity]` section).
- FR-28: Resource limits per job type:

| Container / Job type | CPU request | CPU limit | RAM request | RAM limit |
|----------------------|------------|-----------|-------------|-----------|
| IMPLEMENT | 250m | 500m | 1Gi | 2Gi |
| REVIEW | 250m | 500m | 1Gi | 2Gi |
| AUDIT | 250m | 500m | 1Gi | 2Gi |
| REVISE | 250m | 500m | 1Gi | 2Gi |
| TEST (default) | 500m | 1000m | 1Gi | 3Gi |
| TEST (jvm tag) | 1000m | 2000m | 2Gi | 6Gi |
| auth-sidecar (all) | 50m | 100m | 64Mi | 128Mi |

- FR-29: Jobs shall have `activeDeadlineSeconds: 900` (15 min) as a watchdog. The control plane may override this per stage.
- FR-30: The sidecar shall write a readiness file to `/tmp/shared/ready` (shared emptyDir volume). The agent entrypoint polls this file (100ms interval, 30s timeout). `shareProcessNamespace` shall NOT be used (it leaks `/proc` across containers).
- FR-31: Job names shall follow the pattern `nemo-{loop_id_short}-{stage}-r{round}-t{attempt}` where `loop_id_short` is the first 8 characters of the loop ID and `attempt` is the retry attempt number (starting at 1), to stay under the K8s 63-character name limit (e.g., `nemo-a3f2b1c9-implement-r2-t1`)
- FR-32: Jobs shall have labels: `nemo.dev/loop-id`, `nemo.dev/stage`, `nemo.dev/engineer`, `nemo.dev/round` for control plane queries

#### Prompt Templates

- FR-33: Default prompt templates shall ship as files embedded in the control plane binary and written to a ConfigMap on deploy
- FR-34: Repo-side overrides shall live in `.nemo/prompts/` within the repo (accessible at `/work/.nemo/prompts/` in the worktree) and take precedence over defaults when present
- FR-35: `implement.md` template shall include: role definition (implementer), spec contents (injected), branch/SHA context, instruction to commit changes using **conventional commit format** (`feat(scope): description` or `fix(scope): description`, where scope is the affected service or module), explicit prohibition of mock/placeholder implementations ("You must implement all functionality fully. Mock implementations, placeholder functions, TODO stubs, and fake data stores are forbidden. Every code path must be real and complete."), and (if round > 1) prior review feedback in the feedback file format (see FR-40b). The template MUST specify: "All commits must use conventional commit format: `feat(scope): description` or `fix(scope): description`. The repo enforces this via a commit hook."
- FR-36: `review.md` template shall include: role definition (adversarial reviewer), spec contents (injected), diff context (`git diff $BASE...$SHA`), the verdict JSON schema (inline), instruction to output valid JSON matching the schema, and instruction to check for: correctness vs spec, edge cases, error handling, test coverage gaps
- FR-37: `spec-audit.md` template shall include: role definition (spec auditor), spec contents (injected), instruction to check for: ambiguity, missing edge cases, untestable requirements, unresolved dependencies, feasibility concerns, contradiction with existing codebase patterns
- FR-38: `spec-revise.md` template shall include: role definition (spec author/reviser), spec contents (injected), audit findings (injected), instruction to revise the spec addressing each finding without removing existing valid requirements, instruction to commit changes using **conventional commit format** (`feat(scope): description` or `fix(scope): description`). The template MUST specify: "All commits must use conventional commit format."
- FR-39: Templates shall use `{{PLACEHOLDER}}` syntax for variable injection: `{{SPEC}}`, `{{DIFF}}`, `{{FEEDBACK}}`, `{{BRANCH}}`, `{{SHA}}`, `{{VERDICT_SCHEMA}}`, `{{AFFECTED_SERVICES}}`
- FR-40: The review verdict JSON schema (embedded in `review.md` and `spec-audit.md`) shall match the schema defined in Lane A (Review Verdict Schema / Audit Verdict Schema sections): `{ clean: bool, confidence: float, issues: [{ severity, category?, file, line, description, suggestion }], summary: string, token_usage: { input, output } }`. The `category` field on each issue is optional (not all reviewers produce categories); when present it is one of `correctness`, `security`, `performance`, `style` (for reviews) or `completeness`, `clarity`, `correctness`, `consistency` (for audits), matching Lane A's verdict schemas.
- FR-40b: The feedback file is a first-class contract between stages. The control plane SHALL validate feedback files before dispatching the next stage. **The feedback file schema is defined in Lane A (Feedback File Schema section) and is the single source of truth.** The format is `{ round, source, issues|failures }` where `source` is `"review"`, `"audit"`, or `"test"`. When source is `"review"` or `"audit"`, the file contains an `issues` array (from the verdict). When source is `"test"`, the file contains a `failures` array (from test results). Example (review feedback):

  ```json
  {
    "round": 2,
    "source": "review",
    "issues": [
      { "severity": "high", "category": "correctness", "file": "api/src/invoice.rs", "line": 42, "description": "...", "suggestion": "..." }
    ]
  }
  ```

  Example (test feedback):

  ```json
  {
    "round": 2,
    "source": "test",
    "failures": [
      { "service": "api", "test_command": "cargo test -p api", "test_name": "...", "exit_code": 101, "stdout": "...", "stderr": "..." }
    ]
  }
  ```

  The feedback file is written by the control plane (not the agent) to `$FEEDBACK_PATH` before dispatching the next round.

#### Network Egress Enforcement

- FR-41a: K8s NetworkPolicy cannot distinguish between containers in the same pod (it operates at pod level). Therefore, egress enforcement uses an init container with iptables/ip6tables rules instead. The Job template shall include an init container (`init-iptables`) that runs with `securityContext: { capabilities: { add: ["NET_ADMIN"] } }` and configures rules to drop non-proxied egress from the agent container (UID 1000). The init container runs, sets up rules, then exits. The agent container inherits the network namespace with the rules applied. **V1 approach:** iptables REDIRECT to an HTTP CONNECT proxy does not work for raw TCP, so V1 does NOT redirect TCP. Instead, V1 relies on HTTP_PROXY env vars (FR-8) for logging outbound HTTP/HTTPS and uses iptables only for defense-in-depth (drop UDP/ICMP, disable IPv6). The exact commands:

  ```
  # --- IPv6: disable entirely in V1 ---
  sysctl -w net.ipv6.conf.all.disable_ipv6=1
  sysctl -w net.ipv6.conf.default.disable_ipv6=1
  sysctl -w net.ipv6.conf.lo.disable_ipv6=1

  # --- IPv4: defense-in-depth rules ---
  # Allow loopback (agent -> sidecar on localhost)
  iptables -A OUTPUT -o lo -j ACCEPT
  # Allow established connections
  iptables -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
  # Allow TCP from UID 1000 (agent) — HTTP_PROXY handles logging, not iptables
  iptables -A OUTPUT -p tcp -m owner --uid-owner 1000 -j ACCEPT
  # Drop all non-TCP egress from UID 1000 (no UDP/ICMP exfiltration)
  iptables -A OUTPUT -p udp -m owner --uid-owner 1000 -j DROP
  iptables -A OUTPUT -p icmp -m owner --uid-owner 1000 -j DROP
  ```

  The sidecar runs as a different UID (UID 65534/nobody) so its own egress is not affected by UID-based rules.

  **V2 upgrade path:** Replace HTTP_PROXY-based logging with TPROXY (`iptables -t mangle -A OUTPUT -p tcp -m owner --uid-owner 1000 -j TPROXY --on-port 9092`) and make the sidecar a TPROXY-compatible transparent proxy that handles both HTTP and raw TCP. This captures all TCP, not just HTTP_PROXY-respecting tools.

- FR-41b: DNS resolution: the agent container's UDP DNS queries (port 53) are dropped by the iptables rules above. All DNS resolution happens through the sidecar's HTTP CONNECT proxy (the agent uses `HTTPS_PROXY` for all outbound HTTP/HTTPS, and the proxy resolves DNS on the agent's behalf). The sidecar itself can resolve DNS normally (different UID, not subject to the UID-based rules). **Note:** Tools that bypass HTTP_PROXY and make raw TCP DNS queries will fail (UDP is dropped, and there is no iptables TCP redirect to a DNS resolver). In practice, all standard tools respect HTTP_PROXY which handles DNS via the CONNECT proxy.

- FR-41c: IPv6 is disabled entirely in V1 via sysctl in the init container (see FR-41a). This prevents IPv6 bypass of the IPv4 iptables rules and eliminates the need for mirrored ip6tables rules. IPv6 private ranges (`fc00::/7`, `fe80::/10`, `::1`) are blocked implicitly. The sidecar SSRF protection (FR-15) additionally blocks these IPv6 ranges in case IPv6 is enabled in V2.

#### TEST Stage

- FR-42a: For the TEST stage, the control plane is the sole source of truth for affected services. The control plane computes affected services by running `git diff --name-only $BASE...$SHA` and mapping changed file paths against `[services.*.path]` in `nemo.toml`. The control plane passes the result as the `AFFECTED_SERVICES` environment variable (JSON array of service names) on the Job. The agent does NOT self-report affected services; there are no agent-reported service fields anywhere in the system. **Cross-reference note:** Design doc line 121-122 references agent-reported `affected_services` in implement job output. This is superseded by the control plane computing `affected_services` from git diff (Lane B/C decision from adversarial review). Lane A `ImplOutput` updated to remove `affected_services` field.
- FR-42b: The entrypoint shall look up the test command for each affected service from `nemo.toml` (located at `/work/nemo.toml` in the worktree), under the `[services.<name>.test]` section. Since `nemo.toml` is TOML (not JSON), the entrypoint uses `tomlq` to convert to JSON before extracting fields with `jq`. Note: `nemo.toml` is read from the worktree (`/work/`), NOT from a ConfigMap or `/specs/` mount. The worktree IS the repo at the correct branch/SHA.
- FR-42c: The entrypoint shall run each test command, capture exit code, stdout, and stderr per service
- FR-42d: The entrypoint shall write structured test results to `/output/result.json` and stdout (with `NEMO_RESULT:` prefix per FR-13) using the common result envelope with stage `"test"` and data: `{ services: [{ name, test_command, exit_code, stdout, stderr }], all_passed: bool, ci_status: "passed|failed|unknown", token_usage }`. The `ci_status` field uses a three-state model: `passed` (all tests exit 0), `failed` (at least one test exit non-zero with test output), `unknown` (test harness itself failed, e.g., command not found, timeout, OOM — cannot determine test results). The control plane uses this to distinguish "code is broken" from "test infrastructure is broken".

#### Terraform Module

- FR-43: The module shall provision a Hetzner Cloud server (default type: `ccx43`, configurable via `server_type` variable)
- FR-44: The module shall install k3s v1.30+ (pinned in Terraform variable `k3s_version`, default `v1.30.4+k3s1`) on the provisioned server with `--disable traefik` (use nginx-ingress instead for TLS support). Pinned component versions (all configurable via Terraform variables with these defaults): nginx-ingress v1.10+, cert-manager v1.14+, postgres:16-alpine.
- FR-45: The module shall deploy Postgres (image: `postgres:16-alpine`) as a k3s pod with a 20Gi PVC (hostPath on single-node)
- FR-46: The module shall deploy the Nemo control plane as TWO separate k3s Deployments, matching the Lane A architecture: (1) `nemo-api-server` Deployment (1 replica, runs the API server binary on `:8080`), and (2) `nemo-loop-engine` Deployment (1 replica, runs the loop engine binary). Both share the same Postgres database. Each gets its own ServiceAccount. V1 is single-replica per deployment; K8s restarts on crash. V2: add leader election via K8s Lease API for HA with 2+ replicas of the loop engine.
- FR-46b: The module shall create RBAC resources (ClusterRole or namespaced Roles + RoleBindings) for the loop engine ServiceAccount in the `nemo-jobs` namespace:

  | Resource | Verbs | Purpose |
  |----------|-------|---------|
  | `batch/v1/Jobs` | create, delete, list, watch, get | Dispatch and manage agent Jobs |
  | `v1/Pods` | list, get | Inspect pod status and exit codes |
  | `v1/Pods/log` | get | Read pod logs for `NEMO_RESULT:` parsing |
  | `v1/Secrets` | create, update, get | Per-engineer credentials (`nemo-creds-{name}`) |
  | `v1/ConfigMaps` | create, update, get | Default prompt templates, cluster config |
  | `v1/PersistentVolumeClaims` | get, list | Access bare repo and session PVCs |

  The API server ServiceAccount needs only Secrets (create, update, get) in `nemo-jobs` for `nemo auth` credential writes.
- FR-47: The module shall create a 100Gi PVC for the shared bare repo. An init Job (`nemo-repo-init`) shall run on first deploy to initialize the bare repo. The init Job:

  ```yaml
  apiVersion: batch/v1
  kind: Job
  metadata:
    name: nemo-repo-init
    namespace: nemo-system
  spec:
    template:
      spec:
        containers:
        - name: repo-init
          image: alpine/git:latest
          command: ["/bin/sh", "-c"]
          args:
          - |
            set -e
            if [ ! -d /bare-repo/HEAD ]; then
              git init --bare /bare-repo
            fi
            git -C /bare-repo remote remove origin 2>/dev/null || true
            git -C /bare-repo remote add origin "$GIT_REPO_URL"
            mkdir -p /root/.ssh
            cp /secrets/ssh-key/id_ed25519 /root/.ssh/id_ed25519
            chmod 600 /root/.ssh/id_ed25519
            cp /secrets/ssh-known-hosts/known_hosts /root/.ssh/known_hosts
            git -C /bare-repo fetch --all
          env:
          - name: GIT_REPO_URL
            valueFrom:
              configMapKeyRef:
                name: nemo-cluster-config
                key: git_repo_url
          volumeMounts:
          - name: bare-repo
            mountPath: /bare-repo
          - name: ssh-key
            mountPath: /secrets/ssh-key
            readOnly: true
          - name: ssh-known-hosts
            mountPath: /secrets/ssh-known-hosts
            readOnly: true
        volumes:
        - name: bare-repo
          persistentVolumeClaim:
            claimName: nemo-bare-repo
        - name: ssh-key
          secret:
            secretName: nemo-repo-ssh-key
            defaultMode: 0600
        - name: ssh-known-hosts
          configMap:
            name: nemo-ssh-known-hosts
        restartPolicy: OnFailure
    backoffLimit: 3
  ```

  Terraform also creates a ConfigMap `nemo-cluster-config` with keys: `git_repo_url`, `domain`. This ConfigMap is referenced by Jobs via `valueFrom.configMapKeyRef` instead of Terraform string interpolation in YAML values.

  **The authoritative fetch is per-job**, executed by the control plane's `prepare_worktree()` before each job dispatch (see Lane B FR-10). There is NO fetch CronJob in V1. The per-job fetch ensures the worktree always reflects the latest remote state at dispatch time without stale-cache risk.

- FR-47b: The module shall create a 10Gi PVC (`nemo-sessions`) for session state persistence across rounds. Mounted into agent pods at `/sessions`. Used for session continuation (`--resume`): claude-code and opencode session files persist here so that round N+1 can resume the session from round N. The PVC is shared across all agent jobs (same as the bare repo PVC pattern).

- FR-48: The module shall configure nginx-ingress with Let's Encrypt TLS via cert-manager. Prerequisites: the user must pre-create a DNS A record pointing `domain` to the server IP. Terraform inputs for TLS: `acme_email` (required), `ingress_class` (default `nginx`). cert-manager uses HTTP-01 challenge by default (requires port 80 open). The ClusterIssuer resource is created by Terraform.
- FR-49: The module shall create a K8s Namespace `nemo-system` for control plane components and `nemo-jobs` for agent jobs
- FR-50: The module shall create the `nemo-jobs` namespace. RBAC is defined in FR-46b. Per-engineer secrets (SSH key + model credentials) are NOT created by Terraform; they are created by `nemo auth` via the control plane API (`POST /credentials`) at runtime. `nemo auth` also registers the engineer's identity (name + email from `~/.nemo/config.toml` `[identity]`) in the `engineers` table. Secret naming convention: `nemo-creds-{engineer-name}`. Each secret contains keys: `claude`, `openai`, `ssh`.
- FR-51: Required input variables: `hetzner_api_token`, `domain`, `git_repo_url`, `ssh_public_keys` (for server access), `acme_email` (for Let's Encrypt), `ssh_known_hosts` (string containing known_hosts entries for the git remote; user provides via `ssh-keyscan github.com > known_hosts` or equivalent), `git_host_token` (GitHub PAT with repo + PR permissions for PR creation/merge in ship mode, stored in `nemo-git-host-token` K8s Secret per FR-52b). **Prerequisite:** User must create a DNS A record pointing `domain` to the Hetzner server IP BEFORE running `terraform apply`. Terraform does not manage DNS records (DNS providers vary). cert-manager HTTP-01 challenge will fail without the A record.
- FR-51b: The module shall create a ConfigMap `nemo-ssh-known-hosts` from the `ssh_known_hosts` input variable. This ConfigMap is mounted into the repo-init Job (FR-47) and agent Job sidecars (FR-18 git proxy). If `ssh_known_hosts` is not provided, Terraform shall run a `null_resource` provisioner that executes `ssh-keyscan` against the git remote host (extracted from `git_repo_url`) and populates the ConfigMap. The `null_resource` approach is a convenience fallback; providing `ssh_known_hosts` explicitly is preferred for reproducibility.
- FR-52: Optional input variables: `server_type` (default `ccx43`), `server_location` (default `fsn1`), `node_count` (default `1`, for future multi-node support), `postgres_password`, `control_plane_image`, `agent_base_image`, `k3s_version` (default `v1.30.4+k3s1`), `nginx_ingress_version` (default `v1.10.0`), `cert_manager_version` (default `v1.14.0`), `ingress_class` (default `nginx`), `image_pull_secret_dockerconfigjson` (default `null`; if provided, Terraform creates a `kubernetes.io/dockerconfigjson` Secret named `nemo-registry-creds` in `nemo-jobs` namespace, and Job templates reference it in `imagePullSecrets`)
- FR-52b: The module shall provision two cluster-level K8s Secrets in the `nemo-system` namespace for control plane auth and git host integration:
  - `nemo-api-key`: Contains a generated API key (`NEMO_API_KEY`) used by the CLI to authenticate against the control plane API. Terraform generates a random 32-byte hex token and stores it in this Secret. The API server reads this on startup.
  - `nemo-git-host-token`: Contains a GitHub PAT (`GIT_HOST_TOKEN`) for PR creation and merge operations. Provided via Terraform input variable `git_host_token` (required). The control plane reads this on startup to interact with the git host API (create PRs, merge PRs in ship mode, check CI status).

  Both Secrets are mounted into the control plane Deployments (API server and loop engine). The corresponding `cluster_credentials` rows in Postgres (see Lane B FR-4c) are seeded by a post-migration init step referencing these Secret names.
- FR-52c: The module shall generate a random 32-byte hex API key during `terraform apply` and store it in the `nemo-api-key` K8s Secret. Terraform outputs the key (`api_key`, marked sensitive) so the engineer can configure their CLI: `nemo config --set api_key <key-from-terraform-output>`. This solves the bootstrap chicken-and-egg: the cluster provisions its own auth key, and the engineer retrieves it from Terraform output.
- FR-53: Outputs: `control_plane_url`, `kubeconfig` (sensitive), `server_ip`, `namespace_jobs`, `namespace_system`, `api_key` (sensitive)
- FR-54: The module shall configure k3s container log rotation: 50MB max per container, 5 files retained
- FR-55: The module shall deploy a CronJob that runs `pg_dump` daily, writing backups to `/data/backups/` on the host (hostPath volume). Backups are retained for 7 days; the CronJob deletes files older than 7 days before writing the new backup.
- FR-56: The module shall create a K8s Service named `nemo-postgres` on port 5432 exposing the Postgres pod. The Postgres password shall be stored in a K8s Secret (`nemo-postgres-credentials`). Control plane Deployments shall construct `DATABASE_URL` using K8s-native env var composition (no shell interpolation):

  ```yaml
  env:
  - name: POSTGRES_PASSWORD
    valueFrom:
      secretKeyRef:
        name: nemo-postgres-credentials
        key: password
  - name: DATABASE_URL
    value: "postgres://nemo:$(POSTGRES_PASSWORD)@nemo-postgres:5432/nemo"
  ```

  K8s resolves `$(POSTGRES_PASSWORD)` via the dependent env var syntax (not shell expansion). The Postgres pod receives `POSTGRES_PASSWORD` via the same Secret.

### Non-Functional Requirements

- NFR-1: Base agent image size shall be under 2 GB (compressed). Minimize layers; use multi-stage build for tool installation.
- NFR-2: Auth sidecar binary shall be under 15 MB (static, no runtime dependencies)
- NFR-3: Auth sidecar startup to ready shall be under 2 seconds
- NFR-4: Agent job startup (image pull excluded, from pod scheduled to entrypoint running) shall be under 10 seconds
- NFR-5: Under a load of 10 concurrent connections with 1MB/sec throughput, the egress logger shall add less than 5ms p99 latency to proxied requests
- NFR-6: `terraform apply` on a clean state shall complete in under 10 minutes
- NFR-7: Model API proxy shall not buffer request/response bodies (stream through) to support streaming model responses
- NFR-8: All sidecar logs shall be structured JSON (parseable by k3s log collection)
- NFR-9: Terraform state shall be stored locally (no remote backend for V1). The `kubeconfig` output shall be marked sensitive.
- NFR-10: _(Reserved for future use.)_ **V1 has no git fetch CronJob.** All fetches are per-job via `prepare_worktree()`. A background fetch CronJob is a potential V2+ optimization for cache warmth on high-traffic clusters, but is not required for correctness.

## Behavior

### Worktree Lifecycle (Control Plane Responsibility)

The control plane owns the full lifecycle of git worktrees. Before creating a K8s Job, the control plane creates the worktree (via the git module, holding the worktree mutex) at a path under the bare repo PVC. The Job's pod mounts this pre-created worktree path as `/work`. After the Job completes (success or failure), the control plane deletes the worktree (again holding the mutex). The agent never creates or deletes worktrees.

### Normal Flow: Agent Job Lifecycle

1. Control plane creates the worktree (see above), then creates a K8s Job from the template, substituting environment variables and volume mounts for the specific loop/stage/round
2. K8s schedules the pod. Both containers start. Sidecar begins listening on :9090, :9091, :9092, writes `/tmp/shared/ready` to the shared emptyDir volume
3. Agent entrypoint polls for `/tmp/shared/ready` (100ms interval, 30s timeout)
4. Entrypoint reads `$STAGE`, loads the prompt template from `/work/.nemo/prompts/{stage}.md` (repo override, read from the worktree) or falls back to `/etc/nemo/prompts/{stage}.md` (default)
5. Entrypoint injects variables into template (spec content, feedback, branch, SHA, etc.)
6. Entrypoint invokes the CLI tool (claude or opencode) with the assembled prompt
7. CLI tool streams model API calls through :9090 (auth injection), makes outbound HTTP calls through :9092 (egress logging), performs git operations through :9091 (SSH proxy)
8. CLI tool completes. Entrypoint parses output, writes result JSON to `/output/result.json` AND emits a single line `NEMO_RESULT:{...}` to stdout (per FR-13)
9. Agent container exits 0. Sidecar receives SIGTERM, drains, exits.
10. Control plane watches for Job completion, reads result from pod logs (the durable channel) BEFORE deleting the Job. Pod logs are authoritative; `/output/result.json` is for the agent's own use during execution.
11. Control plane deletes the Job and associated resources, then deletes the worktree (see Worktree Lifecycle above)

### Session Continuation Flow (Round > 1)

1. Control plane sets `SESSION_ID` to the session ID from the previous round's `NEMO_RESULT:` output (parsed from pod logs)
2. Control plane writes the feedback file (validated against FR-40b schema) to the session PVC and sets `FEEDBACK_PATH` to its path
3. Entrypoint detects `$SESSION_ID` is set, passes `--resume $SESSION_ID` (claude) or `-s $SESSION_ID` (opencode)
4. The session PVC persists session state across Job instances for the same loop

### Dockerfile.nemo Extension Flow

1. Team creates `Dockerfile.nemo` in monorepo root: `FROM ghcr.io/nemo/agent-base:latest` + project-specific toolchain installs
2. Team builds and pushes to their registry: `docker build -f Dockerfile.nemo -t registry/nemo-agent-myrepo:latest .`
3. Team sets `agent_base_image` terraform variable (or `nemo.toml` `[image]` section) to the custom image tag
4. Control plane uses the custom image for all agent jobs in that cluster

## Edge Cases

| Scenario | Expected Behavior |
|----------|-------------------|
| Sidecar fails to start within 30s | Agent entrypoint exits 1 with error "sidecar readiness timeout". Job fails. Control plane retries per failure handling policy. |
| Model API returns 429 (rate limit) | Sidecar passes the 429 through. CLI tool handles retry internally (both claude-code and opencode have built-in retry). |
| Model API returns 401 (bad credentials) | Sidecar passes the 401 through. CLI tool exits non-zero (exit code 42 for auth failure). Job fails. Control plane detects 401 / exit code 42 and transitions loop to AWAITING_REAUTH (not FAILED), preserving the current stage in `reauth_from_state`. Engineer runs `nemo auth` to refresh credentials, then loop resumes from where it left off. |
| SSH key rejected on git push | Git push proxy returns the SSH error. Entrypoint logs the error, exits non-zero. Control plane marks loop FAILED with "git auth failure". |
| Agent container OOM-killed | K8s marks container as OOMKilled. Job fails. Control plane retries with backoff (30s, 120s). On 3rd failure, loop FAILED. |
| Egress logger port conflict | Sidecar logs error and exits. Pod restart backoff applies. Should not happen in practice (ports are hardcoded localhost-only). |
| Session PVC full | CLI tool fails to write session state. Job exits non-zero. Control plane should alert engineer. Manual cleanup required for V1. |
| Worktree volume not mounted (bare repo PVC missing) | Agent entrypoint checks for `/work` mount, exits 1 with "worktree volume not found". Job fails immediately. |
| Job exceeds activeDeadlineSeconds | K8s terminates the pod. Control plane detects DeadlineExceeded condition, treats as timeout. Deadline-exceeded jobs retry per the unified per-stage retry budget (default 2 retries), matching Lane A's retry model. When retries are exhausted, loop transitions to FAILED. |
| Template variable not set (e.g., missing SPEC_PATH) | Entrypoint validates all required env vars on startup, exits 1 with list of missing vars. Fail-fast before invoking any CLI tool. |
| Repo .nemo/prompts/ has partial overrides | Entrypoint loads per-template: if `/work/.nemo/prompts/implement.md` exists, use it; otherwise fall back to `/etc/nemo/prompts/implement.md` (default). Each template resolved independently. |
| Terraform apply with existing server | Hetzner provider detects existing server by name, updates in place or recreates if server_type changed. Standard Terraform behavior. |
| Concurrent git fetch and worktree creation | Not a conflict. `git fetch` updates the bare repo refs. `git worktree add` creates a new worktree from a ref. The control plane mutex serializes worktree create/delete, not fetch. |
| Multiple engineers with same model provider | Each engineer's credentials stored in separate K8s Secrets (`nemo-creds-{engineer-name}`). Job mounts only the submitting engineer's Secret. |
| ImagePullBackOff | K8s cannot pull agent or sidecar image (bad credentials, missing tag, registry down). Job stays pending. Control plane detects ImagePullBackOff condition after 60s, marks loop FAILED with "image pull failure", notifies engineer. |
| Credential rotation during running jobs | `nemo auth` warns if engineer has running jobs. Sidecar reads credentials from mounted file on each request (not cached at startup), so K8s Secret volume updates propagate automatically. |

## Error Handling

| Error | Detection | Response | Recovery |
|-------|-----------|----------|----------|
| Sidecar crash mid-job | Agent gets connection refused on proxy ports; liveness probe fails | Agent entrypoint detects localhost connection failures and exits with code 111 (sidecar failure) | Control plane sees exit code 111, retries job (new pod, fresh sidecar) |
| Malformed NEMO_RESULT line | Control plane JSON parse fails on `NEMO_RESULT:` prefix line | Log raw output, mark job ERRORED | Control plane retries up to 2 times (matching Lane A's malformed verdict retry policy). If still malformed after 2 retries, loop FAILED. |
| Terraform apply partial failure | Terraform exits non-zero with state file | Resources may be partially created | `terraform apply` is idempotent; re-run. `terraform destroy` to clean up. |
| k3s API unreachable from control plane | Job creation fails with connection error | Control plane retries with 10s backoff, max 3 attempts | If persistent, alert (k3s down or network issue) |
| Postgres PVC full | Postgres pod restarts with disk pressure | Control plane health check detects DB connection failure | Manual: expand PVC or clean old data |
| cert-manager fails TLS | Ingress serves self-signed cert | Control plane still reachable (CLI can skip TLS verify for V1) | Check DNS, cert-manager logs. Re-run terraform apply. |
| Agent writes to bare repo directly (bug) | Should not happen (push goes through sidecar proxy, which pushes to remote) | If detected, per-job fetch in `prepare_worktree()` self-heals by resetting to remote state | Fix the bug in entrypoint |

## Out of Scope

- Transparent TCP interception (V1 relies on HTTP_PROXY env vars; raw TCP that bypasses the proxy is not logged. V2: TPROXY-based transparent proxy)
- IPv6 networking (V1 disables IPv6 entirely in the pod via sysctl. V2: enable IPv6 with mirrored ip6tables rules)
- CI/CD pipeline for building agent images (V1 is manual `docker build && docker push`)
- Multi-node k3s (V1 is single-node; `node_count` defaults to 1 with the variable present for future multi-node)
- GPU-backed jobs (all agent work is API-bound, not local inference)
- Custom sidecar configuration per job (V1 sidecar is identical for all jobs)
- Terraform remote state backend (V1 is local state)
- Helm chart packaging (V2)
- GitLab support (V1 is GitHub only via `gh` CLI and GitHub PAT; GitLab deferred to V2)
- mTLS authentication (V1 is API key only; mTLS deferred to V2)
- Automatic credential rotation
- Agent image vulnerability scanning
- Web dashboard (V1 is CLI-only per design doc)

## Acceptance Criteria

- [ ] `docker build` of base agent image succeeds and image size is under 2 GB compressed
- [ ] Running `claude -p "hello" --output-format stream-json` inside the base image produces valid JSON output (with sidecar providing auth)
- [ ] Running `opencode run --format json` inside the base image produces valid JSON output (with sidecar providing auth)
- [ ] Auth sidecar binary starts in under 2s and passes readiness probe on :9093/healthz
- [ ] Auth sidecar injects correct `x-api-key` header for Anthropic API requests proxied through :9090
- [ ] Auth sidecar injects correct `Authorization: Bearer` header for OpenAI API requests proxied through :9090
- [ ] Auth sidecar git push proxy successfully pushes a commit using mounted SSH key (from `nemo-creds-{engineer}` Secret's `ssh` key, projected to `/secrets/ssh-key/id_ed25519`)
- [ ] Egress logger logs all outbound connections with timestamp, host, method, bytes in JSON-lines format to stdout
- [ ] Agent container has no access to files under `/secrets/` (volume not mounted)
- [ ] K8s Job with both containers starts, agent waits for sidecar readiness, executes, emits `NEMO_RESULT:` line to stdout, and exits cleanly
- [ ] Session continuation works: round 2 job with SESSION_ID resumes prior session state from PVC
- [ ] Prompt template variable injection produces correct prompts for all four stages
- [ ] Repo-side `/work/.nemo/prompts/implement.md` overrides the default template when present (read from worktree, not ConfigMap)
- [ ] Review stage produces a verdict JSON file matching the schema (validated with JSON Schema)
- [ ] `terraform init && terraform apply` provisions a working Hetzner server with k3s, Postgres, and control plane in under 10 minutes
- [ ] `terraform output control_plane_url` returns the HTTPS URL of the running control plane
- [ ] `terraform destroy` cleanly removes all resources
- [ ] Job resource limits match the table in FR-28 for each job type
- [ ] Jobs exceeding activeDeadlineSeconds are terminated by K8s
- [ ] iptables init container configures rules; UDP/ICMP from agent container (UID 1000) is dropped; IPv6 is disabled; sidecar (UID 65534) can reach external hosts; agent TCP is allowed (logging is via HTTP_PROXY, not iptables redirect)
- [ ] Agent container runs as non-root (UID 1000) with read-only root filesystem
- [ ] TEST stage reads AFFECTED_SERVICES, runs test commands from `/work/nemo.toml` (worktree), and writes structured results
- [ ] Sidecar re-reads credential files on each request (credential rotation without pod restart)
- [ ] pg_dump CronJob runs daily and writes backup to host directory
- [ ] k3s log rotation configured at 50MB/5 files per container
- [ ] Review stage mounts worktree read-only and opencode runs with `{ "edit": "deny", "bash": "deny", "read": "allow" }`
- [ ] Sidecar model API proxy rejects requests to internal/private IPs (SSRF protection)
- [ ] Sidecar git SSH proxy only allows `git-upload-pack` and `git-receive-pack` to the configured remote host
- [ ] Sidecar liveness probe on :9093/healthz causes K8s to restart sidecar on failure
- [ ] Agent exits with code 111 when sidecar proxy connection fails
- [ ] Feedback file validates against the JSON schema (FR-40b) before the control plane dispatches next stage
- [ ] `NEMO_RESULT:` prefix on stdout result line is correctly parsed by control plane
- [ ] Init Job (`nemo-repo-init`) creates bare repo, configures remote, fetches. No fetch CronJob in V1; all fetches are per-job via `prepare_worktree()`.
- [ ] Integration tests for git module: branch create from correct ref, push before PR, worktree cleanup, concurrent worktree operations against a real git repo
- [ ] Default implement.md template contains the mock/placeholder prohibition directive
- [ ] Postgres backup written to /data/backups/ on host, files older than 7 days cleaned up

## Open Questions

- [x] ~~Claude Code Max subscription auth: does `claude -p` work headless with a session token, or do we need API keys for V1?~~ RESOLVED: Claude Code headless mode (`claude -p --output-format stream-json`) works with Max subscription auth. Credentials mounted from K8s Secret into auth sidecar. Validated during project research phase (2026-03-27). The K8s Secret `nemo-creds-{engineer}` (one secret per engineer) has keys: `claude` (session data), `openai` (opencode auth data), and `ssh` (engineer's SSH private key). Mounted at `/secrets/model-credentials/` in the sidecar for model creds, and at `/secrets/ssh-key/` (projected from the `ssh` key) for the SSH key. The auth sidecar proxies model API calls and injects auth headers; the agent container never sees raw credentials. All three credential types are uploaded via `nemo auth` (which also registers the engineer's name + email in the `engineers` table).
- [x] ~~OpenCode binary availability: is `opencode run --format json` stable in the current release?~~ RESOLVED: OpenCode `opencode run --format json` produces JSON events including message content. The review stage extracts the final message, which must contain the verdict JSON conforming to Lane A's Review Verdict Schema. The review prompt template instructs the model to output ONLY the verdict JSON as its final message. Version: opencode v1.3+. Pin this version in the base image Dockerfile.
- [ ] Session PVC sizing: how large do claude-code and opencode session files get per round? Need to size the PVC appropriately (estimate: 100MB per session, 1Gi PVC per loop should suffice).
