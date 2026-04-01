# Nautiloop Architecture Diagrams

Mermaid diagrams rendered natively by GitHub. Visual companion to [`design.md`](design.md) and [`architecture.md`](architecture.md).

---

## 1. System Architecture Overview

High-level view of all components: the engineer's machine, the k3s cluster (split into `nautiloop-system` and `nautiloop-jobs` namespaces), shared storage, and external services.

```mermaid
graph TD
    CLI["<b>nemo CLI</b><br/>Engineer's Machine<br/>~/.nemo/config.toml<br/>~/.claude/ credentials"]

    CLI -->|"HTTPS (API key)"| API

    subgraph k3s["k3s Cluster (Hetzner CCX43)"]

        subgraph nautiloop-system["Namespace: nautiloop-system"]
            API["<b>API Server</b><br/>axum on :8080<br/>submit, status, logs,<br/>cancel, approve, inspect"]
            PG[("<b>Postgres 16</b><br/>Pod + PVC<br/>loops, jobs, engineers,<br/>egress_logs, log_events")]
            LE["<b>Loop Engine</b><br/>Reconciliation tick 5s<br/>K8s Job watcher (kube-rs)<br/>State machine driver"]
        end

        subgraph nautiloop-jobs["Namespace: nautiloop-jobs"]
            J1["Implement Job<br/>(claude-code)"]
            J2["Test Job<br/>(runner)"]
            J3["Review Job<br/>(opencode)"]
        end

        BareRepo[("Bare Repo PVC<br/>100 Gi<br/>Shared worktree source")]
    end

    API -->|"SQL reads/writes"| PG
    LE -->|"SQL reads/writes"| PG
    LE -->|"kube-rs: create/delete/watch Jobs"| J1
    LE -->|"kube-rs"| J2
    LE -->|"kube-rs"| J3
    BareRepo -.-|"mount as /work"| J1
    BareRepo -.-|"mount as /work"| J2
    BareRepo -.-|"mount as /work"| J3

    J1 -->|"direct (session auth)"| Anthropic3["<b>Anthropic API</b><br/>api.anthropic.com"]
    J3 -->|"via sidecar :9090"| OpenAIAPI["<b>OpenAI API</b><br/>api.openai.com"]
    J1 -->|"via sidecar :9091"| Git["<b>Git Remote</b><br/>GitHub"]
    J3 -->|"via sidecar :9091"| Git

    style k3s fill:#1a1a2e,stroke:#e94560,color:#fff
    style nautiloop-system fill:#16213e,stroke:#0f3460,color:#fff
    style nautiloop-jobs fill:#1a1a2e,stroke:#e94560,color:#fff
    style CLI fill:#0f3460,stroke:#e94560,color:#fff
    style ModelAPIs fill:#533483,stroke:#e94560,color:#fff
    style Git fill:#533483,stroke:#e94560,color:#fff
```

---

## 2. Job Pod Internal Architecture

Each agent job runs as a K8s Job with two containers: the agent (claude-code or opencode) and an auth sidecar. The agent never sees raw credentials. All external traffic routes through the sidecar via localhost.

```mermaid
graph LR
    subgraph Pod["K8s Job Pod: nautiloop-{id}-{stage}-r{round}"]
        direction LR

        subgraph Agent["Agent Container<br/>(claude-code OR opencode)<br/>User 1000, readOnlyRootFilesystem"]
            A_ENV["Env: STAGE, SPEC_PATH,<br/>BRANCH, SHA, MODEL,<br/>OPENAI_BASE_URL=<br/>localhost:9090/openai"]
            A_WORK["/work — Bare Repo PVC<br/>(worktree)"]
            A_SESS["/sessions — Session PVC<br/>(cross-round)"]
            A_SPEC["/specs — ConfigMap/PVC"]
            A_OUT["/output — emptyDir"]
            A_TMP["/tmp — emptyDir"]
        end

        subgraph Sidecar["Auth Sidecar<br/>(Go binary, ~10 MB)"]
            S_MODEL[":9090 Model API proxy"]
            S_GIT[":9091 Git SSH proxy"]
            S_LOG[":9092 Egress logger"]
            S_HEALTH[":9093 /healthz"]
            S_CREDS["/secrets/model-credentials<br/>(K8s Secret)"]
            S_SSH["/secrets/ssh-key<br/>(K8s Secret)"]
        end

        SHARED["/tmp/shared<br/>emptyDir<br/>(readiness signal)"]
    end

    Agent -->|"model API call<br/>localhost:9090"| S_MODEL
    Agent -->|"git push<br/>localhost:9091"| S_GIT
    Agent -->|"any HTTP<br/>localhost:9092"| S_LOG
    Agent ---|"readiness poll"| SHARED
    Sidecar ---|"writes /tmp/shared/ready"| SHARED

    Agent -->|"direct (session auth)<br/>HTTPS"| Anthropic["api.anthropic.com"]
    S_MODEL -->|"inject Bearer<br/>HTTPS"| OpenAI["api.openai.com"]
    S_GIT -->|"inject SSH key"| GitHub["github.com"]
    S_LOG -->|"passthrough + log"| Internet["any host"]

    style Pod fill:#1a1a2e,stroke:#e94560,color:#fff
    style Agent fill:#16213e,stroke:#0f3460,color:#fff
    style Sidecar fill:#0f3460,stroke:#533483,color:#fff
    style SHARED fill:#533483,stroke:#e94560,color:#fff
```

---

## 3. Full Loop Lifecycle

The complete flow from `nemo start --harden` through spec hardening, engineer approval, implementation rounds, and convergence to a PR.

```mermaid
sequenceDiagram
    participant E as Engineer
    participant API as API Server
    participant PG as Postgres
    participant LE as Loop Engine
    participant K8s as K8s / Agent Pods

    E->>API: nemo start --harden spec.md
    API->>PG: INSERT loop (state=PENDING, harden=true)
    API-->>E: 201 {loop_id, branch}

    Note over LE,PG: Reconciliation tick (<=5s)
    LE->>PG: Read PENDING loop

    rect rgb(30, 40, 70)
        Note over LE,K8s: HARDENING PHASE
        loop Harden rounds (until audit clean)
            LE->>PG: stage=SPEC_AUDIT, sub=DISPATCHED
            LE->>K8s: Create Job (audit, openai)
            K8s-->>LE: Job watcher: RUNNING
            LE->>PG: sub=RUNNING
            K8s-->>LE: Job watcher: SUCCEEDED
            LE->>PG: sub=COMPLETED
            Note over LE: Parse audit verdict

            alt verdict.clean == false
                LE->>PG: stage=SPEC_REVISE, sub=DISPATCHED
                LE->>K8s: Create Job (revise, claude)
                K8s-->>LE: SUCCEEDED
                LE->>PG: sub=COMPLETED, round++
            else verdict.clean == true
                Note over LE: Hardening converged
            end
        end
    end

    LE->>PG: state=AWAITING_APPROVAL

    E->>API: nemo approve {loop_id}
    API->>PG: approve_requested=true
    API-->>E: 200 OK

    Note over LE,PG: Reconciliation tick

    rect rgb(40, 30, 60)
        Note over LE,K8s: IMPLEMENTATION PHASE
        loop Implement rounds (until review clean)
            LE->>PG: stage=IMPLEMENTING, sub=DISPATCHED
            LE->>K8s: Create Job (implement, claude)
            K8s-->>LE: SUCCEEDED
            LE->>PG: sub=COMPLETED

            LE->>PG: stage=TESTING, sub=DISPATCHED
            LE->>K8s: Create Job (test)
            K8s-->>LE: SUCCEEDED
            LE->>PG: sub=COMPLETED

            alt tests failed
                Note over LE: Feed test failures to next round
            else tests passed
                LE->>PG: stage=REVIEWING, sub=DISPATCHED
                LE->>K8s: Create Job (review, openai)
                K8s-->>LE: SUCCEEDED
                LE->>PG: sub=COMPLETED

                alt verdict.clean == false
                    Note over LE: Write feedback file, round++
                else verdict.clean == true
                    Note over LE: Create PR
                end
            end
        end
    end

    LE->>PG: state=CONVERGED
    E->>API: nemo status
    API-->>E: CONVERGED, PR #42
```

---

## 4. State Machine

All loop states, sub-states, transitions, and terminal states. Interrupt states (PAUSED, AWAITING_REAUTH, CANCELLED) are reachable from any active state.

```mermaid
stateDiagram-v2
    [*] --> PENDING: submit

    PENDING --> HARDENING: --harden flag
    PENDING --> AWAITING_APPROVAL: no --harden flag

    state HARDENING {
        [*] --> H_DISPATCHED: dispatch audit
        H_DISPATCHED --> H_RUNNING: pod started
        H_RUNNING --> H_COMPLETED: job exits
        H_COMPLETED --> [*]: evaluate
    }

    HARDENING --> HARDENING: audit not clean\n(loop: revise then re-audit)
    HARDENING --> AWAITING_APPROVAL: audit clean
    HARDENING --> HARDENED: nemo harden\n+ audit clean

    AWAITING_APPROVAL --> IMPLEMENTING: approve / auto-approve

    state IMPLEMENTING {
        [*] --> I_DISPATCHED: dispatch impl
        I_DISPATCHED --> I_RUNNING: pod started
        I_RUNNING --> I_COMPLETED: job exits
        I_COMPLETED --> [*]: evaluate
    }

    IMPLEMENTING --> TESTING: impl completed

    state TESTING {
        [*] --> T_DISPATCHED: dispatch tests
        T_DISPATCHED --> T_RUNNING: pod started
        T_RUNNING --> T_COMPLETED: job exits
        T_COMPLETED --> [*]: evaluate
    }

    TESTING --> IMPLEMENTING: tests failed\n(feedback to impl)
    TESTING --> REVIEWING: tests passed

    state REVIEWING {
        [*] --> R_DISPATCHED: dispatch review
        R_DISPATCHED --> R_RUNNING: pod started
        R_RUNNING --> R_COMPLETED: job exits
        R_COMPLETED --> [*]: evaluate
    }

    REVIEWING --> IMPLEMENTING: verdict has issues\n(feedback to impl)
    REVIEWING --> CONVERGED: verdict clean
    REVIEWING --> SHIPPED: verdict clean\n+ ship_mode

    IMPLEMENTING --> FAILED: max rounds exceeded
    REVIEWING --> FAILED: max rounds exceeded

    state "Interrupt States" as interrupts {
        PAUSED: PAUSED\n(branch diverged)
        AWAITING_REAUTH: AWAITING_REAUTH\n(credentials expired)
        CANCELLED: CANCELLED\n(user requested)
    }

    CONVERGED --> [*]
    HARDENED --> [*]
    SHIPPED --> [*]
    FAILED --> [*]
    CANCELLED --> [*]

    PAUSED --> IMPLEMENTING: nemo resume
    PAUSED --> CANCELLED: nemo cancel
    AWAITING_REAUTH --> IMPLEMENTING: nemo auth\n(re-dispatch prev stage)
```

---

## 5. Control Plane Communication

The API Server and Loop Engine share NO direct RPC. Postgres is the only shared medium. The Loop Engine uses a `select!` over a 5s ticker and a K8s Job watcher channel.

```mermaid
graph LR
    subgraph API["API Server (Deployment)"]
        API_W["<b>Writes:</b><br/>INSERT loops (submit)<br/>SET cancel_requested<br/>SET approve_requested<br/>UPDATE engineer_credentials"]
        API_R["<b>Reads:</b><br/>SELECT loops (status)<br/>SELECT jobs (inspect)<br/>SELECT log_events (logs SSE)"]
    end

    subgraph PG["Postgres"]
        LOOPS["<b>loops</b><br/>id, engineer_id, state,<br/>sub_state, round, sha,<br/>cancel_requested,<br/>approve_requested"]
        JOBS["<b>jobs</b><br/>id, loop_id, stage,<br/>round, k8s_job_name,<br/>status, verdict_json"]
        LOGS["<b>log_events</b><br/>id, loop_id, timestamp,<br/>stage, round, line"]
    end

    subgraph LE["Loop Engine (Deployment)"]
        LE_W["<b>Writes:</b><br/>UPDATE loops state/sub_state<br/>INSERT jobs (dispatch)<br/>UPDATE jobs (completion)<br/>INSERT log_events"]
        LE_R["<b>Reads:</b><br/>SELECT non-terminal loops<br/>SELECT cancel/approve flags<br/>SELECT engineer_credentials"]
        RECON["<b>Reconciler</b><br/>select! {<br/>  ticker.tick() =><br/>  job_watcher.recv() =><br/>}"]
    end

    API_W -->|"SQL INSERT/UPDATE"| PG
    API_R -->|"SQL SELECT"| PG
    LE_W -->|"SQL INSERT/UPDATE"| PG
    LE_R -->|"SQL SELECT"| PG

    API_W -.->|"pg_notify('loop_update')<br/>(optimization)"| LE_R

    K8sAPI["K8s API<br/>(kube-rs watcher)"] -->|"Job status change<br/>channel send"| RECON
    RECON -->|"wake reconciler<br/>immediately"| LE_R

    style API fill:#16213e,stroke:#0f3460,color:#fff
    style PG fill:#1a1a2e,stroke:#e94560,color:#fff
    style LE fill:#16213e,stroke:#0f3460,color:#fff
```

---

## 6. Config Resolution

Three config layers merge with increasing priority. CLI flags are the highest-priority override, applied per-request. Engineer values are capped by cluster limits.

```mermaid
graph TD
    CL["<b>Layer 1: Cluster Config</b><br/>(lowest priority)<br/>K8s ConfigMap + env vars<br/>/etc/nautiloop/cluster.toml<br/><i>node_size, provider, domain,<br/>default models, max caps</i>"]

    REPO["<b>Layer 2: Repo Config</b><br/>(team conventions)<br/>nemo.toml in monorepo root<br/>(checked in)<br/><i>models, limits, services,<br/>max_rounds_harden/implement</i>"]

    ENG["<b>Layer 3: Engineer Config</b><br/>(personal preferences)<br/>~/.nemo/config.toml<br/>(not checked in)<br/><i>identity, model overrides,<br/>max_parallel_loops</i>"]

    CLI_FLAGS["<b>CLI Flags</b><br/>(highest priority)<br/>--model-impl, --model-review<br/><i>Applied per-request only</i>"]

    CL -->|"base defaults"| MERGE
    REPO -->|"override scalars,<br/>deep merge services"| MERGE
    ENG -->|"override scalars,<br/>capped by cluster"| MERGE

    MERGE["Merge Algorithm"]

    MERGE --> MC["<b>MergedConfig</b>"]

    CLI_FLAGS -->|"override for<br/>this loop only"| MC

    MC --> R1["implementor_model:<br/>engineer > repo > cluster > ERROR"]
    MC --> R2["reviewer_model:<br/>engineer > repo > cluster > ERROR"]
    MC --> R3["max_parallel_loops:<br/>min(engineer, cluster_cap)"]
    MC --> R4["services:<br/>repo defines; engineer can ADD only"]
    MC --> R5["max_rounds:<br/>repo only (not overridable)"]

    style CL fill:#16213e,stroke:#0f3460,color:#fff
    style REPO fill:#1a1a2e,stroke:#e94560,color:#fff
    style ENG fill:#0f3460,stroke:#533483,color:#fff
    style CLI_FLAGS fill:#533483,stroke:#e94560,color:#fff
    style MC fill:#1a1a2e,stroke:#e94560,color:#fff
```

---

## 7. Git Worktree Lifecycle

The full lifecycle of a worktree: mutex-protected creation, job execution, and mutex-protected cleanup. The mutex serializes all git worktree operations to avoid file lock contention.

```mermaid
sequenceDiagram
    participant LE as Loop Engine
    participant MX as Mutex
    participant BR as Bare Repo (PVC)
    participant WT as Worktree
    participant JOB as Agent Job Pod

    Note over LE,MX: Phase 1: Create worktree

    LE->>MX: acquire mutex
    activate MX

    LE->>BR: git fetch --prune
    LE->>BR: git rev-parse (resolve SHA)
    BR-->>LE: sha = abc123def4...
    LE->>BR: git worktree add /worktrees/{id} abc123def4
    BR-->>LE: worktree_path = /worktrees/{id}

    LE->>MX: release mutex
    deactivate MX

    Note over LE,JOB: Phase 2: Job runs (minutes)

    LE->>JOB: Dispatch K8s Job (mount worktree at /work)
    activate JOB

    JOB->>WT: read code
    JOB->>WT: make changes
    JOB->>WT: git commit
    JOB->>WT: git push (via sidecar)

    JOB-->>LE: Job watcher: SUCCEEDED
    deactivate JOB

    Note over LE,MX: Phase 3: Cleanup worktree

    LE->>MX: acquire mutex
    activate MX

    LE->>BR: git worktree remove --force /worktrees/{id}
    LE->>BR: git worktree prune

    LE->>MX: release mutex
    deactivate MX

    Note over MX: Mutex hold time: <1s per op<br/>Worst case queue (15 jobs): ~15s
```

---

## 8. Auth Flow

Credentials flow from the engineer's machine into K8s Secrets, are mounted only into the auth sidecar, and are injected into outbound requests. The agent container never sees raw credentials.

```mermaid
graph LR
    subgraph Machine["Engineer's Machine"]
        CLAUDE["~/.claude/<br/>(session tokens)"]
        OPENAI["OpenAI auth tokens<br/>(Pro subscription)"]
        SSH["SSH private key<br/>(for git push)"]
    end

    AUTH_CMD["<b>nemo auth</b><br/>Read local creds,<br/>create/update K8s Secrets"]

    CLAUDE --> AUTH_CMD
    OPENAI --> AUTH_CMD
    SSH --> AUTH_CMD

    AUTH_CMD -->|"K8s API"| SECRETS

    subgraph Cluster["k3s Cluster"]
        SECRETS["<b>K8s Secrets</b><br/>nautiloop-creds-{engineer}<br/>nautiloop-ssh-{engineer}"]

        subgraph JobPod["Job Pod"]
            AGENT["<b>Agent Container</b><br/>NO /secrets mount<br/>Cannot read credentials"]
            SIDECAR["<b>Auth Sidecar</b><br/>/secrets/model-credentials<br/>/secrets/ssh-key"]
        end
    end

    SECRETS -->|"volume mount<br/>(sidecar only)"| SIDECAR

    AGENT -->|"localhost:9090<br/>(no auth header)"| SIDECAR
    AGENT -->|"localhost:9091<br/>(git push)"| SIDECAR

    AGENT -->|"direct (session auth)<br/>HTTPS"| Anthropic2["<b>Anthropic API</b><br/>api.anthropic.com"]
    SIDECAR -->|"inject Bearer<br/>HTTPS"| OpenAI2["<b>OpenAI API</b><br/>api.openai.com"]
    SIDECAR -->|"inject SSH key"| GitRemote["<b>Git Remote</b><br/>github.com"]

    style Machine fill:#16213e,stroke:#0f3460,color:#fff
    style Cluster fill:#1a1a2e,stroke:#e94560,color:#fff
    style JobPod fill:#0f3460,stroke:#533483,color:#fff
    style SECRETS fill:#533483,stroke:#e94560,color:#fff
```

---

## 9. Retry and Error Handling

Decision tree for all failure scenarios. Each failure type has a specific retry policy and terminal condition.

```mermaid
flowchart TD
    START["Job Completes"] --> EXIT{"Exit code?"}

    EXIT -->|"exit 0"| OK["Success:<br/>continue loop"]
    EXIT -->|"exit 137"| OOM["OOM / Eviction"]
    EXIT -->|"exit 1 / non-zero"| ERR_TYPE{"Check error type"}

    OOM --> OOM_R{"retry_count < 2?"}
    OOM_R -->|"yes (1st)"| OOM_WAIT1["Wait 30s, re-dispatch<br/>retry_count++"]
    OOM_R -->|"yes (2nd)"| OOM_WAIT2["Wait 120s, re-dispatch<br/>retry_count++"]
    OOM_R -->|"no (3rd failure)"| FAILED_OOM["FAILED<br/>OOM after 3 attempts"]

    ERR_TYPE -->|"401 / auth error"| REAUTH["AWAITING_REAUTH<br/>Engineer runs nemo auth"]
    ERR_TYPE -->|"API timeout<br/>(10 min)"| TIMEOUT_R{"Retry once"}
    ERR_TYPE -->|"Malformed verdict<br/>(parse error)"| VERDICT_R{"retry_count < 2?"}
    ERR_TYPE -->|"No output 15 min<br/>(stuck)"| STUCK["Kill job, retry once"]

    REAUTH -->|"nemo auth"| RESUME["Resume at<br/>prev stage/DISPATCHED"]

    TIMEOUT_R -->|"succeeds"| OK
    TIMEOUT_R -->|"timeout again"| FAILED_TIMEOUT["FAILED<br/>Model API timeout after retry"]

    VERDICT_R -->|"retry 1"| VERDICT_R1["Re-dispatch same stage"]
    VERDICT_R1 --> VERDICT_R2{"Retry 2?"}
    VERDICT_R2 -->|"succeeds"| OK
    VERDICT_R2 -->|"still malformed"| FAILED_VERDICT["FAILED<br/>Malformed verdict after 2 retries"]

    STUCK -->|"succeeds"| OK
    STUCK -->|"stuck again"| FAILED_STUCK["FAILED<br/>No output for 15 min"]

    DIVERGE["Branch divergence<br/>(detected before dispatch)"] --> DIV_TYPE{"Divergence type?"}
    DIV_TYPE -->|"LocalAhead"| OK2["Continue<br/>(normal)"]
    DIV_TYPE -->|"RemoteAhead"| PAUSED_RA["PAUSED<br/>(engineer pushed)"]
    DIV_TYPE -->|"ForceDeviated"| PAUSED_FD["PAUSED<br/>(force push detected)"]

    PAUSED_RA -->|"nemo resume"| RESUME_SHA["Resume at remote SHA"]
    PAUSED_RA -->|"nemo cancel"| CANCELLED["CANCELLED"]
    PAUSED_FD -->|"nemo resume"| RESUME_SHA
    PAUSED_FD -->|"nemo cancel"| CANCELLED

    DISK["Disk full<br/>(worktree add fails)"] --> DISK_R{"Retry once<br/>after 60s"}
    DISK_R -->|"succeeds"| OK
    DISK_R -->|"still full"| FAILED_DISK["FAILED<br/>Disk full"]

    style FAILED_OOM fill:#8b0000,stroke:#e94560,color:#fff
    style FAILED_TIMEOUT fill:#8b0000,stroke:#e94560,color:#fff
    style FAILED_VERDICT fill:#8b0000,stroke:#e94560,color:#fff
    style FAILED_STUCK fill:#8b0000,stroke:#e94560,color:#fff
    style FAILED_DISK fill:#8b0000,stroke:#e94560,color:#fff
    style CANCELLED fill:#8b0000,stroke:#e94560,color:#fff
    style REAUTH fill:#b8860b,stroke:#e94560,color:#fff
    style PAUSED_RA fill:#b8860b,stroke:#e94560,color:#fff
    style PAUSED_FD fill:#b8860b,stroke:#e94560,color:#fff
    style OK fill:#006400,stroke:#e94560,color:#fff
    style OK2 fill:#006400,stroke:#e94560,color:#fff
    style RESUME fill:#006400,stroke:#e94560,color:#fff
    style RESUME_SHA fill:#006400,stroke:#e94560,color:#fff
```

---

## 10. Engineer Workflow (Pitch Diagram)

How an engineer uses Nautiloop day-to-day. This is the product from the user's perspective.

```mermaid
graph TD
    subgraph Engineer["👤 Engineer's Daily Workflow"]
        direction TB
        WRITE["Write a spec<br/><i>specs/feat/invoice-cancel.md</i>"]
        SUBMIT["<b>nemo start --harden spec.md</b><br/>One command. Walk away.<br/><i>or: nemo ship --harden spec.md</i>"]
        MONITOR["<b>nemo status</b><br/>Check progress anytime"]
        REVIEW_SPEC["Hardened spec ready!<br/>Read the improved spec"]
        APPROVE["<b>nemo approve</b><br/>Green-light implementation"]
        PR["Clean PR appears 🎉<br/>Tests pass. Review clean."]
        MERGE["Review PR, merge<br/><i>(nemo ship: auto-merged)</i>"]
    end

    subgraph Nautiloop["⚡ Nautiloop (runs on shared cluster)"]
        direction TB

        subgraph Harden["Spec Hardening Loop"]
            H1["🔍 Audit spec<br/><i>OpenAI finds ambiguity,<br/>missing edge cases</i>"]
            H2["✍️ Revise spec<br/><i>Claude fixes issues,<br/>adds detail</i>"]
            H3{"Audit clean?"}
        end

        subgraph Build["Implementation Loop"]
            I1["🛠️ Implement<br/><i>Claude writes code,<br/>runs tests locally</i>"]
            I2["🧪 Verify<br/><i>Independent CI:<br/>run test suites for<br/>all affected services</i>"]
            I3["🔍 Review<br/><i>OpenAI adversarial<br/>code review</i>"]
            I4{"Review clean?"}
        end
    end

    WRITE --> SUBMIT
    SUBMIT --> H1
    H1 --> H3
    H3 -->|"No — issues found"| H2
    H2 --> H1
    H3 -->|"Yes — spec is solid"| REVIEW_SPEC
    REVIEW_SPEC --> APPROVE
    APPROVE --> I1
    I1 --> I2
    I2 -->|"Tests fail"| I1
    I2 -->|"Tests pass"| I3
    I3 --> I4
    I4 -->|"No — issues found"| I1
    I4 -->|"Yes — code is clean"| PR

    SUBMIT -.->|"meanwhile..."| MONITOR
    PR --> MERGE

    style Engineer fill:#0f3460,stroke:#e94560,color:#fff
    style Nautiloop fill:#1a1a2e,stroke:#e94560,color:#fff
    style Harden fill:#16213e,stroke:#0f3460,color:#fff
    style Build fill:#16213e,stroke:#0f3460,color:#fff
    style PR fill:#006400,stroke:#e94560,color:#fff
    style SUBMIT fill:#533483,stroke:#e94560,color:#fff
```

---

## 11. Parallel Execution (Team View)

What it looks like when a team of 3 engineers is using Nautiloop simultaneously. Each engineer runs up to 5 parallel loops on shared infrastructure.

```mermaid
gantt
    title Nautiloop: 3 Engineers × 5 Parallel Loops
    dateFormat HH:mm
    axisFormat %H:%M

    section Alice
    feat/invoice-cancel (harden)       :a1, 09:00, 20min
    feat/invoice-cancel (implement)    :a1b, after a1, 45min
    fix/budget-overflow (implement)    :a2, 09:05, 30min
    feat/reporting (harden)            :a3, 09:10, 25min
    feat/reporting (implement)         :a3b, after a3, 50min
    feat/export-pdf (implement)        :a4, 09:15, 35min

    section Bob
    feat/tracker-extract (implement)   :b1, 09:00, 40min
    spec/campaign-pause (harden)       :b2, 09:05, 30min
    fix/auth-token (implement)         :b3, 09:20, 25min

    section Eve
    refactor/auth-middleware (implement) :e1, 09:00, 60min
    feat/notifications (harden)         :e2, 09:10, 20min
    feat/notifications (implement)      :e2b, after e2, 40min
```

---

## 12. The Convergent Loop (Core Primitive)

The single primitive that powers everything in Nautiloop. Both spec hardening and implementation are instances of this same loop.

```mermaid
graph TD
    START(("Start")) --> WORK["<b>Work Stage</b><br/><i>Harden: Revise spec</i><br/><i>Implement: Write code</i>"]
    WORK --> CHECK["<b>Check Stage</b><br/><i>Harden: Audit spec</i><br/><i>Implement: Test + Review</i>"]
    CHECK --> EVAL{"Clean?"}
    EVAL -->|"Issues found"| FEEDBACK["Write feedback file<br/><i>Issues become input<br/>for next round</i>"]
    FEEDBACK --> WORK
    EVAL -->|"All clean"| CONVERGE(("✓ Converged"))
    EVAL -->|"Max rounds"| HUMAN["Needs human review"]

    CHECK -.->|"Round N"| ROUND["Round counter +1"]
    ROUND -.-> EVAL

    style START fill:#533483,stroke:#e94560,color:#fff
    style CONVERGE fill:#006400,stroke:#e94560,color:#fff
    style HUMAN fill:#b8860b,stroke:#e94560,color:#fff
    style WORK fill:#0f3460,stroke:#e94560,color:#fff
    style CHECK fill:#0f3460,stroke:#e94560,color:#fff
```

**Key insight:** The loop doesn't run a fixed number of times. It runs until the adversarial check finds nothing wrong. The exit condition is quality, not iteration count.
