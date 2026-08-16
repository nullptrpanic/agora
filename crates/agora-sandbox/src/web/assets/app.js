(() => {
  "use strict";

  const elements = {
    connectionDot: document.querySelector("#connection-dot"),
    connectionLabel: document.querySelector("#connection-label"),
    connectionBanner: document.querySelector("#connection-banner"),
    activeTrace: document.querySelector("#active-trace"),
    elapsed: document.querySelector("#elapsed"),
    terminalHost: document.querySelector("#terminal"),
    terminalLoading: document.querySelector("#terminal-loading"),
    terminalSize: document.querySelector("#terminal-size"),
    copyTerminal: document.querySelector("#copy-terminal"),
    stopSession: document.querySelector("#stop-session"),
    startSession: document.querySelector("#start-session"),
    clearTrace: document.querySelector("#clear-trace"),
    eventCount: document.querySelector("#event-count"),
    diagnosticPill: document.querySelector("#diagnostic-pill"),
    diagnosticCount: document.querySelector("#diagnostic-count"),
    traceSearch: document.querySelector("#trace-search"),
    showCloses: document.querySelector("#show-closes"),
    timeline: document.querySelector("#timeline"),
    emptyState: document.querySelector("#empty-state"),
    traceNotice: document.querySelector("#trace-notice"),
    detailPanel: document.querySelector("#detail-panel"),
    detailBadge: document.querySelector("#detail-badge"),
    detailTitle: document.querySelector("#detail-title"),
    detailFields: document.querySelector("#detail-fields"),
    detailRaw: document.querySelector("#detail-raw"),
    closeDetail: document.querySelector("#close-detail"),
  };
  const timelineFollow = window.AgoraTimelineFollow;

  const fragment = new URLSearchParams(window.location.hash.slice(1));
  const fragmentToken = fragment.get("token");
  const historyToken = window.history.state?.agoraViewerToken;
  const token = fragmentToken || historyToken;
  if (fragmentToken) {
    window.history.replaceState(
      { agoraViewerToken: fragmentToken },
      "",
      window.location.pathname + window.location.search,
    );
  }

  const terminal = new Terminal({
    allowProposedApi: false,
    convertEol: false,
    cursorBlink: true,
    cursorStyle: "bar",
    fontFamily: '"SFMono-Regular", "Cascadia Code", "Liberation Mono", Menlo, monospace',
    fontSize: 12,
    lineHeight: 1.25,
    letterSpacing: 0,
    scrollback: 10000,
    theme: {
      background: "#080b0f",
      foreground: "#d8e1ec",
      cursor: "#63e6a7",
      cursorAccent: "#080b0f",
      selectionBackground: "#274b4266",
      black: "#151a21",
      red: "#ff7b86",
      green: "#63e6a7",
      yellow: "#f2bf62",
      blue: "#7da8ff",
      magenta: "#b7a1ff",
      cyan: "#66d9ef",
      white: "#d8e1ec",
      brightBlack: "#657286",
      brightRed: "#ff9ba4",
      brightGreen: "#8af0c1",
      brightYellow: "#ffd58b",
      brightBlue: "#a3c0ff",
      brightMagenta: "#cbbdff",
      brightCyan: "#95e9f6",
      brightWhite: "#ffffff",
    },
  });
  const fitAddon = new FitAddon.FitAddon();
  terminal.loadAddon(fitAddon);
  terminal.open(elements.terminalHost);

  const state = {
    socket: null,
    authenticated: false,
    reconnectAttempt: 0,
    reconnectTimer: null,
    replaying: false,
    diagnostics: [],
    activeRootTraceId: null,
    status: "idle",
    statusMessage: null,
    traceTruncated: false,
    terminalTruncated: false,
    selectedKey: null,
    filters: new Set(["exec", "file", "network"]),
    timelineFollowing: true,
    startedAt: null,
  };
  const traceBatch = window.AgoraTraceBatch.create({
    keyOf: eventKey,
    onFlush: () => {
      renderTimeline();
      renderHeader();
      renderTruncationNotice();
    },
    delayMs: 1000,
    maxEvents: 5000,
  });

  const textEncoder = new TextEncoder();

  function setConnection(kind, label, banner) {
    elements.connectionDot.className = `connection-dot ${kind}`;
    elements.connectionLabel.textContent = label;
    elements.connectionBanner.textContent = banner || "";
    elements.connectionBanner.classList.toggle("hidden", !banner);
  }

  function showNotice(message) {
    elements.traceNotice.textContent = message || "";
    elements.traceNotice.classList.toggle("hidden", !message);
  }

  function sendControl(control) {
    if (state.socket?.readyState === WebSocket.OPEN && state.authenticated) {
      state.socket.send(JSON.stringify(control));
    }
  }

  function connect() {
    if (!token) {
      setConnection("disconnected", "Token missing", "Open the complete URL printed by agora-sandbox web to authenticate this viewer.");
      elements.terminalLoading.querySelector("strong").textContent = "Viewer token missing";
      elements.terminalLoading.querySelector("span").textContent = "Return to the terminal and open the printed URL.";
      return;
    }

    window.clearTimeout(state.reconnectTimer);
    setConnection("", "Connecting", "");
    const socket = new WebSocket(`ws://${window.location.host}/ws`);
    socket.binaryType = "arraybuffer";
    state.socket = socket;
    state.authenticated = false;

    socket.addEventListener("open", () => {
      setConnection("", "Authenticating", "");
      socket.send(JSON.stringify({ type: "auth", token }));
    });

    socket.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        handleControl(event.data);
      } else {
        terminal.write(new Uint8Array(event.data));
      }
    });

    socket.addEventListener("close", (event) => {
      if (state.socket !== socket) return;
      state.authenticated = false;
      const suffix = event.reason ? ` — ${event.reason}` : "";
      setConnection("disconnected", "Disconnected", `Viewer connection lost${suffix}. Reconnecting…`);
      scheduleReconnect();
    });

    socket.addEventListener("error", () => {
      if (state.socket === socket) {
        setConnection("disconnected", "Connection error", "The local viewer is unavailable. Reconnecting…");
      }
    });
  }

  function scheduleReconnect() {
    window.clearTimeout(state.reconnectTimer);
    const delay = Math.min(5000, 300 * (2 ** state.reconnectAttempt));
    state.reconnectAttempt += 1;
    state.reconnectTimer = window.setTimeout(connect, delay);
  }

  function handleControl(raw) {
    let message;
    try {
      message = JSON.parse(raw);
    } catch (_error) {
      showNotice("The viewer received an invalid server message.");
      return;
    }

    switch (message.type) {
      case "replay_start":
        state.authenticated = true;
        state.reconnectAttempt = 0;
        state.replaying = true;
        state.terminalTruncated = Boolean(message.truncated);
        terminal.reset();
        setConnection("connected", "Connected", "");
        elements.terminalLoading.classList.add("hidden");
        break;
      case "replay_end":
        state.replaying = false;
        fitTerminal();
        terminal.focus();
        break;
      case "snapshot":
        traceBatch.replace(Array.isArray(message.traces) ? message.traces : []);
        state.diagnostics = Array.isArray(message.diagnostics) ? message.diagnostics : [];
        state.activeRootTraceId = message.active_root_trace_id || null;
        state.traceTruncated = Boolean(message.trace_truncated);
        state.terminalTruncated = Boolean(message.terminal_truncated);
        setSessionStatus(message.status, message.exit_code, message.message);
        renderAll();
        break;
      case "trace":
        if (message.event) appendTrace(message.event);
        break;
      case "status":
        setSessionStatus(message.status, message.exit_code, message.message);
        if (message.status === "exited" || message.status === "error") traceBatch.flush();
        break;
      case "diagnostic":
        if (message.message) {
          state.diagnostics.push(message.message);
          if (state.diagnostics.length > 100) state.diagnostics.shift();
          renderDiagnostics();
        }
        break;
      case "trace_cleared":
        traceBatch.clear();
        state.diagnostics = [];
        state.activeRootTraceId = null;
        state.traceTruncated = false;
        closeDetail();
        renderAll();
        break;
      default:
        showNotice(`Unsupported viewer message: ${String(message.type || "unknown")}`);
    }
  }

  function setSessionStatus(status, exitCode, message) {
    state.status = status || "idle";
    state.statusMessage = message || null;
    if (state.status === "starting") {
      state.startedAt = Date.now();
      state.activeRootTraceId = null;
      renderHeader();
      if (!state.replaying) terminal.reset();
    } else if (state.status === "running" && !state.startedAt) {
      state.startedAt = Date.now();
    }

    const active = state.status === "running" || state.status === "starting";
    elements.stopSession.classList.toggle("hidden", !active);
    elements.startSession.classList.toggle("hidden", active || state.status === "idle");

    if (state.status === "exited") {
      const result = Number.isInteger(exitCode) ? `exit ${exitCode}` : "process exited";
      showNotice(`Sandbox session finished (${result}). Terminal scrollback and trace remain available.`);
    } else if (state.status === "error") {
      showNotice(message || "The sandbox terminal could not be started.");
    } else if (state.traceTruncated || state.terminalTruncated) {
      renderTruncationNotice();
    } else {
      showNotice("");
    }
  }

  function appendTrace(event) {
    if (traceBatch.append(event)) state.traceTruncated = true;
    if (!state.activeRootTraceId) state.activeRootTraceId = event.root_trace_id;
  }

  function eventKey(event) {
    return `${event.id}:${event.root_trace_id}:${event.occurred_at}`;
  }

  function renderAll() {
    renderHeader();
    renderDiagnostics();
    renderTimeline();
    renderTruncationNotice();
  }

  function renderHeader() {
    elements.eventCount.textContent = String(traceBatch.size);
    elements.activeTrace.textContent = state.activeRootTraceId || "Waiting for events";
    elements.activeTrace.title = state.activeRootTraceId || "";
  }

  function renderDiagnostics() {
    elements.diagnosticCount.textContent = String(state.diagnostics.length);
    elements.diagnosticPill.classList.toggle("hidden", state.diagnostics.length === 0);
    elements.diagnosticPill.title = state.diagnostics.at(-1) || "";
  }

  function renderTruncationNotice() {
    if (state.status === "exited" || state.status === "error") return;
    const notices = [];
    if (state.terminalTruncated) notices.push("older terminal output was dropped from in-memory replay");
    if (state.traceTruncated) notices.push("older trace events were dropped from this view");
    showNotice(notices.length ? `Viewer limit reached: ${notices.join("; ")}. The sandbox log is unchanged.` : "");
  }

  function visibleEvents() {
    const query = elements.traceSearch.value.trim().toLocaleLowerCase();
    return traceBatch.values().filter((event) => {
      const category = event.kind === "network" ? "network" : event.kind === "exec" ? "exec" : "file";
      if (!state.filters.has(category)) return false;
      if (event.kind === "file_close" && !elements.showCloses.checked) return false;
      if (!query) return true;
      return `${event.title} ${event.root_trace_id} ${JSON.stringify(event.detail)}`
        .toLocaleLowerCase()
        .includes(query);
    });
  }

  function renderTimeline() {
    const previousScrollTop = elements.timeline.scrollTop;
    const events = visibleEvents();
    const fragmentNode = document.createDocumentFragment();
    if (events.length === 0) {
      const hasSourceEvents = traceBatch.size > 0;
      elements.emptyState.querySelector("strong").textContent = hasSourceEvents ? "No events match these filters" : "Waiting for runtime activity";
      elements.emptyState.querySelector("p").textContent = hasSourceEvents
        ? "Adjust the event types, search text, or close-event toggle to reveal more activity."
        : "Run a command in the terminal. Process execution, opened files, and network destinations will appear here.";
      fragmentNode.append(elements.emptyState);
    } else {
      let previousRoot = null;
      for (const event of events) {
        if (event.root_trace_id !== previousRoot) {
          fragmentNode.append(createRootDivider(event.root_trace_id));
          previousRoot = event.root_trace_id;
        }
        fragmentNode.append(createEventRow(event));
      }
    }
    elements.timeline.replaceChildren(fragmentNode);
    timelineFollow.restoreAfterRender(elements.timeline, state.timelineFollowing, previousScrollTop);
  }

  function createRootDivider(rootTraceId) {
    const divider = document.createElement("div");
    divider.className = "root-divider";
    if (rootTraceId === state.activeRootTraceId) divider.classList.add("active-root");
    divider.textContent = rootTraceId === state.activeRootTraceId ? `Active · ${rootTraceId}` : `Trace · ${rootTraceId}`;
    divider.title = rootTraceId;
    return divider;
  }

  function createEventRow(event) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "timeline-event";
    row.dataset.eventKey = eventKey(event);
    if (row.dataset.eventKey === state.selectedKey) row.classList.add("selected");

    const badge = document.createElement("span");
    badge.className = `event-badge kind-${event.kind}`;
    badge.textContent = kindLabel(event.kind);

    const copy = document.createElement("span");
    copy.className = "event-copy";
    const title = document.createElement("span");
    title.className = "event-title";
    title.textContent = event.title;
    title.title = event.title;
    const meta = document.createElement("span");
    meta.className = "event-meta";
    meta.textContent = eventMeta(event);
    copy.append(title, meta);

    const time = document.createElement("time");
    time.className = "event-time";
    time.dateTime = event.occurred_at;
    time.textContent = formatTime(event.occurred_at);
    row.append(badge, copy, time);
    row.addEventListener("click", () => openDetail(event));
    return row;
  }

  function kindLabel(kind) {
    return {
      exec: "EXEC",
      file_open: "FILE OPEN",
      file_close: "FILE CLOSE",
      network: "NETWORK",
    }[kind] || String(kind || "EVENT").toUpperCase();
  }

  function eventMeta(event) {
    const detail = event.detail || {};
    const parts = [];
    if (detail.pid !== undefined) parts.push(`pid ${detail.pid}`);
    if (detail.ppid !== undefined) parts.push(`ppid ${detail.ppid}`);
    if (detail.current_dir) parts.push(detail.current_dir);
    if (detail.mode?.access) parts.push(detail.mode.access);
    if (detail.destination_ip) parts.push(detail.destination_ip);
    return parts.join(" · ") || event.root_trace_id;
  }

  function formatTime(value) {
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return String(value);
    return parsed.toLocaleTimeString([], {
      hour12: false,
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      fractionalSecondDigits: 3,
    });
  }

  function openDetail(event) {
    state.selectedKey = eventKey(event);
    elements.detailPanel.classList.remove("hidden");
    elements.detailBadge.className = `event-badge kind-${event.kind}`;
    elements.detailBadge.textContent = kindLabel(event.kind);
    elements.detailTitle.textContent = event.title;
    elements.detailTitle.title = event.title;
    elements.detailFields.replaceChildren();

    const fields = [
      ["Time", event.occurred_at],
      ["Root trace", event.root_trace_id],
      ...Object.entries(event.detail || {}).map(([key, value]) => [key, displayValue(value)]),
    ];
    for (const [name, value] of fields) {
      const term = document.createElement("dt");
      term.textContent = String(name).replaceAll("_", " ");
      const definition = document.createElement("dd");
      definition.textContent = value;
      elements.detailFields.append(term, definition);
    }
    elements.detailRaw.textContent = JSON.stringify(event.detail, null, 2);
    traceBatch.flush();
  }

  function displayValue(value) {
    if (value === null) return "null";
    if (typeof value === "object") return JSON.stringify(value);
    return String(value);
  }

  function closeDetail() {
    state.selectedKey = null;
    elements.detailPanel.classList.add("hidden");
    traceBatch.flush();
  }

  function fitTerminal() {
    try {
      fitAddon.fit();
      const cols = Math.max(2, Math.min(500, terminal.cols));
      const rows = Math.max(2, Math.min(500, terminal.rows));
      elements.terminalSize.textContent = `${cols} × ${rows}`;
      sendControl({ type: "resize", cols, rows });
    } catch (_error) {
      // The fit addon can run before the panel receives dimensions during page startup.
    }
  }

  terminal.onData((data) => {
    if (state.socket?.readyState === WebSocket.OPEN && state.authenticated) {
      state.socket.send(textEncoder.encode(data));
    }
  });

  terminal.onBinary((data) => {
    if (state.socket?.readyState !== WebSocket.OPEN || !state.authenticated) return;
    const bytes = new Uint8Array(data.length);
    for (let index = 0; index < data.length; index += 1) bytes[index] = data.charCodeAt(index) & 255;
    state.socket.send(bytes);
  });

  const resizeObserver = new ResizeObserver(() => window.requestAnimationFrame(fitTerminal));
  resizeObserver.observe(elements.terminalHost);

  elements.timeline.addEventListener("scroll", () => {
    state.timelineFollowing = timelineFollow.isAtBottom(elements.timeline);
  });

  document.querySelectorAll(".filter-chip").forEach((button) => {
    button.addEventListener("click", () => {
      const filter = button.dataset.filter;
      if (state.filters.has(filter)) state.filters.delete(filter);
      else state.filters.add(filter);
      button.classList.toggle("active", state.filters.has(filter));
      traceBatch.flush();
    });
  });

  elements.traceSearch.addEventListener("input", () => traceBatch.flush());
  elements.showCloses.addEventListener("change", () => traceBatch.flush());
  elements.closeDetail.addEventListener("click", closeDetail);
  elements.stopSession.addEventListener("click", () => sendControl({ type: "stop" }));
  elements.startSession.addEventListener("click", () => sendControl({ type: "start" }));
  elements.clearTrace.addEventListener("click", () => sendControl({ type: "clear_trace" }));
  elements.copyTerminal.addEventListener("click", async () => {
    const selection = terminal.getSelection();
    if (!selection) {
      showNotice("Select terminal text before copying.");
      return;
    }
    try {
      await navigator.clipboard.writeText(selection);
      showNotice("Terminal selection copied.");
      window.setTimeout(renderTruncationNotice, 1400);
    } catch (_error) {
      showNotice("Clipboard access was denied by the browser.");
    }
  });

  window.setInterval(() => {
    if (!state.startedAt) {
      elements.elapsed.textContent = "00:00";
      return;
    }
    const elapsedSeconds = Math.max(0, Math.floor((Date.now() - state.startedAt) / 1000));
    const minutes = Math.floor(elapsedSeconds / 60).toString().padStart(2, "0");
    const seconds = (elapsedSeconds % 60).toString().padStart(2, "0");
    elements.elapsed.textContent = `${minutes}:${seconds}`;
  }, 1000);

  fitTerminal();
  connect();
})();
