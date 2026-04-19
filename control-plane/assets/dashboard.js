// Nautiloop Dashboard — Vanilla ES2022, <10 KB minified
// FR-7: Card grid poll, action button fetch, SSE log stream, tab title updates

(function () {
  "use strict";

  const API_KEY = getCookie("nautiloop_api_key") || "";
  let pollTimer = null;
  let sseSource = null;
  let lastFocusConvergedCount = 0;
  let convergedSinceLastFocus = 0;
  // FR-7a: Bell sound on converge (opt-in, off by default).
  // Enable via localStorage: localStorage.setItem("nautiloop_bell", "true")
  let prevConvergedIds = new Set();

  // ── Helpers ──

  function getCookie(name) {
    const match = document.cookie.match(new RegExp("(?:^|; )" + name + "=([^;]*)"));
    return match ? decodeURIComponent(match[1]) : "";
  }

  function authHeaders() {
    return { Authorization: "Bearer " + API_KEY, "Content-Type": "application/json" };
  }

  async function apiFetch(url, opts = {}) {
    opts.headers = Object.assign(authHeaders(), opts.headers || {});
    const res = await fetch(url, opts);
    if (!res.ok) {
      const body = await res.text();
      throw new Error(res.status + ": " + body);
    }
    return res;
  }

  function formatElapsed(isoDate) {
    const ms = Date.now() - new Date(isoDate).getTime();
    const s = Math.floor(ms / 1000);
    if (s < 60) return s + "s";
    const m = Math.floor(s / 60);
    if (m < 60) return m + "m " + (s % 60) + "s";
    const h = Math.floor(m / 60);
    return h + "h " + (m % 60) + "m";
  }

  function formatTokens(n) {
    if (n == null || n === 0) return "0";
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return (n / 1000).toFixed(0) + "K";
    return String(n);
  }

  function stateClass(state) {
    return "badge-" + state.toLowerCase().replace(/_/g, "-");
  }

  function showStatus(msg) {
    let bar = document.getElementById("status-bar");
    if (!bar) return;
    bar.textContent = msg;
    bar.classList.add("visible");
    setTimeout(() => bar.classList.remove("visible"), 4000);
  }

  // FR-7a: Play a short bell tone via Web Audio when a loop converges.
  function playBell() {
    if (localStorage.getItem("nautiloop_bell") !== "true") return;
    try {
      const ctx = new (window.AudioContext || window.webkitAudioContext)();
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.type = "sine";
      osc.frequency.value = 880;
      gain.gain.value = 0.15;
      gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.3);
      osc.connect(gain);
      gain.connect(ctx.destination);
      osc.start(ctx.currentTime);
      osc.stop(ctx.currentTime + 0.3);
    } catch { /* ignore if Web Audio unavailable */ }
  }

  // ── Card Grid Polling (FR-3d) ──

  function startGridPoll() {
    if (!document.getElementById("card-grid")) return;
    pollGrid();
    pollTimer = setInterval(pollGrid, 5000);
  }

  async function pollGrid() {
    try {
      const params = new URLSearchParams(window.location.search);
      const url = "/dashboard/state?" + params.toString();
      const res = await apiFetch(url);
      const data = await res.json();
      updateGrid(data);
      updateFleetSummary(data.fleet);
      updateTabTitle(data);
    } catch (e) {
      console.error("Poll failed:", e);
    }
  }

  function updateGrid(data) {
    const grid = document.getElementById("card-grid");
    if (!grid || !data.loops) return;

    // FR-7a: Detect newly converged loops and play bell
    const terminalStates = ["CONVERGED", "HARDENED", "SHIPPED"];
    const currentConvergedIds = new Set(
      data.loops.filter((l) => terminalStates.includes(l.state)).map((l) => l.loop_id)
    );
    if (prevConvergedIds.size > 0) {
      for (const id of currentConvergedIds) {
        if (!prevConvergedIds.has(id)) {
          playBell();
          break; // one bell per poll cycle is enough
        }
      }
    }
    prevConvergedIds = currentConvergedIds;

    // Update existing cards and track IDs
    const existingIds = new Set();
    for (const card of grid.querySelectorAll("[data-loop-id]")) {
      existingIds.add(card.dataset.loopId);
    }

    const newIds = new Set(data.loops.map((l) => l.loop_id));

    // Remove cards no longer present
    for (const card of grid.querySelectorAll("[data-loop-id]")) {
      if (!newIds.has(card.dataset.loopId)) {
        card.remove();
      }
    }

    // Update or insert cards
    for (const loop of data.loops) {
      const existing = grid.querySelector('[data-loop-id="' + loop.loop_id + '"]');
      if (existing) {
        updateCardFields(existing, loop);
      }
      // Don't insert new cards via JS to avoid complex HTML duplication;
      // the next full page load will pick them up.
    }
  }

  function updateCardFields(card, loop) {
    const badge = card.querySelector(".badge");
    if (badge) {
      const oldClass = badge.className;
      badge.className = "badge " + stateClass(loop.state);
      badge.textContent = loop.state;
      if (oldClass !== badge.className) {
        badge.style.transition = "background-color 1s ease, color 1s ease";
      }
    }
    const elapsed = card.querySelector(".card-elapsed");
    if (elapsed) elapsed.textContent = formatElapsed(loop.created_at);

    const progress = card.querySelector(".card-progress");
    if (progress) {
      const stage = loop.current_stage ? " \u00B7 stage: " + loop.current_stage : "";
      const isTerminal = ["CONVERGED", "FAILED", "CANCELLED", "HARDENED", "SHIPPED"].includes(loop.state);
      progress.textContent = isTerminal
        ? "round " + loop.round
        : "round " + loop.round + "/" + loop.max_rounds + stage;
    }

    const pulse = card.querySelector(".pulse");
    if (pulse) {
      const isActive = !["CONVERGED", "FAILED", "CANCELLED", "HARDENED", "SHIPPED"].includes(loop.state);
      pulse.classList.toggle("active", isActive);
    }

    const metrics = card.querySelector(".card-metrics");
    if (metrics && loop.total_tokens != null) {
      const parts = [];
      parts.push(formatTokens(loop.total_tokens) + " tokens");
      if (loop.total_cost != null) parts.push("$" + loop.total_cost.toFixed(2));
      if (loop.last_verdict != null) parts.push(loop.last_verdict);
      metrics.textContent = parts.join(" \u00B7 ");
    }
  }

  function updateFleetSummary(fleet) {
    const el = document.getElementById("fleet-summary");
    if (!el || !fleet) return;
    el.textContent = fleet.text;
  }

  function updateTabTitle(data) {
    if (!data.loops) return;
    const convergedCount = data.loops.filter(
      (l) => l.state === "CONVERGED" || l.state === "HARDENED" || l.state === "SHIPPED"
    ).length;

    if (document.hidden) {
      const diff = convergedCount - lastFocusConvergedCount;
      if (diff > convergedSinceLastFocus) {
        convergedSinceLastFocus = diff;
        document.title = "(" + convergedSinceLastFocus + ") nautiloop";
      }
    } else {
      lastFocusConvergedCount = convergedCount;
      convergedSinceLastFocus = 0;
      document.title = "nautiloop";
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      convergedSinceLastFocus = 0;
      document.title = "nautiloop";
    }
  });

  // ── Filter Chips ──

  function initChips() {
    for (const chip of document.querySelectorAll(".chip[data-filter]")) {
      chip.addEventListener("click", () => {
        const group = chip.dataset.group;
        const value = chip.dataset.filter;
        // Deactivate siblings
        for (const sibling of document.querySelectorAll('.chip[data-group="' + group + '"]')) {
          sibling.classList.remove("active");
        }
        chip.classList.add("active");
        // Update URL params and re-poll
        const params = new URLSearchParams(window.location.search);
        if (group === "state") {
          if (value === "all") params.delete("state_filter");
          else params.set("state_filter", value);
        } else if (group === "engineer") {
          if (value === "team") {
            params.set("team", "true");
            params.delete("engineer");
          } else if (value === "mine") {
            params.delete("team");
            params.delete("engineer");
          } else {
            params.set("engineer", value);
            params.delete("team");
          }
        }
        const qs = params.toString();
        history.replaceState(null, "", "/dashboard" + (qs ? "?" + qs : ""));
        pollGrid();
      });
    }
  }

  // ── Action Buttons (FR-4b) ──

  function initActions() {
    for (const btn of document.querySelectorAll("[data-action]")) {
      btn.addEventListener("click", async (e) => {
        e.preventDefault();
        const action = btn.dataset.action;
        const loopId = btn.dataset.loopId;

        if (action === "cancel" || action === "cancel-all") {
          showConfirmModal(btn);
          return;
        }

        btn.disabled = true;
        try {
          await executeAction(action, loopId);
          showStatus("Action completed: " + action);
          // Reload detail page to reflect new state
          if (window.location.pathname.includes("/loops/")) {
            setTimeout(() => window.location.reload(), 500);
          }
        } catch (err) {
          showStatus("Error: " + err.message);
        } finally {
          btn.disabled = false;
        }
      });
    }
  }

  async function executeAction(action, loopId) {
    switch (action) {
      case "approve":
        await apiFetch("/approve/" + loopId, { method: "POST" });
        break;
      case "cancel":
        await apiFetch("/cancel/" + loopId, { method: "DELETE" });
        break;
      case "resume":
        await apiFetch("/resume/" + loopId, { method: "POST" });
        break;
      case "extend":
        await apiFetch("/extend/" + loopId, {
          method: "POST",
          body: JSON.stringify({ add_rounds: 10 }),
        });
        break;
    }
  }

  // ── Kill Switch (FR-10) ──

  function initKillSwitch() {
    const btn = document.getElementById("cancel-all-btn");
    if (!btn) return;
    btn.addEventListener("click", () => showConfirmModal(btn));
  }

  function showConfirmModal(triggerBtn) {
    const action = triggerBtn.dataset.action;
    const loopId = triggerBtn.dataset.loopId;
    const isAll = action === "cancel-all";

    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML =
      '<div class="modal">' +
      "<h3>" +
      (isAll ? "Cancel all active loops?" : "Cancel this loop?") +
      "</h3>" +
      "<p class='text-sm text-muted mb-md'>" +
      (isAll ? "This cannot be undone." : "This will request cancellation of the loop.") +
      "</p>" +
      '<div class="modal-actions">' +
      '<button class="btn" id="modal-cancel">Cancel</button>' +
      '<button class="btn btn-danger" id="modal-confirm">' +
      (isAll ? "Confirm cancel all" : "Confirm cancel") +
      "</button>" +
      "</div></div>";

    document.body.appendChild(overlay);

    document.getElementById("modal-cancel").addEventListener("click", () => overlay.remove());
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) overlay.remove();
    });

    document.getElementById("modal-confirm").addEventListener("click", async () => {
      const confirmBtn = document.getElementById("modal-confirm");
      confirmBtn.disabled = true;
      try {
        if (isAll) {
          await cancelAllLoops();
        } else {
          await executeAction("cancel", loopId);
          showStatus("Loop cancelled");
        }
        overlay.remove();
        if (window.location.pathname.includes("/loops/")) {
          setTimeout(() => window.location.reload(), 500);
        } else {
          pollGrid();
        }
      } catch (err) {
        showStatus("Error: " + err.message);
        overlay.remove();
      }
    });
  }

  async function cancelAllLoops() {
    const res = await apiFetch("/dashboard/state?team=true");
    const data = await res.json();
    const activeLoops = (data.loops || []).filter(
      (l) => !["CONVERGED", "FAILED", "CANCELLED", "HARDENED", "SHIPPED"].includes(l.state)
    );

    let succeeded = 0;
    let failed = 0;
    const errors = [];

    await Promise.allSettled(
      activeLoops.map(async (loop) => {
        try {
          await apiFetch("/cancel/" + loop.loop_id, { method: "DELETE" });
          succeeded++;
        } catch (err) {
          failed++;
          errors.push(loop.loop_id + ": " + err.message);
        }
      })
    );

    let msg = "Cancelled " + succeeded + "/" + activeLoops.length + " loops";
    if (failed > 0) msg += " (" + failed + " failed)";
    showStatus(msg);
  }

  // ── SSE Log Stream (FR-4a) ──

  function initLogStream() {
    const logPane = document.getElementById("log-pane");
    const loopId = logPane && logPane.dataset.loopId;
    const isTerminal = logPane && logPane.dataset.terminal === "true";
    if (!logPane || !loopId || isTerminal) return;

    // For active loops, connect SSE
    sseSource = new EventSource("/dashboard/stream/" + loopId);
    sseSource.addEventListener("log", (e) => {
      try {
        const data = JSON.parse(e.data);
        const line = document.createElement("span");
        line.className = "log-line";
        line.textContent = data.line + "\n";
        logPane.appendChild(line);
        // Limit to last 200 lines
        while (logPane.children.length > 200) {
          logPane.removeChild(logPane.firstChild);
        }
        logPane.scrollTop = logPane.scrollHeight;
      } catch {
        // ignore parse errors
      }
    });
    sseSource.onerror = () => {
      // SSE closed (loop terminated or network error)
      if (sseSource) sseSource.close();
    };
  }

  // ── Pod Introspect (FR-5) ──

  function initPodIntrospect() {
    const disclosure = document.getElementById("pod-introspect");
    if (!disclosure) return;
    let pollId = null;

    disclosure.addEventListener("toggle", () => {
      if (disclosure.open) {
        pollPodIntrospect(disclosure);
        pollId = setInterval(() => pollPodIntrospect(disclosure), 5000);
      } else {
        if (pollId) clearInterval(pollId);
      }
    });
  }

  async function pollPodIntrospect(disclosure) {
    const loopId = disclosure.dataset.loopId;
    const body = disclosure.querySelector(".disclosure-body");
    if (!loopId || !body) return;
    try {
      const res = await apiFetch("/pod-introspect/" + loopId);
      const data = await res.json();
      let html = '<table class="token-table">';
      html += "<tr><th>PID</th><th>CPU%</th><th>Command</th></tr>";
      for (const p of data.processes || []) {
        html += "<tr><td>" + p.pid + "</td><td>" + p.cpu_percent.toFixed(1) + "</td><td>" + escapeHtml(p.cmd) + "</td></tr>";
      }
      html += "</table>";
      if (data.container_stats) {
        html +=
          '<p class="text-sm text-muted">CPU: ' +
          data.container_stats.cpu_millicores +
          "m / Mem: " +
          (data.container_stats.memory_bytes / 1048576).toFixed(0) +
          " MB</p>";
      }
      if (data.worktree) {
        html += '<p class="text-sm text-muted">HEAD: ' + (data.worktree.head_sha || "—") + "</p>";
      }
      body.innerHTML = html;
    } catch {
      body.innerHTML = '<p class="text-sm text-muted">Unavailable</p>';
    }
  }

  function escapeHtml(s) {
    const d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  // ── Judge Reasoning (FR-11) ──

  function initJudgeToggles() {
    for (const icon of document.querySelectorAll(".judge-icon")) {
      icon.addEventListener("click", (e) => {
        e.stopPropagation();
        const detail = icon.closest("tr").nextElementSibling;
        if (detail && detail.classList.contains("judge-detail-row")) {
          detail.classList.toggle("hidden");
        }
      });
    }
  }

  // ── Feed Load More (FR-12) ──

  function initLoadMore() {
    const btn = document.getElementById("load-more-btn");
    if (!btn) return;
    btn.addEventListener("click", async () => {
      const cursor = btn.dataset.cursor;
      if (!cursor) return;
      btn.disabled = true;
      btn.textContent = "Loading...";
      try {
        const params = new URLSearchParams(window.location.search);
        params.set("cursor", cursor);
        const res = await apiFetch("/dashboard/feed/json?" + params.toString());
        const data = await res.json();
        const list = document.getElementById("feed-list");
        if (!list) return;
        for (const item of data.items) {
          const li = document.createElement("a");
          li.className = "feed-item";
          li.href = "/dashboard/loops/" + item.loop_id;
          li.innerHTML =
            '<span class="feed-time">' + new Date(item.updated_at).toLocaleTimeString() + "</span>" +
            '<span class="feed-engineer">' + escapeHtml(item.engineer) + "</span>" +
            '<span class="feed-spec">' + escapeHtml(item.spec_path) + "</span>" +
            '<span class="badge ' + stateClass(item.state) + '">' + item.state + "</span>" +
            '<span class="feed-cost">$' + (item.total_cost || 0).toFixed(2) + "</span>";
          list.appendChild(li);
        }
        if (data.items.length > 0 && data.next_cursor) {
          btn.dataset.cursor = data.next_cursor;
          btn.textContent = "Load more";
          btn.disabled = false;
        } else {
          btn.remove();
        }
      } catch (err) {
        btn.textContent = "Error loading";
        console.error(err);
      }
    });
  }

  // ── Stats Window Toggle (FR-14) ──

  function initWindowToggle() {
    for (const btn of document.querySelectorAll(".window-toggle button")) {
      btn.addEventListener("click", () => {
        const win = btn.dataset.window;
        const params = new URLSearchParams(location.search);
        params.set("window", win);
        location.href = "/dashboard/stats?" + params.toString();
      });
    }
  }

  // ── Menu Toggle ──

  function initMenu() {
    const menuBtn = document.getElementById("menu-toggle");
    const menuDropdown = document.getElementById("menu-dropdown");
    if (!menuBtn || !menuDropdown) return;

    menuBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      menuDropdown.classList.toggle("hidden");
    });

    document.addEventListener("click", () => {
      menuDropdown.classList.add("hidden");
    });
  }

  // ── Init ──

  function init() {
    initChips();
    initActions();
    initKillSwitch();
    initLogStream();
    initPodIntrospect();
    initJudgeToggles();
    initLoadMore();
    initWindowToggle();
    initMenu();
    startGridPoll();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }

  // Clean up on page unload
  window.addEventListener("beforeunload", () => {
    if (pollTimer) clearInterval(pollTimer);
    if (sseSource) sseSource.close();
  });
})();
