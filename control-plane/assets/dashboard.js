// nautiloop dashboard — vanilla ES2022 (FR-7)
// Mandatory: polling, action buttons, SSE log stream, filter chips, confirm modals
// Progressive: tab title badge, web audio bell, color fade animation, localStorage filters

(function() {
  "use strict";

  const POLL_INTERVAL = 5000;
  const INTROSPECT_INTERVAL = 5000;
  const LOG_LINE_CAP = 200;

  // --- State ---
  let pollTimer = null;
  let introspectTimer = null;
  let eventSource = null;
  let activeFilter = "active";
  let engineerFilter = "mine";
  let convergedSinceLastFocus = 0;
  let prevStates = {};
  let viewer = document.body.dataset.viewer || "";

  // --- Helpers ---
  function esc(s) {
    if (!s) return "";
    const d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  function fmtTokens(n) {
    if (n == null) return "\u2014";
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return (n / 1000).toFixed(0) + "K";
    return String(n);
  }

  function fmtCost(c) {
    if (c == null) return "\u2014";
    return "$" + c.toFixed(2);
  }

  function fmtElapsed(created) {
    const ms = Date.now() - new Date(created).getTime();
    const s = Math.floor(ms / 1000);
    if (s < 60) return s + "s";
    const m = Math.floor(s / 60);
    if (m < 60) return m + "m " + (s % 60) + "s";
    const h = Math.floor(m / 60);
    return h + "h " + (m % 60) + "m";
  }

  function specFilename(path) {
    if (!path) return "\u2014";
    const parts = path.split("/");
    return parts[parts.length - 1];
  }

  function shortId(id) {
    return id ? id.substring(0, 8) : "\u2014";
  }

  function badgeClass(state) {
    const green = ["CONVERGED", "HARDENED", "SHIPPED"];
    const red = ["FAILED", "CANCELLED"];
    const amber = ["AWAITING_APPROVAL", "PAUSED", "AWAITING_REAUTH"];
    const blue = ["IMPLEMENTING", "TESTING", "REVIEWING", "HARDENING"];
    if (green.includes(state)) return "badge-green";
    if (red.includes(state)) return "badge-red";
    if (amber.includes(state)) return "badge-amber";
    if (blue.includes(state)) return "badge-blue";
    return "badge-gray";
  }

  function pulseClass(subState) {
    if (subState === "RUNNING") return "pulse-running";
    if (subState === "DISPATCHED") return "pulse-dispatched";
    return "pulse-completed";
  }

  function isTerminal(state) {
    return ["CONVERGED", "FAILED", "CANCELLED", "HARDENED", "SHIPPED"].includes(state);
  }

  function isActive(state) {
    return !isTerminal(state);
  }

  // Stable per-engineer color from name hash (uses UTF-8 bytes to match Rust)
  function engineerColor(name) {
    let h = 0;
    const bytes = new TextEncoder().encode(name);
    for (let i = 0; i < bytes.length; i++) {
      h = ((h << 5) - h + bytes[i]) | 0;
    }
    const hue = ((h % 360) + 360) % 360;
    return "hsl(" + hue + ",55%,45%)";
  }

  function engineerInitials(name) {
    if (!name) return "?";
    return name.substring(0, 2).toUpperCase();
  }

  // --- Card Grid ---
  function renderCard(loop) {
    const showEngineer = engineerFilter !== "mine" || loop.engineer !== viewer;
    const engBadge = showEngineer
      ? '<span class="card-engineer" style="background:' + engineerColor(loop.engineer) + '">' + esc(engineerInitials(loop.engineer)) + '</span>'
      : '';
    const verdict = loop.last_verdict || "\u2014";
    const progress = isTerminal(loop.state)
      ? "round " + loop.round
      : "round " + loop.round + "/" + loop.max_rounds + " \u00b7 stage: " + esc(loop.current_stage || "\u2014");

    return '<a class="card" href="/dashboard/loops/' + esc(loop.id) + '" data-id="' + esc(loop.id) + '">'
      + engBadge
      + '<div class="card-header">'
      + '<span class="pulse ' + pulseClass(loop.sub_state) + '"></span>'
      + '<span class="badge ' + badgeClass(loop.state) + '">' + esc(loop.state) + '</span>'
      + '<span style="font-size:0.75rem;color:var(--text-muted)">' + esc(shortId(loop.id)) + '</span>'
      + '<span style="margin-left:auto;font-size:0.75rem;color:var(--text-muted)">' + fmtElapsed(loop.created_at) + '</span>'
      + '</div>'
      + '<div class="card-title">' + esc(specFilename(loop.spec_path)) + '</div>'
      + '<div class="card-subtitle">' + esc(loop.branch) + '</div>'
      + '<div class="card-progress">' + progress + '</div>'
      + '<div class="card-metrics">'
      + '<span>' + fmtTokens(((loop.total_tokens||{}).input || 0) + ((loop.total_tokens||{}).output || 0)) + ' tok</span>'
      + '<span>' + fmtCost(loop.total_cost) + '</span>'
      + '<span>' + esc(verdict) + '</span>'
      + '</div>'
      + '</a>';
  }

  function matchesFilter(loop) {
    // State filter
    if (activeFilter === "active" && isTerminal(loop.state)) return false;
    if (activeFilter === "converged" && loop.state !== "CONVERGED" && loop.state !== "HARDENED" && loop.state !== "SHIPPED") return false;
    if (activeFilter === "failed" && loop.state !== "FAILED" && loop.state !== "CANCELLED") return false;
    // Engineer filter — "mine" filters handled server-side via ?team param,
    // but also filter client-side for immediate chip changes before next poll
    if (engineerFilter === "mine" && loop.engineer !== viewer) return false;
    if (engineerFilter !== "mine" && engineerFilter !== "team" && loop.engineer !== engineerFilter) return false;
    return true;
  }

  function updateGrid(data) {
    const grid = document.getElementById("card-grid");
    if (!grid) return;

    // Track state changes for tab title
    if (data.loops) {
      for (const loop of data.loops) {
        if (prevStates[loop.id] && !isTerminal(prevStates[loop.id]) && loop.state === "CONVERGED") {
          convergedSinceLastFocus++;
          playBell();
        }
        prevStates[loop.id] = loop.state;
      }
    }

    const filtered = (data.loops || []).filter(matchesFilter);
    if (filtered.length === 0) {
      grid.innerHTML = '<div class="empty-state">No loops match the current filters.</div>';
    } else {
      grid.innerHTML = filtered.map(renderCard).join("");
    }

    // Update chips counts
    updateChipCounts(data);

    // Update fleet summary
    updateFleetSummary(data);

    // Update viewer from response
    if (data.viewer) viewer = data.viewer;

    // Update tab title (progressive enhancement)
    updateTabTitle();

    // Show/hide kill switch (FR-10d)
    const killBtn = document.getElementById("kill-switch-btn");
    if (killBtn) {
      killBtn.style.display = engineerFilter === "team" ? "" : "none";
    }
  }

  function updateChipCounts(data) {
    const counts = data.aggregates ? data.aggregates.counts_by_state : {};
    let activeCount = 0, convergedCount = 0, failedCount = 0, totalCount = data.aggregates ? data.aggregates.total_loops : 0;
    for (const [st, n] of Object.entries(counts)) {
      if (isTerminal(st)) {
        if (st === "CONVERGED" || st === "HARDENED" || st === "SHIPPED") convergedCount += n;
        else failedCount += n;
      } else {
        activeCount += n;
      }
    }
    setText("chip-active-count", activeCount);
    setText("chip-converged-count", convergedCount);
    setText("chip-failed-count", failedCount);
    setText("chip-all-count", totalCount);

    // Engineer chips
    const engBar = document.getElementById("engineer-chips");
    if (engBar && data.engineers) {
      // Keep Mine and Team, replace individual engineer chips
      const existing = engBar.querySelectorAll("[data-eng-individual]");
      existing.forEach(e => e.remove());
      for (const eng of data.engineers) {
        const chip = document.createElement("button");
        chip.className = "chip" + (engineerFilter === eng ? " active" : "");
        chip.dataset.engIndividual = eng;
        chip.textContent = eng;
        chip.onclick = () => { setEngineerFilter(eng); };
        engBar.appendChild(chip);
      }
    }
  }

  function setText(id, val) {
    const el = document.getElementById(id);
    if (el) el.textContent = val;
  }

  function updateFleetSummary(data) {
    const el = document.getElementById("fleet-summary-content");
    if (!el || !data.fleet_summary) return;
    const f = data.fleet_summary;
    let parts = [];
    parts.push("This week");
    parts.push('\u00b7 <a href="/dashboard/stats#total-loops" class="fleet-link">' + f.total_loops + " loops</a>");
    if (f.total_cost != null) parts.push('\u00b7 <a href="/dashboard/stats#total-cost" class="fleet-link">' + fmtCost(f.total_cost) + "</a>");
    const cr = f.converge_rate != null ? Math.round(f.converge_rate * 100) : null;
    if (cr != null) {
      let trend = "";
      if (f.trends && f.trends.converge_rate_delta != null) {
        const d = Math.round(f.trends.converge_rate_delta * 100);
        if (d > 0) trend = ' <span class="trend-up">\u2191' + d + '%</span>';
        else if (d < 0) trend = ' <span class="trend-down">\u2193' + Math.abs(d) + '%</span>';
      }
      parts.push('\u00b7 <a href="/dashboard/stats#converge-rate" class="fleet-link">' + cr + "%" + trend + " converged</a>");
    }
    if (f.avg_rounds != null) {
      let trend = "";
      if (f.trends && f.trends.avg_rounds_delta != null) {
        const d = f.trends.avg_rounds_delta;
        if (d > 0.05) trend = ' <span class="trend-down">\u2191' + d.toFixed(1) + '</span>';
        else if (d < -0.05) trend = ' <span class="trend-up">\u2193' + Math.abs(d).toFixed(1) + '</span>';
      }
      parts.push('\u00b7 <a href="/dashboard/stats#avg-rounds" class="fleet-link">avg ' + f.avg_rounds.toFixed(1) + trend + " rounds</a>");
    }
    if (f.top_spender && f.top_spender.cost != null) {
      parts.push('\u00b7 <a href="/dashboard/stats#per-engineer" class="fleet-link">top: ' + esc(f.top_spender.engineer) + " (" + fmtCost(f.top_spender.cost) + ")</a>");
    }
    el.innerHTML = parts.join(" ");
  }

  function updateTabTitle() {
    if (document.hasFocus()) {
      convergedSinceLastFocus = 0;
      document.title = "nautiloop";
    } else if (convergedSinceLastFocus > 0) {
      document.title = "(" + convergedSinceLastFocus + ") nautiloop";
    }
  }

  // --- Polling ---
  function startPolling() {
    if (pollTimer) return;
    poll();
    pollTimer = setInterval(poll, POLL_INTERVAL);
  }

  function stopPolling() {
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  }

  function poll() {
    const team = engineerFilter !== "mine";
    const url = "/dashboard/state?team=" + team + "&include_terminal=all";
    fetch(url, { credentials: "same-origin" })
      .then(r => { if (!r.ok) throw new Error(r.status); return r.json(); })
      .then(updateGrid)
      .catch(e => console.warn("poll failed:", e));
  }

  // --- Filter Chips ---
  function setStateFilter(filter) {
    activeFilter = filter;
    document.querySelectorAll("[data-state-filter]").forEach(el => {
      el.classList.toggle("active", el.dataset.stateFilter === filter);
    });
    poll();
  }

  function setEngineerFilter(filter) {
    engineerFilter = filter;
    document.querySelectorAll("[data-eng-filter]").forEach(el => {
      el.classList.toggle("active", el.dataset.engFilter === filter);
    });
    document.querySelectorAll("[data-eng-individual]").forEach(el => {
      el.classList.toggle("active", el.dataset.engIndividual === filter);
    });
    poll();
  }

  // --- Action Buttons ---
  function actionFetch(url, method, body) {
    const opts = { method: method, credentials: "same-origin", headers: {} };
    if (body) {
      opts.headers["Content-Type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    return fetch(url, opts).then(r => {
      if (!r.ok) return r.json().then(d => { throw new Error(d.error || r.status); });
      return r.json();
    });
  }

  function showToast(msg, duration) {
    const container = document.getElementById("toast-container");
    if (!container) return;
    const toast = document.createElement("div");
    toast.className = "toast";
    toast.textContent = msg;
    container.appendChild(toast);
    setTimeout(() => toast.remove(), duration || 10000);
  }

  function bindActions() {
    document.addEventListener("click", function(e) {
      const btn = e.target.closest("[data-action]");
      if (!btn) return;
      e.preventDefault();
      const action = btn.dataset.action;
      const loopId = btn.dataset.loopId;

      if (action === "approve") {
        actionFetch("/approve/" + loopId, "POST")
          .then(() => { showToast("Approved"); location.reload(); })
          .catch(err => showToast("Error: " + err.message));
      } else if (action === "cancel") {
        showConfirmModal("Cancel this loop?", "This cannot be undone.", function() {
          actionFetch("/cancel/" + loopId, "DELETE")
            .then(() => { showToast("Cancelled"); location.reload(); })
            .catch(err => showToast("Error: " + err.message));
        });
      } else if (action === "resume") {
        actionFetch("/resume/" + loopId, "POST")
          .then(() => { showToast("Resumed"); location.reload(); })
          .catch(err => showToast("Error: " + err.message));
      } else if (action === "extend") {
        actionFetch("/extend/" + loopId, "POST", { add_rounds: 10 })
          .then(() => { showToast("Extended +10 rounds"); location.reload(); })
          .catch(err => showToast("Error: " + err.message));
      } else if (action === "cancel-all") {
        // Fetch current active loop IDs from /dashboard/state before cancelling
        fetch("/dashboard/state?team=true", { credentials: "same-origin" })
          .then(r => r.ok ? r.json() : Promise.reject(r.status))
          .then(data => {
            const activeLoops = (data.loops || [])
              .filter(l => isActive(l.state))
              .map(l => l.id);
            if (activeLoops.length === 0) {
              showToast("No active loops to cancel.");
              return;
            }
            showConfirmModal(
              "Cancel " + activeLoops.length + " active loops?",
              "This cannot be undone.",
              function() {
                let ok = 0, fail = 0;
                const promises = activeLoops.map(id =>
                  actionFetch("/cancel/" + id, "DELETE")
                    .then(() => ok++)
                    .catch(() => fail++)
                );
                Promise.all(promises).then(() => {
                  let msg = "Cancelled " + ok + "/" + activeLoops.length + " loops";
                  if (fail > 0) msg += " (" + fail + " failed)";
                  showToast(msg);
                  poll();
                });
              }
            );
          })
          .catch(e => showToast("Error fetching active loops: " + e));
      }
    });
  }

  // --- Confirm Modal ---
  function showConfirmModal(title, body, onConfirm) {
    const overlay = document.getElementById("confirm-modal");
    if (!overlay) return;
    document.getElementById("modal-title").textContent = title;
    document.getElementById("modal-body").textContent = body;
    overlay.classList.add("open");

    const confirmBtn = document.getElementById("modal-confirm");
    const cancelBtn = document.getElementById("modal-cancel");
    const handler = () => {
      overlay.classList.remove("open");
      confirmBtn.removeEventListener("click", handler);
      onConfirm();
    };
    const cancelHandler = () => {
      overlay.classList.remove("open");
      cancelBtn.removeEventListener("click", cancelHandler);
    };
    confirmBtn.addEventListener("click", handler);
    cancelBtn.addEventListener("click", cancelHandler);
  }

  // --- SSE Log Stream ---
  function startLogStream(loopId, pane) {
    if (eventSource) eventSource.close();
    eventSource = new EventSource("/dashboard/stream/" + loopId);
    eventSource.onmessage = function(e) {
      const line = document.createElement("span");
      line.className = "log-line";
      line.textContent = e.data;
      pane.appendChild(line);
      // Cap lines
      while (pane.children.length > LOG_LINE_CAP) {
        pane.removeChild(pane.firstChild);
      }
      pane.scrollTop = pane.scrollHeight;
    };
    eventSource.onerror = function() {
      eventSource.close();
      eventSource = null;
    };
  }

  // --- Pod Introspect Polling (FR-5) ---
  function startIntrospectPolling(loopId) {
    const details = document.getElementById("pod-introspect");
    if (!details) return;
    details.addEventListener("toggle", function() {
      if (details.open) {
        fetchIntrospect(loopId);
        introspectTimer = setInterval(() => fetchIntrospect(loopId), INTROSPECT_INTERVAL);
      } else {
        if (introspectTimer) { clearInterval(introspectTimer); introspectTimer = null; }
      }
    });
  }

  function fetchIntrospect(loopId) {
    const el = document.getElementById("introspect-data");
    if (!el) return;
    fetch("/pod-introspect/" + loopId, { credentials: "same-origin" })
      .then(r => r.ok ? r.json() : Promise.reject(r.status))
      .then(data => {
        let html = "<pre>" + esc(JSON.stringify(data, null, 2)) + "</pre>";
        el.innerHTML = html;
      })
      .catch(() => { el.textContent = "Unavailable"; });
  }

  // --- Header Menu ---
  function bindHeaderMenu() {
    const btn = document.getElementById("header-menu-toggle");
    const dropdown = document.getElementById("header-menu-dropdown");
    if (!btn || !dropdown) return;
    btn.addEventListener("click", function(e) {
      e.stopPropagation();
      dropdown.classList.toggle("open");
    });
    document.addEventListener("click", function() {
      dropdown.classList.remove("open");
    });
  }

  // --- Bell Toggle ---
  function bindBellToggle() {
    const btn = document.getElementById("bell-toggle");
    if (!btn) return;
    // Initialize label from current state
    btn.textContent = bellEnabled ? "Bell: on" : "Bell: off";
    btn.addEventListener("click", function(e) {
      e.stopPropagation();
      bellEnabled = !bellEnabled;
      btn.textContent = bellEnabled ? "Bell: on" : "Bell: off";
      try { localStorage.setItem("nautiloop_bell", bellEnabled ? "on" : "off"); } catch(e) {}
    });
  }

  // --- Round Row Expand ---
  function bindRoundExpand() {
    document.querySelectorAll(".round-row").forEach(function(row) {
      row.addEventListener("click", function() {
        const detail = row.nextElementSibling;
        if (detail && detail.classList.contains("round-detail")) {
          detail.classList.toggle("open");
        }
      });
    });
  }

  // --- Feed Page ---
  // Persist feed filter selection in localStorage (FR-12b, progressive enhancement).
  function bindFeedFilters() {
    const feedList = document.getElementById("feed-list");
    if (!feedList) return;
    // Wire feed filter chips via data attributes (no inline onclick).
    document.querySelectorAll(".filter-bar [data-feed-filter-type]").forEach(function(chip) {
      chip.addEventListener("click", function() {
        var filterType = chip.getAttribute("data-feed-filter-type");
        var filterVal = chip.getAttribute("data-feed-filter");
        var url = "/dashboard/feed";
        if (filterType === "state" && filterVal) {
          url += "?state_filter=" + encodeURIComponent(filterVal);
        } else if (filterType === "engineer" && filterVal) {
          url += "?engineer_filter=" + encodeURIComponent(filterVal);
        }
        // Persist for restore on next visit
        try {
          localStorage.setItem("nautiloop_feed_filter", filterType + ":" + (filterVal || ""));
        } catch(e) {}
        location.href = url;
      });
    });
  }

  // On feed page load, apply saved filter if no explicit filter is in the URL.
  // If the restored filter yields an empty feed, clear the localStorage value
  // and redirect to the unfiltered view so the user is not stuck.
  function restoreFeedFilter() {
    var feedList = document.getElementById("feed-list");
    if (!feedList) return;
    var params = new URLSearchParams(location.search);
    var hasFilter = params.has("state_filter") || params.has("engineer_filter") || params.has("filter");
    // If we navigated with a filter and the feed has no items, clear the stale filter.
    if (hasFilter && feedList.querySelectorAll(".feed-item").length === 0) {
      try { localStorage.removeItem("nautiloop_feed_filter"); } catch(e) {}
      location.href = "/dashboard/feed";
      return;
    }
    // Only restore if user navigated to /dashboard/feed without any filter param.
    if (!hasFilter && !params.has("cursor")) {
      try {
        var saved = localStorage.getItem("nautiloop_feed_filter");
        if (saved) {
          var parts = saved.split(":");
          var type = parts[0];
          var val = parts.slice(1).join(":");
          if (type === "state" && val) {
            location.href = "/dashboard/feed?state_filter=" + encodeURIComponent(val);
          } else if (type === "engineer" && val) {
            location.href = "/dashboard/feed?engineer_filter=" + encodeURIComponent(val);
          }
        }
      } catch(e) {}
    }
  }

  function bindFeedLoadMore() {
    const btn = document.getElementById("feed-load-more");
    if (!btn) return;
    btn.addEventListener("click", function() {
      const cursor = btn.dataset.cursor;
      const stateFilter = btn.dataset.stateFilter || "";
      const engineerFilter = btn.dataset.engineerFilter || "";
      let url = "/dashboard/feed?limit=50";
      if (cursor) url += "&cursor=" + encodeURIComponent(cursor);
      if (stateFilter) url += "&state_filter=" + encodeURIComponent(stateFilter);
      if (engineerFilter) url += "&engineer_filter=" + encodeURIComponent(engineerFilter);
      fetch(url, { credentials: "same-origin" })
        .then(r => r.json())
        .then(data => {
          const list = document.getElementById("feed-list");
          if (!list) return;
          for (const ev of data.events) {
            list.insertAdjacentHTML("beforeend", renderFeedItem(ev));
          }
          if (data.has_more && data.events.length > 0) {
            const last = data.events[data.events.length - 1];
            btn.dataset.cursor = last.updated_at + "|" + last.id;
          } else {
            btn.style.display = "none";
          }
        })
        .catch(e => console.warn("feed load failed:", e));
    });
  }

  function renderFeedItem(ev) {
    const time = new Date(ev.updated_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const ext = ev.extensions > 0 ? " [extended \u00d7" + ev.extensions + "]" : "";
    return '<a class="feed-item" href="/dashboard/loops/' + esc(ev.id) + '">'
      + '<span class="feed-time">' + esc(time) + '</span>'
      + '<span>' + esc(ev.engineer) + '</span>'
      + '<span class="feed-spec">' + esc(specFilename(ev.spec_path)) + '</span>'
      + '<span class="badge ' + badgeClass(ev.state) + '">' + esc(ev.state) + '</span>'
      + (ev.spec_pr_url ? '<span class="feed-detail">PR</span>' : '')
      + '<span class="feed-detail">' + ev.rounds + ' rounds</span>'
      + '<span class="feed-detail">' + fmtCost(ev.total_cost) + '</span>'
      + '<span class="feed-detail">' + ext + '</span>'
      + '</a>';
  }

  // --- Stats Page ---
  function bindStatsWindow() {
    document.querySelectorAll("[data-window]").forEach(function(btn) {
      btn.addEventListener("click", function() {
        const w = btn.dataset.window;
        window.location.href = "/dashboard/stats?window=" + w;
      });
    });
  }

  // --- Web Audio Bell (Progressive Enhancement, FR-7a) ---
  // Opt-in notification bell when a loop converges. Preference stored in localStorage.
  let bellEnabled = false;
  try { bellEnabled = localStorage.getItem("nautiloop_bell") === "on"; } catch(e) {}

  function playBell() {
    if (!bellEnabled) return;
    try {
      const ctx = new (window.AudioContext || window.webkitAudioContext)();
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.connect(gain);
      gain.connect(ctx.destination);
      osc.type = "sine";
      osc.frequency.value = 880;
      gain.gain.setValueAtTime(0.3, ctx.currentTime);
      gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.3);
      osc.start(ctx.currentTime);
      osc.stop(ctx.currentTime + 0.3);
    } catch(e) {}
  }

  // --- Focus Tracking (Progressive Enhancement) ---
  window.addEventListener("focus", function() {
    convergedSinceLastFocus = 0;
    updateTabTitle();
  });

  // --- Highlight on hash (FR-9c) ---
  function highlightHash() {
    if (!location.hash) return;
    const el = document.getElementById(location.hash.slice(1));
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "center" });
      el.classList.add("highlight-pulse");
      setTimeout(() => el.classList.remove("highlight-pulse"), 2000);
    }
  }

  // --- Init ---
  document.addEventListener("DOMContentLoaded", function() {
    // Card grid page
    if (document.getElementById("card-grid")) {
      // Bind filter chips
      document.querySelectorAll("[data-state-filter]").forEach(function(el) {
        el.addEventListener("click", function() { setStateFilter(el.dataset.stateFilter); });
      });
      document.querySelectorAll("[data-eng-filter]").forEach(function(el) {
        el.addEventListener("click", function() { setEngineerFilter(el.dataset.engFilter); });
      });
      startPolling();
    }

    // Detail page
    const logPane = document.getElementById("log-pane");
    const loopId = document.body.dataset.loopId;
    if (logPane && loopId) {
      const isLive = document.body.dataset.loopTerminal !== "true";
      if (isLive) {
        startLogStream(loopId, logPane);
      }
      logPane.scrollTop = logPane.scrollHeight;
    }
    if (loopId) {
      startIntrospectPolling(loopId);
    }

    bindActions();
    bindHeaderMenu();
    bindBellToggle();
    bindRoundExpand();
    restoreFeedFilter();
    bindFeedFilters();
    bindFeedLoadMore();
    bindStatsWindow();
    highlightHash();
  });

})();
