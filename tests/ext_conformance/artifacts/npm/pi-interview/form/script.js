(() => {
  const data = window.__INTERVIEW_DATA__ || {};
  const questions = Array.isArray(data.questions) ? data.questions : [];
  const sessionToken = data.sessionToken || "";
  const sessionId = data.sessionId || "";
  const cwd = data.cwd || "";
  const gitBranch = data.gitBranch || "";
  const timeout = typeof data.timeout === "number" ? data.timeout : 0;

  const titleEl = document.getElementById("form-title");
  const descriptionEl = document.getElementById("form-description");
  const containerEl = document.getElementById("questions-container");
  const formEl = document.getElementById("interview-form");
  
  const submitBtn = document.getElementById("submit-btn");
  const errorContainer = document.getElementById("error-container");
  const successOverlay = document.getElementById("success-overlay");
  const expiredOverlay = document.getElementById("expired-overlay");
  const closeTabBtn = document.getElementById("close-tab-btn");
  const countdownBadge = document.getElementById("countdown-badge");
  const countdownValue = countdownBadge?.querySelector(".countdown-value");
  const countdownRingProgress = countdownBadge?.querySelector(".countdown-ring-progress");
  const closeCountdown = document.getElementById("close-countdown");
  const stayBtn = document.getElementById("stay-btn");
  const queueToast = document.getElementById("queue-toast");
  const queueToastTitle = queueToast?.querySelector(".queue-toast-header span");
  const queueToastClose = queueToast?.querySelector(".queue-toast-close");
  const queueSessionSelect = document.getElementById("queue-session-select");
  const queueOpenBtn = document.getElementById("queue-open-btn");

  const MAX_SIZE = 5 * 1024 * 1024;
  const MAX_DIMENSION = 4096;
  const MAX_IMAGES = 12;
  const ALLOWED_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"];

  const imageState = new Map();
  const imagePathState = new Map();
  const attachState = new Map();
  const attachPathState = new Map();
  const nav = {
    questionIndex: 0,
    optionIndex: 0,
    inSubmitArea: false,
    cards: [],
  };

  const session = {
    storageKey: null,
    expired: false,
    countdownEndTime: 0,
    tickLoopRunning: false,
    ended: false,
    cancelSent: false,
    reloadIntent: false,
  };
  const timers = {
    save: null,
    countdown: null,
    expiration: null,
    heartbeat: null,
    queuePoll: null,
  };
  let filePickerOpen = false;
  const CLOSE_DELAY = 10;
  const RING_CIRCUMFERENCE = 100.53;
  const RELOAD_INTENT_KEY = "pi-interview-reload-intent";
  const queueState = {
    dismissed: false,
    knownIds: new Set(),
  };

  function updateCountdownBadge(secondsLeft, totalSeconds) {
    if (!countdownBadge || !countdownValue || !countdownRingProgress) return;
    
    countdownValue.textContent = formatTime(secondsLeft);
    const progress = (totalSeconds - secondsLeft) / totalSeconds;
    countdownRingProgress.style.strokeDashoffset = RING_CIRCUMFERENCE * progress;
  }

  function formatTime(seconds) {
    const mins = Math.floor(seconds / 60);
    const secs = seconds % 60;
    if (mins > 0) {
      return `${mins}:${secs.toString().padStart(2, "0")}`;
    }
    return String(secs);
  }

  function startCountdownDisplay() {
    if (!countdownBadge || timeout <= 0) return;
    
    const expandThreshold = 120;
    const urgentThreshold = 30;
    session.countdownEndTime = Date.now() + (timeout * 1000);
    
    countdownBadge.classList.remove("hidden");
    countdownBadge.classList.add("minimal");
    
    if (session.tickLoopRunning) return;
    session.tickLoopRunning = true;
    
    const tick = () => {
      const now = Date.now();
      const remaining = Math.max(0, Math.ceil((session.countdownEndTime - now) / 1000));
      
      updateCountdownBadge(remaining, timeout);
      
      if (remaining <= expandThreshold) {
        countdownBadge.classList.remove("minimal");
      }
      
      if (remaining <= urgentThreshold) {
        countdownBadge.classList.add("urgent");
      } else {
        countdownBadge.classList.remove("urgent");
      }
      
      if (remaining > 0 && !session.expired) {
        requestAnimationFrame(tick);
      } else {
        session.tickLoopRunning = false;
      }
    };
    
    requestAnimationFrame(tick);
  }

  function refreshCountdown() {
    if (session.expired || timeout <= 0) return;
    session.countdownEndTime = Date.now() + (timeout * 1000);
    countdownBadge?.classList.add("minimal");
    countdownBadge?.classList.remove("urgent");
    
    if (timers.expiration) {
      clearTimeout(timers.expiration);
    }
    timers.expiration = setTimeout(() => {
      showSessionExpired();
    }, timeout * 1000);
  }

  function showSessionExpired() {
    if (session.expired) return;
    session.expired = true;
    session.tickLoopRunning = false;
    
    submitBtn.disabled = true;
    countdownBadge?.classList.add("hidden");
    
    expiredOverlay.classList.remove("hidden");
    requestAnimationFrame(() => {
      expiredOverlay.classList.add("visible");
      stayBtn.focus();
    });
    
    let closeIn = CLOSE_DELAY;
    if (closeCountdown) closeCountdown.textContent = closeIn;
    
    timers.countdown = setInterval(() => {
      closeIn--;
      if (closeCountdown) closeCountdown.textContent = closeIn;
      
      if (closeIn <= 0) {
        clearInterval(timers.countdown);
        cancelInterview("timeout").finally(() => window.close());
      }
    }, 1000);
  }

  function startHeartbeat() {
    if (timers.heartbeat) return;
    timers.heartbeat = setInterval(() => {
      fetch("/heartbeat", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: sessionToken }),
      }).catch(() => {});
    }, 5000);
  }

  function stopHeartbeat() {
    if (timers.heartbeat) {
      clearInterval(timers.heartbeat);
      timers.heartbeat = null;
    }
  }

  function stopQueuePolling() {
    if (timers.queuePoll) {
      clearInterval(timers.queuePoll);
      timers.queuePoll = null;
    }
  }

  function formatRelativeTime(timestamp) {
    const seconds = Math.floor((Date.now() - timestamp) / 1000);
    if (seconds < 0) return "just now";
    if (seconds < 60) return `${seconds}s ago`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    return `${hours}h ago`;
  }

  function truncateText(text, maxLength) {
    if (!text || text.length <= maxLength) return text;
    const head = Math.ceil((maxLength - 3) * 0.6);
    const tail = Math.floor((maxLength - 3) * 0.4);
    return `${text.slice(0, head)}...${text.slice(-tail)}`;
  }

  function formatSessionLabel(session) {
    const status = session.status === "active" ? "Active" : "Waiting";
    const branch = session.gitBranch ? ` (${session.gitBranch})` : "";
    const project = session.cwd ? truncateText(session.cwd + branch, 36) : "Unknown";
    const title = truncateText(session.title || "Interview", 32);
    const timeAgo = formatRelativeTime(session.startedAt);
    return `${status}: ${title} — ${project} · ${timeAgo}`;
  }

  function updateQueueToast(sessions) {
    if (!queueToast || !queueSessionSelect || !queueOpenBtn) return;
    const others = sessions.filter((s) => s.id !== sessionId);
    if (others.length === 0) {
      queueToast.classList.add("hidden");
      queueState.dismissed = false;
      queueState.knownIds.clear();
      return;
    }

    const newIds = others.filter((s) => !queueState.knownIds.has(s.id));
    others.forEach((s) => queueState.knownIds.add(s.id));
    if (newIds.length > 0) {
      queueState.dismissed = false;
    }

    if (queueState.dismissed) return;

    const currentSession = sessions.find((s) => s.id === sessionId);
    const sortedOthers = others.slice().sort((a, b) => b.startedAt - a.startedAt);
    const sorted = currentSession ? [currentSession, ...sortedOthers] : sortedOthers;
    const currentValue = queueSessionSelect.value;
    queueSessionSelect.innerHTML = "";
    sorted.forEach((session) => {
      const option = document.createElement("option");
      option.value = session.url;
      if (session.id === sessionId) {
        const branch = session.gitBranch ? ` (${session.gitBranch})` : "";
        const project = session.cwd ? truncateText(session.cwd + branch, 36) : "Unknown";
        const title = truncateText(session.title || "Interview", 32);
        const timeAgo = formatRelativeTime(session.startedAt);
        option.textContent = `Active (this tab): ${title} — ${project} · ${timeAgo}`;
        option.disabled = true;
      } else {
        option.textContent = formatSessionLabel(session);
      }
      queueSessionSelect.appendChild(option);
    });
    const selectedSession =
      (currentValue && sorted.find((s) => s.url === currentValue && s.id !== sessionId)) ||
      sorted.find((s) => s.id !== sessionId);
    if (selectedSession) {
      queueSessionSelect.value = selectedSession.url;
    }

    if (queueToastTitle) {
      queueToastTitle.textContent =
        others.length === 1 ? "Another interview started" : `${others.length} interviews waiting`;
    }

    const selectedOption = queueSessionSelect.options[queueSessionSelect.selectedIndex];
    queueOpenBtn.disabled = !queueSessionSelect.value || selectedOption?.disabled;
    queueToast.classList.remove("hidden");
  }

  async function pollQueueSessions() {
    try {
      const response = await fetch(`/sessions?session=${encodeURIComponent(sessionToken)}`, {
        method: "GET",
        headers: { "Accept": "application/json" },
        cache: "no-store",
      });
      if (!response.ok) return;
      const data = await response.json();
      if (!data || !data.ok || !Array.isArray(data.sessions)) return;
      updateQueueToast(data.sessions);
    } catch (_err) {}
  }

  function startQueuePolling() {
    if (!queueToast || timers.queuePoll) return;
    pollQueueSessions();
    timers.queuePoll = setInterval(pollQueueSessions, 6000);
  }

  function markReloadIntent() {
    session.reloadIntent = true;
    try {
      sessionStorage.setItem(RELOAD_INTENT_KEY, "1");
      setTimeout(() => {
        sessionStorage.removeItem(RELOAD_INTENT_KEY);
      }, 2000);
    } catch (_err) {}
  }

  function clearReloadIntent() {
    session.reloadIntent = false;
    try {
      sessionStorage.removeItem(RELOAD_INTENT_KEY);
    } catch (_err) {}
  }

  function hasReloadIntent() {
    if (session.reloadIntent) return true;
    try {
      return sessionStorage.getItem(RELOAD_INTENT_KEY) === "1";
    } catch (_err) {
      return false;
    }
  }

  function sendCancelBeacon(reason) {
    if (session.cancelSent || session.ended) return;
    session.cancelSent = true;
    const responses = collectResponses();
    const payload = JSON.stringify({ token: sessionToken, reason, responses });
    if (navigator.sendBeacon) {
      const blob = new Blob([payload], { type: "application/json" });
      navigator.sendBeacon("/cancel", blob);
      return;
    }
    fetch("/cancel", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: payload,
      keepalive: true,
    }).catch(() => {});
  }

  async function cancelInterview(reason) {
    if (session.ended) return;
    session.ended = true;
    session.cancelSent = true;
    stopHeartbeat();
    stopQueuePolling();
    const responses = collectResponses();
    try {
      await fetch("/cancel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: sessionToken, reason, responses }),
      });
    } catch (_err) {}
  }

  function isNetworkError(err) {
    if (err instanceof TypeError) return true;
    if (err.name === "TypeError") return true;
    const msg = String(err.message || "").toLowerCase();
    return msg.includes("fetch") || msg.includes("network") || msg.includes("failed to fetch");
  }

  function escapeSelector(value) {
    if (window.CSS && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return String(value).replace(/["\\]/g, "\\$&");
  }

  function setText(el, text) {
    if (!el) return;
    el.textContent = text || "";
  }

  function renderLightMarkdown(text) {
    if (!text) return "";
    let html = text
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
    html = html.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
    html = html.replace(/`([^`]+)`/g, "<code>$1</code>");
    html = html.replace(/\n/g, "<br>");
    html = html.replace(/\s(\d+\.)\s/g, "<br>$1 ");
    return html;
  }

  function getOptionLabel(option) {
    return typeof option === "string" ? option : option.label;
  }

  function isRichOption(option) {
    return typeof option === "object" && option !== null && "label" in option;
  }

  function renderCodeBlock(block) {
    if (!block || !block.code) return null;

    const container = document.createElement("div");
    container.className = "code-block";

    const showLineNumbers = !!block.file || !!block.lines;
    const isDiff = block.lang === "diff";
    const lines = block.code.split("\n");
    const highlights = new Set(block.highlights || []);
    
    // Parse starting line number from lines prop (e.g., "10-16" -> 10, "42" -> 42)
    let startLineNum = 1;
    if (block.lines) {
      const match = block.lines.match(/^(\d+)/);
      if (match) startLineNum = parseInt(match[1], 10);
    }

    if (block.file || block.lines || block.lang || block.title) {
      const header = document.createElement("div");
      header.className = "code-block-header";

      if (block.title) {
        const titleEl = document.createElement("span");
        titleEl.className = "code-block-title";
        titleEl.textContent = block.title;
        header.appendChild(titleEl);
      }

      if (block.file) {
        const fileEl = document.createElement("span");
        fileEl.className = "code-block-file";
        fileEl.textContent = block.file;
        header.appendChild(fileEl);
      }

      if (block.lines) {
        const linesEl = document.createElement("span");
        linesEl.className = "code-block-lines";
        linesEl.textContent = `L${block.lines}`;
        header.appendChild(linesEl);
      }

      if (block.lang && block.lang !== "diff") {
        const langEl = document.createElement("span");
        langEl.className = "code-block-lang";
        langEl.textContent = block.lang;
        header.appendChild(langEl);
      }

      container.appendChild(header);
    }

    const pre = document.createElement("pre");
    const code = document.createElement("code");

    if (showLineNumbers || isDiff || highlights.size > 0) {
      const linesContainer = document.createElement("div");
      linesContainer.className = "code-block-lines-container";

      lines.forEach((lineText, i) => {
        const lineNum = startLineNum + i;
        const lineEl = document.createElement("div");
        lineEl.className = "code-block-line";

        if (highlights.has(i + 1)) {
          lineEl.classList.add("highlighted");
        }

        if (isDiff) {
          if (lineText.startsWith("+") && !lineText.startsWith("+++")) {
            lineEl.classList.add("diff-add");
          } else if (lineText.startsWith("-") && !lineText.startsWith("---")) {
            lineEl.classList.add("diff-remove");
          } else if (lineText.startsWith("@@") || lineText.startsWith("---") || lineText.startsWith("+++")) {
            lineEl.classList.add("diff-header");
          }
        }

        if (showLineNumbers) {
          const numEl = document.createElement("span");
          numEl.className = "code-block-line-number";
          numEl.textContent = String(lineNum);
          lineEl.appendChild(numEl);
        }

        const contentEl = document.createElement("span");
        contentEl.className = "code-block-line-content";
        contentEl.textContent = lineText;
        lineEl.appendChild(contentEl);

        linesContainer.appendChild(lineEl);
      });

      code.appendChild(linesContainer);
    } else {
      code.textContent = block.code;
    }

    pre.appendChild(code);
    container.appendChild(pre);

    return container;
  }

  function isPrintableKey(event) {
    if (event.metaKey || event.ctrlKey || event.altKey) return false;
    return event.key.length === 1;
  }

  function maybeStartOtherInput(event) {
    const active = document.activeElement;
    if (!(active instanceof HTMLInputElement)) return false;
    if ((active.type !== "radio" && active.type !== "checkbox") || active.value !== "__other__") return false;
    if (!isPrintableKey(event)) return false;
    const card = active.closest(".question-card");
    const otherInput = card?.querySelector(".other-input");
    if (!otherInput) return false;

    event.preventDefault();
    if (!active.checked) {
      active.checked = true;
      const question = questions.find((q) => q.id === active.name);
      if (question?.type === "multi") updateDoneState(active.name);
      debounceSave();
    }
    otherInput.focus();
    otherInput.value += event.key;
    otherInput.dispatchEvent(new Event("input", { bubbles: true }));
    return true;
  }

  const themeConfig = data.theme || {};
  const themeMode = themeConfig.mode || "dark";
  const themeToggleHotkey =
    typeof themeConfig.toggleHotkey === "string" ? themeConfig.toggleHotkey : "";
  const themeLinkLight = document.querySelector('link[data-theme-link="light"]');
  const themeLinkDark = document.querySelector('link[data-theme-link="dark"]');
  const THEME_OVERRIDE_KEY = "pi-interview-theme-override";

  function getSystemTheme() {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }

  function getStoredThemeOverride() {
    const value = localStorage.getItem(THEME_OVERRIDE_KEY);
    return value === "light" || value === "dark" ? value : null;
  }

  function setStoredThemeOverride(value) {
    if (!value) {
      localStorage.removeItem(THEME_OVERRIDE_KEY);
      return;
    }
    localStorage.setItem(THEME_OVERRIDE_KEY, value);
  }

  function setThemeLinkEnabled(link, enabled) {
    if (!link) return;
    link.disabled = !enabled;
    link.media = enabled ? "all" : "not all";
  }

  function applyTheme(mode) {
    document.documentElement.dataset.theme = mode;
    setThemeLinkEnabled(themeLinkLight, mode === "light");
    setThemeLinkEnabled(themeLinkDark, mode === "dark");
  }

  function getEffectiveThemeMode() {
    const override = getStoredThemeOverride();
    if (override) return override;
    if (themeMode === "auto") return getSystemTheme();
    return themeMode;
  }

  function parseHotkey(value) {
    if (!value) return null;
    const parts = value.toLowerCase().split("+").map(part => part.trim()).filter(Boolean);
    if (parts.length === 0) return null;
    const key = parts[parts.length - 1];
    const mods = parts.slice(0, -1);
    const hotkey = { key, mod: false, shift: false, alt: false };

    mods.forEach((mod) => {
      if (mod === "mod" || mod === "cmd" || mod === "meta" || mod === "ctrl" || mod === "control") {
        hotkey.mod = true;
      } else if (mod === "shift") {
        hotkey.shift = true;
      } else if (mod === "alt" || mod === "option") {
        hotkey.alt = true;
      }
    });

    return key ? hotkey : null;
  }

  function updateThemeShortcutDisplay(hotkey) {
    const shortcut = document.querySelector("[data-theme-shortcut]");
    if (!shortcut) return;
    if (!hotkey) {
      shortcut.classList.add("hidden");
      return;
    }

    const keysEl = shortcut.querySelector("[data-theme-keys]");
    if (!keysEl) return;
    keysEl.innerHTML = "";

    const isMac = navigator.platform.toUpperCase().indexOf("MAC") >= 0;
    const parts = [];
    if (hotkey.mod) parts.push(isMac ? "⌘" : "Ctrl");
    if (hotkey.shift) parts.push("Shift");
    if (hotkey.alt) parts.push(isMac ? "Option" : "Alt");
    parts.push(hotkey.key.length === 1 ? hotkey.key.toUpperCase() : hotkey.key.toUpperCase());

    parts.forEach((part) => {
      const kbd = document.createElement("kbd");
      kbd.textContent = part;
      keysEl.appendChild(kbd);
    });

    shortcut.classList.remove("hidden");
  }

  function matchesHotkey(event, hotkey) {
    const key = event.key.toLowerCase();
    if (key !== hotkey.key) return false;
    const isMac = navigator.platform.toUpperCase().indexOf("MAC") >= 0;
    const modPressed = isMac ? event.metaKey : event.ctrlKey;
    if (hotkey.mod !== modPressed) return false;
    if (hotkey.shift !== event.shiftKey) return false;
    if (hotkey.alt !== event.altKey) return false;
    if (!hotkey.mod && (event.metaKey || event.ctrlKey)) return false;
    if (!hotkey.shift && event.shiftKey) return false;
    if (!hotkey.alt && event.altKey) return false;
    return true;
  }

  function toggleTheme() {
    const current = getEffectiveThemeMode();
    const next = current === "dark" ? "light" : "dark";
    if (themeMode === "auto") {
      const system = getSystemTheme();
      if (next === system) {
        setStoredThemeOverride(null);
      } else {
        setStoredThemeOverride(next);
      }
    } else {
      setStoredThemeOverride(next);
    }
    applyTheme(next);
  }

  function initTheme() {
    applyTheme(getEffectiveThemeMode());

    if (themeMode === "auto") {
      const media = window.matchMedia("(prefers-color-scheme: dark)");
      media.addEventListener("change", () => {
        if (!getStoredThemeOverride()) {
          applyTheme(getSystemTheme());
        }
      });
    }

    const hotkey = parseHotkey(themeToggleHotkey);
    updateThemeShortcutDisplay(hotkey);
    if (hotkey) {
      document.addEventListener("keydown", (event) => {
        if (matchesHotkey(event, hotkey)) {
          event.preventDefault();
          toggleTheme();
        }
      });
    }
  }

  function normalizePath(path) {
    let normalized = path.replace(/\\ /g, " ");  // Shell escape: backslash-space to space
    // macOS screenshots use narrow no-break space (\u202f) before AM/PM in "Screenshot YYYY-MM-DD at H.MM.SS AM/PM.png"
    normalized = normalized.replace(/(\d{1,2}\.\d{2}\.\d{2}) (AM|PM)(\.\w+)?$/i, "$1\u202f$2$3");
    return normalized;
  }

  function debounceSave() {
    if (timers.save) {
      window.clearTimeout(timers.save);
    }
    timers.save = window.setTimeout(() => {
      saveProgress();
    }, 500);
  }

  function createImageManager(options) {
    const {
      fileState,
      pathState,
      containerSelector,
      onUpdate,
      onRenderComplete,
      removeLabel = "×",
    } = options;

    const manager = {
      render(questionId) {
        const container = document.querySelector(containerSelector(questionId));
        if (!container) return;
        container.innerHTML = "";

        const entry = fileState.get(questionId);
        if (entry) {
          const item = document.createElement("div");
          item.className = "selected-item selected-image";

          const img = document.createElement("img");
          const url = URL.createObjectURL(entry.file);
          img.src = url;
          img.onload = () => URL.revokeObjectURL(url);

          const name = document.createElement("span");
          name.className = "selected-item-name";
          name.textContent = entry.file.name;

          const removeBtn = document.createElement("button");
          removeBtn.type = "button";
          removeBtn.className = "selected-item-remove";
          removeBtn.textContent = removeLabel;
          removeBtn.addEventListener("click", () => {
            fileState.delete(questionId);
            manager.render(questionId);
            onUpdate();
          });

          item.appendChild(img);
          item.appendChild(name);
          item.appendChild(removeBtn);
          container.appendChild(item);
        }

        const paths = pathState.get(questionId) || [];
        paths.forEach(path => {
          const item = document.createElement("div");
          item.className = "selected-item selected-path";

          const pathText = document.createElement("span");
          pathText.className = "selected-item-path";
          pathText.textContent = path;

          const removeBtn = document.createElement("button");
          removeBtn.type = "button";
          removeBtn.className = "selected-item-remove";
          removeBtn.textContent = removeLabel;
          removeBtn.addEventListener("click", () => {
            const arr = pathState.get(questionId) || [];
            const idx = arr.indexOf(path);
            if (idx > -1) arr.splice(idx, 1);
            manager.render(questionId);
            onUpdate();
          });

          item.appendChild(pathText);
          item.appendChild(removeBtn);
          container.appendChild(item);
        });

        if (onRenderComplete) onRenderComplete(questionId, manager);
      },

      addFile(questionId, file) {
        fileState.set(questionId, { file });
        manager.render(questionId);
        onUpdate();
      },

      removeFile(questionId) {
        fileState.delete(questionId);
        manager.render(questionId);
        onUpdate();
      },

      addPath(questionId, path) {
        const paths = pathState.get(questionId) || [];
        if (!paths.includes(path)) {
          paths.push(path);
          pathState.set(questionId, paths);
          manager.render(questionId);
          onUpdate();
        }
      },

      removePath(questionId, path) {
        const paths = pathState.get(questionId) || [];
        const index = paths.indexOf(path);
        if (index > -1) {
          paths.splice(index, 1);
          pathState.set(questionId, paths);
          manager.render(questionId);
          onUpdate();
        }
      },

      getFile(questionId) {
        return fileState.get(questionId);
      },

      getPaths(questionId) {
        return pathState.get(questionId) || [];
      },

      hasContent(questionId) {
        return fileState.has(questionId) || (pathState.get(questionId) || []).length > 0;
      },

      countFiles() {
        return fileState.size;
      },
    };

    return manager;
  }

  const questionImages = createImageManager({
    fileState: imageState,
    pathState: imagePathState,
    containerSelector: (id) => `[data-selected-for="${escapeSelector(id)}"]`,
    onUpdate: debounceSave,
  });

  const attachments = createImageManager({
    fileState: attachState,
    pathState: attachPathState,
    containerSelector: (id) => `[data-attach-items-for="${escapeSelector(id)}"]`,
    onUpdate: debounceSave,
    removeLabel: "x",
    onRenderComplete: (questionId, manager) => {
      const btn = document.querySelector(
        `.attach-btn[data-question-id="${escapeSelector(questionId)}"]`
      );
      const panel = document.querySelector(
        `[data-attach-inline-for="${escapeSelector(questionId)}"]`
      );
      const hasContent = manager.hasContent(questionId);
      if (btn) btn.classList.toggle("has-attachment", hasContent);
      if (panel && hasContent) panel.classList.remove("hidden");
    },
  });

  function updateDoneState(questionId) {
    const doneItem = document.querySelector(`[data-done-for="${escapeSelector(questionId)}"]`);
    if (!doneItem) return;
    const hasSelection = document.querySelectorAll(`input[name="${escapeSelector(questionId)}"]:checked`).length > 0;
    doneItem.classList.toggle("disabled", !hasSelection);
  }

  function clearGlobalError() {
    if (!errorContainer) return;
    errorContainer.textContent = "";
    errorContainer.classList.add("hidden");
  }

  function showGlobalError(message) {
    if (!errorContainer) return;
    errorContainer.textContent = message;
    errorContainer.classList.remove("hidden");
  }

  function setFieldError(id, message) {
    const field = document.querySelector(`[data-error-for="${escapeSelector(id)}"]`);
    if (!field) return;
    field.textContent = message || "";
  }

  function clearFieldErrors() {
    const fields = document.querySelectorAll(".field-error");
    fields.forEach((el) => {
      el.textContent = "";
    });
  }

  const formFooter = document.querySelector('.form-footer');

  function getOptionsForCard(card) {
    const inputs = Array.from(card.querySelectorAll('input[type="radio"], input[type="checkbox"]'));
    const dropzone = card.querySelector('.file-dropzone');
    const pathInput = card.querySelector('.image-path-input');
    const doneItem = card.querySelector('.done-item');
    
    const items = [...inputs];
    if (dropzone) items.push(dropzone);
    if (pathInput) items.push(pathInput);
    if (doneItem) items.push(doneItem);
    
    return items;
  }

  function isPathInput(el) {
    return el && (el.classList.contains('image-path-input') || el.classList.contains('attach-inline-path') || el.classList.contains('other-input'));
  }

  function isDropzone(el) {
    return el && el.classList.contains('file-dropzone');
  }

  function isOptionInput(el) {
    return el && (el.type === 'radio' || el.type === 'checkbox');
  }

  function isDoneItem(el) {
    return el && el.classList.contains('done-item');
  }

  function setupDropzone(dropzone, fileInput) {
    dropzone.addEventListener("dragover", (e) => {
      e.preventDefault();
      e.stopPropagation();
      dropzone.classList.add("dragover");
    });
    dropzone.addEventListener("dragleave", () => {
      dropzone.classList.remove("dragover");
    });
    dropzone.addEventListener("drop", (e) => {
      e.preventDefault();
      e.stopPropagation();
      dropzone.classList.remove("dragover");
      const files = e.dataTransfer?.files;
      if (files && files.length > 0) {
        const dt = new DataTransfer();
        dt.items.add(files[0]);
        fileInput.files = dt.files;
        fileInput.dispatchEvent(new Event("change"));
      }
    });
  }

  function setupEdgeNavigation(element) {
    element.addEventListener("keydown", (e) => {
      if (e.key === "ArrowRight" && element.selectionStart === element.value.length) {
        e.preventDefault();
        e.stopPropagation();
        nextQuestion();
      }
      if (e.key === "ArrowLeft" && element.selectionStart === 0) {
        e.preventDefault();
        e.stopPropagation();
        prevQuestion();
      }
    });
  }

  function highlightOption(card, optionIndex, isKeyboard = true) {
    const options = getOptionsForCard(card);
    options.forEach((opt, i) => {
      const item = isOptionInput(opt) ? opt.closest('.option-item') : opt;
      item?.classList.toggle('focused', i === optionIndex);
    });
    const current = options[optionIndex];
    if (current) {
      current.focus();
    }
    if (isKeyboard) {
      card.classList.add('keyboard-nav');
    }
  }

  function clearOptionHighlight(card) {
    card.querySelectorAll('.option-item, .done-item, .file-dropzone, .image-path-input').forEach(item => {
      item.classList.remove('focused');
    });
  }

  function ensureElementVisible(el) {
    const rect = el.getBoundingClientRect();
    const margin = 80;
    if (rect.top < margin || rect.bottom > window.innerHeight - margin) {
      el.scrollIntoView({ behavior: 'auto', block: 'nearest' });
    }
  }

  function focusQuestion(index, fromDirection = 'next') {
    if (index < 0 || index >= nav.cards.length) return;
    
    deactivateSubmitArea();
    
    const prevCard = nav.cards[nav.questionIndex];
    if (prevCard) {
      prevCard.classList.remove('active', 'keyboard-nav');
      clearOptionHighlight(prevCard);
    }
    
    nav.questionIndex = index;
    const card = nav.cards[index];
    card.classList.add('active');
    ensureElementVisible(card);
    
    const options = getOptionsForCard(card);
    const dropzone = card.querySelector('.file-dropzone');
    const textarea = card.querySelector('textarea');
    
    if (dropzone) {
      nav.optionIndex = 0;
      highlightOption(card, nav.optionIndex);
    } else if (options.length > 0) {
      nav.optionIndex = fromDirection === 'prev' ? options.length - 1 : 0;
      highlightOption(card, nav.optionIndex);
    } else if (textarea) {
      textarea.focus();
      if (fromDirection === 'prev') {
        textarea.selectionStart = textarea.selectionEnd = textarea.value.length;
      }
    }

  }

  function nextQuestion() {
    if (nav.questionIndex < nav.cards.length - 1) {
      focusQuestion(nav.questionIndex + 1, 'next');
    } else {
      activateSubmitArea();
    }
  }

  function activateSubmitArea() {
    const prevCard = nav.cards[nav.questionIndex];
    if (prevCard) {
      prevCard.classList.remove('active', 'keyboard-nav');
      clearOptionHighlight(prevCard);
    }
    nav.inSubmitArea = true;
    formFooter?.classList.add('active');
    submitBtn.focus();
    if (formFooter) ensureElementVisible(formFooter);
  }

  function deactivateSubmitArea() {
    nav.inSubmitArea = false;
    formFooter?.classList.remove('active');
  }

  function prevQuestion() {
    if (nav.questionIndex > 0) {
      focusQuestion(nav.questionIndex - 1, 'prev');
    }
  }

  function handleQuestionKeydown(event) {
    if (event.key === 'Escape') {
      if (!expiredOverlay.classList.contains('hidden')) {
        if (timers.countdown) clearInterval(timers.countdown);
        cancelInterview("user").finally(() => window.close());
        return;
      }
      showSessionExpired();
      return;
    }
    
    const isMeta = event.metaKey || event.ctrlKey;
    if (event.key === 'Enter' && isMeta) {
      event.preventDefault();
      formEl.requestSubmit();
      return;
    }

    if (maybeStartOtherInput(event)) return;

    if (nav.inSubmitArea) return;
    
    const card = nav.cards[nav.questionIndex];
    if (!card) return;
    
    const options = getOptionsForCard(card);
    const textarea = card.querySelector('textarea');
    const isTextFocused = document.activeElement === textarea;
    
    if (event.key === 'Tab') {
      const inAttachArea = document.activeElement?.closest('.attach-inline');
      if (inAttachArea) return;
      
      event.preventDefault();
      
      if (options.length > 0) {
        if (event.shiftKey) {
          nav.optionIndex = (nav.optionIndex - 1 + options.length) % options.length;
        } else {
          nav.optionIndex = (nav.optionIndex + 1) % options.length;
        }
        highlightOption(card, nav.optionIndex);
      }
      return;
    }
    
    if (event.key === 'ArrowLeft') {
      if (isTextFocused || isPathInput(document.activeElement)) {
        return;
      }
      event.preventDefault();
      prevQuestion();
      return;
    }
    
    if (event.key === 'ArrowRight') {
      if (isTextFocused || isPathInput(document.activeElement)) {
        return;
      }
      event.preventDefault();
      nextQuestion();
      return;
    }
    
    if (options.length > 0) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        nav.optionIndex = (nav.optionIndex + 1) % options.length;
        highlightOption(card, nav.optionIndex);
        return;
      }
      
      if (event.key === 'ArrowUp') {
        event.preventDefault();
        nav.optionIndex = (nav.optionIndex - 1 + options.length) % options.length;
        highlightOption(card, nav.optionIndex);
        return;
      }
      
      if (event.key === 'Enter' || event.key === ' ') {
        if (isPathInput(document.activeElement)) {
          return;
        }
        if (document.activeElement?.closest('.attach-inline')) {
          return;
        }
        event.preventDefault();
        const option = options[nav.optionIndex];
        if (option) {
          if (isDoneItem(option)) {
            if (!option.classList.contains('disabled')) {
              nextQuestion();
            }
          } else if (isDropzone(option)) {
            if (!filePickerOpen) {
              filePickerOpen = true;
              const fileInput = card.querySelector('input[type="file"]');
              if (fileInput) fileInput.click();
            }
          } else if (option.type === 'radio') {
            option.checked = true;
            debounceSave();
            if (option.value === '__other__') {
              const otherInput = card.querySelector('.other-input');
              if (otherInput) otherInput.focus();
            } else {
              nextQuestion();
            }
          } else if (option.type === 'checkbox') {
            option.checked = !option.checked;
            debounceSave();
            const questionId = option.name;
            updateDoneState(questionId);
            if (option.value === '__other__' && option.checked) {
              const otherInput = card.querySelector('.other-input');
              if (otherInput) otherInput.focus();
            }
          }
        }
        return;
      }
    }
    
    if (textarea && !isTextFocused) {
      if (event.key === 'Enter') {
        event.preventDefault();
        textarea.focus();
        return;
      }
    }
    
    if (isTextFocused && event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      nextQuestion();
      return;
    }
    
    if (document.activeElement?.type === 'file') {
      if (event.key === 'Enter' || event.key === ' ') {
        return;
      }
    }
  }

  function initQuestionNavigation() {
    nav.cards = Array.from(containerEl.querySelectorAll('.question-card'));
    
    nav.cards.forEach((card, index) => {
      card.setAttribute('tabindex', '0');
      card.addEventListener('focus', () => {
        if (nav.questionIndex !== index) {
          focusQuestion(index);
        }
      });
      card.addEventListener('click', (e) => {
        card.classList.remove('keyboard-nav');
        if (nav.questionIndex !== index) {
          if (e.target.closest('.option-item')) {
            nav.questionIndex = index;
            const prevCard = nav.cards.find(c => c.classList.contains('active'));
            if (prevCard && prevCard !== card) {
              prevCard.classList.remove('active', 'keyboard-nav');
              clearOptionHighlight(prevCard);
            }
            card.classList.add('active');
          } else {
            focusQuestion(index);
          }
        }
      });
    });
    
    containerEl.querySelectorAll('input[type="radio"], input[type="checkbox"]').forEach(input => {
      input.setAttribute('tabindex', '-1');
    });
    
    document.addEventListener('keydown', handleQuestionKeydown);
    
    if (nav.cards.length > 0) {
      setTimeout(() => focusQuestion(0), 100);
    }
  }

  function createQuestionCard(question, index) {
    const card = document.createElement("section");
    card.className = "question-card";
    card.setAttribute("role", "listitem");
    card.dataset.questionId = question.id;

    const title = document.createElement("h2");
    title.className = "question-title";
    title.id = `q-${question.id}-title`;
    title.innerHTML = `${index + 1}. ${renderLightMarkdown(question.question)}`;
    card.appendChild(title);

    if (question.context) {
      const context = document.createElement("p");
      context.className = "question-context";
      context.innerHTML = renderLightMarkdown(question.context);
      card.appendChild(context);
    }

    if (question.codeBlock) {
      const codeBlockEl = renderCodeBlock(question.codeBlock);
      if (codeBlockEl) {
        codeBlockEl.classList.add("question-code-block");
        card.appendChild(codeBlockEl);
      }
    }

    if (question.type === "single" || question.type === "multi") {
      const list = document.createElement("div");
      list.className = "option-list";
      list.setAttribute("role", question.type === "single" ? "radiogroup" : "group");
      list.setAttribute("aria-labelledby", title.id);

      const recommended = question.recommended;
      const recommendedList = Array.isArray(recommended)
        ? recommended
        : recommended
          ? [recommended]
          : [];

      question.options.forEach((option, optionIndex) => {
        const optionLabel = getOptionLabel(option);
        const optionCode = isRichOption(option) ? option.code : null;

        const label = document.createElement("label");
        label.className = "option-item";
        if (optionCode) {
          label.classList.add("has-code");
        }

        const input = document.createElement("input");
        input.type = question.type === "single" ? "radio" : "checkbox";
        input.name = question.id;
        input.value = optionLabel;
        input.id = `q-${question.id}-${optionIndex}`;

        input.addEventListener("change", () => {
          debounceSave();
          if (question.type === "multi") {
            updateDoneState(question.id);
          }
        });

        const text = document.createElement("span");
        text.textContent = optionLabel;
        
        if (recommendedList.includes(optionLabel)) {
          const star = document.createElement("span");
          star.className = "recommended-star";
          star.textContent = "*";
          text.appendChild(star);
        }

        label.appendChild(input);
        label.appendChild(text);

        if (optionCode) {
          const codeBlockEl = renderCodeBlock(optionCode);
          if (codeBlockEl) {
            label.appendChild(codeBlockEl);
          }
        }

        list.appendChild(label);
      });

      const otherLabel = document.createElement("label");
      otherLabel.className = "option-item option-other";
      const otherCheck = document.createElement("input");
      otherCheck.type = question.type === "single" ? "radio" : "checkbox";
      otherCheck.name = question.id;
      otherCheck.value = "__other__";
      otherCheck.id = `q-${question.id}-other`;
      const otherInput = document.createElement("textarea");
      otherInput.className = "other-input";
      otherInput.placeholder = "Other...";
      otherInput.rows = 1;
      otherInput.dataset.questionId = question.id;
      const autoResizeOther = () => {
        otherInput.style.height = "auto";
        otherInput.style.height = otherInput.scrollHeight + "px";
      };
      otherInput.addEventListener("input", () => {
        autoResizeOther();
        if (otherInput.value && !otherCheck.checked) {
          otherCheck.checked = true;
          if (question.type === "multi") updateDoneState(question.id);
        }
        debounceSave();
      });
      otherInput.addEventListener("focus", () => {
        if (!otherCheck.checked) {
          otherCheck.checked = true;
          if (question.type === "multi") updateDoneState(question.id);
          debounceSave();
        }
      });
      otherCheck.addEventListener("change", () => {
        debounceSave();
        if (question.type === "multi") updateDoneState(question.id);
        if (otherCheck.checked) otherInput.focus();
      });
      setupEdgeNavigation(otherInput);
      otherLabel.appendChild(otherCheck);
      otherLabel.appendChild(otherInput);
      list.appendChild(otherLabel);

      if (question.type === "multi") {
        const doneItem = document.createElement("div");
        doneItem.className = "option-item done-item disabled";
        doneItem.setAttribute("tabindex", "0");
        doneItem.dataset.doneFor = question.id;
        doneItem.innerHTML = '<span class="done-check">✓</span><span>Done</span>';
        doneItem.addEventListener("click", () => {
          if (!doneItem.classList.contains("disabled")) {
            nextQuestion();
          }
        });
        doneItem.addEventListener("keydown", (e) => {
          if ((e.key === "Enter" || e.key === " ") && !doneItem.classList.contains("disabled")) {
            e.preventDefault();
            e.stopPropagation();
            nextQuestion();
          }
        });
        list.appendChild(doneItem);
      }

      card.appendChild(list);
    }

    if (question.type === "text") {
      const textarea = document.createElement("textarea");
      textarea.dataset.questionId = question.id;
      textarea.addEventListener("input", () => {
        debounceSave();
      });
      setupEdgeNavigation(textarea);
      card.appendChild(textarea);
    }

    if (question.type === "image") {
      imagePathState.set(question.id, []);
      
      const wrapper = document.createElement("div");
      wrapper.className = "file-input";

      const input = document.createElement("input");
      input.type = "file";
      input.accept = "image/png,image/jpeg,image/gif,image/webp";
      input.dataset.questionId = question.id;

      input.addEventListener("change", () => {
        setTimeout(() => { filePickerOpen = false; }, 200);
        clearGlobalError();
        handleFileChange(question.id, input, questionImages, {
          onEmpty: () => clearImage(question.id),
        });
      });
      input.addEventListener("cancel", () => {
        setTimeout(() => { filePickerOpen = false; }, 200);
      });
      input.addEventListener("blur", () => {
        setTimeout(() => { filePickerOpen = false; }, 500);
      });

      const dropzone = document.createElement("div");
      dropzone.className = "file-dropzone";
      dropzone.setAttribute("tabindex", "0");
      dropzone.innerHTML = `
        <span class="file-dropzone-icon">+</span>
        <span class="file-dropzone-text">Click to upload</span>
        <span class="file-dropzone-hint">PNG, JPG, GIF, WebP (max 5MB)</span>
      `;
      
      const pathInput = document.createElement("input");
      pathInput.type = "text";
      pathInput.className = "image-path-input";
      pathInput.placeholder = "Or paste image path/URL and press Enter...";
      pathInput.dataset.questionId = question.id;
      pathInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && pathInput.value.trim()) {
          e.preventDefault();
          e.stopPropagation();
          questionImages.addPath(question.id, normalizePath(pathInput.value.trim()));
          pathInput.value = "";
        }
      });
      setupEdgeNavigation(pathInput);
      
      const selectedItems = document.createElement("div");
      selectedItems.className = "image-selected-items";
      selectedItems.dataset.selectedFor = question.id;
      dropzone.addEventListener("click", () => {
        if (!filePickerOpen) {
          filePickerOpen = true;
          input.click();
        }
      });
      dropzone.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          e.stopPropagation();
          if (!filePickerOpen) {
            filePickerOpen = true;
            input.click();
          }
        }
        if (e.key === "ArrowRight") {
          e.preventDefault();
          e.stopPropagation();
          nextQuestion();
        }
        if (e.key === "ArrowLeft") {
          e.preventDefault();
          e.stopPropagation();
          prevQuestion();
        }
      });
      
      setupDropzone(dropzone, input);

      wrapper.appendChild(input);
      wrapper.appendChild(dropzone);
      wrapper.appendChild(pathInput);
      wrapper.appendChild(selectedItems);
      card.appendChild(wrapper);
    }

    if (question.type !== "image") {
      attachPathState.set(question.id, []);
      
      const attachHint = document.createElement("div");
      attachHint.className = "attach-hint";
      
      const attachBtn = document.createElement("button");
      attachBtn.type = "button";
      attachBtn.className = "attach-btn";
      attachBtn.innerHTML = '<span>+</span> attach';
      attachBtn.dataset.questionId = question.id;
      
      const attachInline = document.createElement("div");
      attachInline.className = "attach-inline hidden";
      attachInline.dataset.attachInlineFor = question.id;
      
      const attachFileInput = document.createElement("input");
      attachFileInput.type = "file";
      attachFileInput.accept = "image/png,image/jpeg,image/gif,image/webp";
      attachFileInput.style.cssText = "position:absolute;width:1px;height:1px;opacity:0;pointer-events:none;";
      
      const attachDrop = document.createElement("div");
      attachDrop.className = "attach-inline-drop";
      attachDrop.setAttribute("tabindex", "0");
      attachDrop.textContent = "Drop image or click";
      
      const attachPath = document.createElement("input");
      attachPath.type = "text";
      attachPath.className = "attach-inline-path";
      attachPath.placeholder = "Or paste path/URL and press Enter";
      
      const attachItems = document.createElement("div");
      attachItems.className = "attach-inline-items";
      attachItems.dataset.attachItemsFor = question.id;
      
      attachBtn.addEventListener("click", () => {
        const isHidden = attachInline.classList.contains("hidden");
        attachInline.classList.toggle("hidden", !isHidden);
        if (isHidden) attachDrop.focus();
      });
      
      attachFileInput.addEventListener("change", () => {
        setTimeout(() => { filePickerOpen = false; }, 200);
        handleFileChange(question.id, attachFileInput, attachments);
      });
      
      attachDrop.addEventListener("click", () => {
        if (!filePickerOpen) {
          filePickerOpen = true;
          attachFileInput.click();
        }
      });
      attachDrop.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          if (!filePickerOpen) {
            filePickerOpen = true;
            attachFileInput.click();
          }
        }
        if (e.key === "Tab") {
          e.preventDefault();
          if (e.shiftKey) {
            attachBtn.focus();
          } else {
            attachPath.focus();
          }
        }
        if (e.key === "Escape") {
          attachBtn.click();
          attachBtn.focus();
        }
      });
      setupDropzone(attachDrop, attachFileInput);
      
      attachPath.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && attachPath.value.trim()) {
          e.preventDefault();
          attachments.addPath(question.id, normalizePath(attachPath.value.trim()));
          attachPath.value = "";
        }
        if (e.key === "Tab") {
          e.preventDefault();
          if (e.shiftKey) {
            attachDrop.focus();
          } else {
            attachBtn.click();
            attachBtn.focus();
          }
        }
        if (e.key === "Escape") {
          attachBtn.click();
          attachBtn.focus();
        }
      });
      setupEdgeNavigation(attachPath);
      
      attachInline.appendChild(attachFileInput);
      attachInline.appendChild(attachDrop);
      attachInline.appendChild(attachPath);
      attachInline.appendChild(attachItems);
      
      attachHint.appendChild(attachBtn);
      card.appendChild(attachHint);
      card.appendChild(attachInline);
    }

    const error = document.createElement("div");
    error.className = "field-error";
    error.dataset.errorFor = question.id;
    error.setAttribute("aria-live", "polite");
    card.appendChild(error);

    card.addEventListener("dragover", (e) => {
      e.preventDefault();
      card.classList.add("dragover");
    });
    card.addEventListener("dragleave", (e) => {
      if (!card.contains(e.relatedTarget)) {
        card.classList.remove("dragover");
      }
    });
    card.addEventListener("drop", (e) => {
      e.preventDefault();
      card.classList.remove("dragover");
      const files = e.dataTransfer?.files;
      if (files && files.length > 0) {
        const file = files[0];
        if (!file.type.startsWith("image/")) return;
        if (question.type === "image") {
          const input = card.querySelector('input[type="file"]');
          if (input) {
            const dt = new DataTransfer();
            dt.items.add(file);
            input.files = dt.files;
            input.dispatchEvent(new Event("change"));
          }
        } else {
          void addPastedImage(question, file);
        }
      }
    });

    return card;
  }

  function loadImage(file) {
    return new Promise((resolve, reject) => {
      const img = new Image();
      const url = URL.createObjectURL(file);
      img.onload = () => resolve(img);
      img.onerror = () => {
        URL.revokeObjectURL(url);
        reject(new Error("Failed to load image"));
      };
      img.src = url;
    });
  }

  async function validateImage(file) {
    if (!ALLOWED_TYPES.includes(file.type)) {
      return { valid: false, error: "Invalid file type. Use PNG, JPG, GIF, or WebP." };
    }
    if (file.size > MAX_SIZE) {
      return { valid: false, error: "Image exceeds 5MB limit." };
    }

    const img = await loadImage(file);
    if (img.src) URL.revokeObjectURL(img.src);
    if (img.width > MAX_DIMENSION || img.height > MAX_DIMENSION) {
      return { valid: false, error: `Image exceeds ${MAX_DIMENSION}x${MAX_DIMENSION} limit.` };
    }
    return { valid: true };
  }

  function clearImage(id) {
    const input = document.querySelector(
      `input[type="file"][data-question-id="${escapeSelector(id)}"]`
    );
    if (input) input.value = "";
    questionImages.removeFile(id);
    setFieldError(id, "");
  }

  async function handleFileChange(questionId, input, manager, options = {}) {
    const { onEmpty } = options;
    setFieldError(questionId, "");

    const file = input.files && input.files[0];
    if (!file) {
      if (onEmpty) onEmpty();
      else manager.removeFile(questionId);
      return;
    }

    if (countUploadedFiles(questionId) + 1 > MAX_IMAGES) {
      setFieldError(questionId, `Only ${MAX_IMAGES} images allowed.`);
      input.value = "";
      return;
    }

    try {
      const validation = await validateImage(file);
      if (!validation.valid) {
        setFieldError(questionId, validation.error);
        input.value = "";
        return;
      }
    } catch (_err) {
      setFieldError(questionId, "Failed to validate image.");
      input.value = "";
      return;
    }

    manager.addFile(questionId, file);
  }

  function resolveQuestionContext(target) {
    const element = target && target.closest ? target : null;
    let card = element ? element.closest(".question-card") : null;
    
    if (!card) {
      card = document.querySelector(".question-card.active");
    }
    
    if (card?.dataset?.questionId) {
      const question = questions.find((q) => q.id === card.dataset.questionId);
      if (question) {
        return { question, card };
      }
    }

    const question = questions[nav.questionIndex];
    const fallbackCard = nav.cards[nav.questionIndex];
    if (!question || !fallbackCard) return null;
    return { question, card: fallbackCard };
  }

  function revealAttachmentArea(questionId) {
    const attachInline = document.querySelector(
      `[data-attach-inline-for="${escapeSelector(questionId)}"]`
    );
    if (attachInline?.classList.contains("hidden")) {
      attachInline.classList.remove("hidden");
    }
  }

  async function addPastedImage(question, file) {
    if (countUploadedFiles(question.id) + 1 > MAX_IMAGES) {
      setFieldError(question.id, `Only ${MAX_IMAGES} images allowed.`);
      return;
    }

    try {
      const validation = await validateImage(file);
      if (!validation.valid) {
        setFieldError(question.id, validation.error);
        return;
      }
    } catch (_err) {
      setFieldError(question.id, "Failed to validate image.");
      return;
    }

    setFieldError(question.id, "");
    if (question.type === "image") {
      questionImages.addFile(question.id, file);
    } else {
      revealAttachmentArea(question.id);
      attachments.addFile(question.id, file);
    }
  }

  function handlePaste(event) {
    if (nav.inSubmitArea || session.expired) return;
    const clipboard = event.clipboardData;
    if (!clipboard) return;
    
    const context = resolveQuestionContext(event.target);
    if (!context) return;

    const items = Array.from(clipboard.items || []);
    const imageItem = items.find((item) => item.type && item.type.startsWith("image/"));
    
    if (imageItem) {
      const file = imageItem.getAsFile();
      if (!file) return;
      event.preventDefault();
      void addPastedImage(context.question, file);
      return;
    }

    const text = clipboard.getData("text/plain")?.trim();
    const isPathLike = text && (text.startsWith("/") || text.startsWith("~") || text.match(/^[a-zA-Z]:\\/));
    const hasImageExtension = text && /\.(png|jpe?g|gif|webp)$/i.test(text);
    
    if (isPathLike && hasImageExtension) {
      event.preventDefault();
      const normalizedPath = normalizePath(text);
      if (context.question.type === "image") {
        questionImages.addPath(context.question.id, normalizedPath);
      } else {
        revealAttachmentArea(context.question.id);
        attachments.addPath(context.question.id, normalizedPath);
      }
    }
  }

  function countUploadedFiles(excludingId) {
    let count = 0;
    imageState.forEach((_value, key) => {
      if (key !== excludingId) count += 1;
    });
    attachState.forEach((_value, key) => {
      if (key !== excludingId) count += 1;
    });
    return count;
  }

  function getOtherValue(questionId) {
    const otherInput = formEl.querySelector(`.other-input[data-question-id="${escapeSelector(questionId)}"]`);
    return otherInput ? otherInput.value : "";
  }

  function getQuestionValue(question) {
    const id = question.id;
    if (question.type === "single") {
      const selected = formEl.querySelector(`input[name="${escapeSelector(id)}"]:checked`);
      if (!selected) return "";
      if (selected.value === "__other__") return getOtherValue(id);
      return selected.value;
    }
    if (question.type === "multi") {
      return Array.from(
        formEl.querySelectorAll(`input[name="${escapeSelector(id)}"]:checked`)
      ).map((input) => input.value === "__other__" ? getOtherValue(id) : input.value).filter(v => v);
    }
    if (question.type === "text") {
      const textarea = formEl.querySelector(`textarea[data-question-id="${escapeSelector(id)}"]`);
      return textarea ? textarea.value : "";
    }
    if (question.type === "image") {
      return questionImages.getPaths(id);
    }
    return "";
  }

  function collectResponses() {
    return questions.map((question) => {
      const resp = { id: question.id, value: getQuestionValue(question) };
      if (question.type !== "image") {
        const attachPaths = attachments.getPaths(question.id);
        if (attachPaths.length > 0) resp.attachments = attachPaths;
      }
      return resp;
    });
  }

  function collectPersistedData() {
    const data = {};
    questions.forEach((question) => {
      if (question.type !== "image") {
        data[question.id] = getQuestionValue(question);
      }
    });
    return data;
  }

  function populateForm(saved) {
    if (!saved) return;
    questions.forEach((question) => {
      const value = saved[question.id];
      if (question.type === "single" && typeof value === "string") {
        const radios = formEl.querySelectorAll(
          `input[name="${escapeSelector(question.id)}"]`
        );
        radios.forEach((radio) => {
          radio.checked = false;
        });
        if (value !== "") {
          const input = formEl.querySelector(
            `input[name="${escapeSelector(question.id)}"][value="${escapeSelector(value)}"]`
          );
          if (input) {
            input.checked = true;
          } else {
            const otherCheck = formEl.querySelector(
              `input[name="${escapeSelector(question.id)}"][value="__other__"]`
            );
            const otherInput = formEl.querySelector(
              `.other-input[data-question-id="${escapeSelector(question.id)}"]`
            );
            if (otherCheck && otherInput) {
              otherCheck.checked = true;
              otherInput.value = value;
              otherInput.dispatchEvent(new Event("input", { bubbles: true }));
            }
          }
        }
      }
      if (question.type === "multi" && Array.isArray(value)) {
        const checkboxes = formEl.querySelectorAll(
          `input[name="${escapeSelector(question.id)}"]`
        );
        checkboxes.forEach((checkbox) => {
          checkbox.checked = false;
        });
        let otherValue = "";
        value.forEach((val) => {
          const input = formEl.querySelector(
            `input[name="${escapeSelector(question.id)}"][value="${escapeSelector(val)}"]`
          );
          if (input) {
            input.checked = true;
          } else if (val) {
            otherValue = val;
          }
        });
        if (otherValue) {
          const otherCheck = formEl.querySelector(
            `input[name="${escapeSelector(question.id)}"][value="__other__"]`
          );
          const otherInput = formEl.querySelector(
            `.other-input[data-question-id="${escapeSelector(question.id)}"]`
          );
          if (otherCheck && otherInput) {
            otherCheck.checked = true;
            otherInput.value = otherValue;
            otherInput.dispatchEvent(new Event("input", { bubbles: true }));
          }
        }
      }
      if (question.type === "text" && typeof value === "string") {
        const textarea = formEl.querySelector(
          `textarea[data-question-id="${escapeSelector(question.id)}"]`
        );
        if (textarea) textarea.value = value;
      }
    });
  }

  function saveProgress() {
    if (!session.storageKey) return;
    const data = collectPersistedData();
    try {
      localStorage.setItem(session.storageKey, JSON.stringify(data));
    } catch (_err) {
      // ignore storage errors
    }
  }

  function loadProgress() {
    if (!session.storageKey) return;
    try {
      const saved = localStorage.getItem(session.storageKey);
      if (saved) {
        populateForm(JSON.parse(saved));
        questions.forEach((q) => {
          if (q.type === "multi") {
            updateDoneState(q.id);
          }
        });
      }
    } catch (_err) {
      // ignore storage errors
    }
  }

  function clearProgress() {
    if (!session.storageKey) return;
    try {
      localStorage.removeItem(session.storageKey);
    } catch (_err) {
      // ignore storage errors
    }
  }

  async function hashQuestions() {
    const json = JSON.stringify(questions);
    const encoder = new TextEncoder();
    const data = encoder.encode(json);
    const hashBuffer = await crypto.subtle.digest("SHA-256", data);
    const hashArray = Array.from(new Uint8Array(hashBuffer));
    const hashHex = hashArray.map((b) => b.toString(16).padStart(2, "0")).join("");
    return hashHex.slice(0, 8);
  }

  async function initStorage() {
    try {
      const hash = await hashQuestions();
      session.storageKey = `pi-interview-${hash}`;
      loadProgress();
    } catch (_err) {
      session.storageKey = null;
    }
  }

  // Set storage key without loading (for revival from saved interview)
  async function initStorageKeyOnly() {
    try {
      const hash = await hashQuestions();
      session.storageKey = `pi-interview-${hash}`;
    } catch (_err) {
      session.storageKey = null;
    }
  }

  // Pre-populate form from saved interview answers
  function populateFromSavedAnswers(savedAnswers) {
    // Convert ResponseItem[] to Record for existing populateForm()
    const valueMap = {};
    savedAnswers.forEach((ans) => {
      const question = questions.find((q) => q.id === ans.id);
      if (question?.type !== "image") {
        valueMap[ans.id] = ans.value;
      }
    });
    populateForm(valueMap);

    // Restore attachments to attachPathState
    savedAnswers.forEach((ans) => {
      if (ans.attachments && ans.attachments.length > 0) {
        attachPathState.set(ans.id, [...ans.attachments]);
        attachments.render(ans.id);
        const panel = document.querySelector(
          `[data-attach-inline-for="${escapeSelector(ans.id)}"]`
        );
        if (panel) panel.classList.remove("hidden");
        const btn = document.querySelector(
          `.attach-btn[data-question-id="${escapeSelector(ans.id)}"]`
        );
        if (btn) btn.classList.add("has-attachment");
      }
    });

    // Restore image paths for image-type questions
    savedAnswers.forEach((ans) => {
      const question = questions.find((q) => q.id === ans.id);
      if (question?.type === "image" && ans.value) {
        const paths = Array.isArray(ans.value) ? ans.value : [ans.value];
        const validPaths = paths.filter((p) => typeof p === "string" && p);
        if (validPaths.length > 0) {
          imagePathState.set(ans.id, validPaths);
          questionImages.render(ans.id);
        }
      }
    });

    // Update done states for multi-select
    questions.forEach((q) => {
      if (q.type === "multi") {
        updateDoneState(q.id);
      }
    });
  }

  function readFileBase64(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        if (typeof reader.result !== "string") {
          reject(new Error("Failed to read file"));
          return;
        }
        const parts = reader.result.split(",");
        resolve(parts[1] || "");
      };
      reader.onerror = () => reject(new Error("Failed to read file"));
      reader.readAsDataURL(file);
    });
  }

  async function buildPayload() {
    const responses = collectResponses();
    const images = [];

    for (const question of questions) {
      const imageEntry = questionImages.getFile(question.id);
      if (imageEntry) {
        const file = imageEntry.file;
        const data = await readFileBase64(file);
        images.push({
          id: question.id,
          filename: file.name,
          mimeType: file.type,
          data,
        });
      }

      if (question.type !== "image") {
        const attachEntry = attachments.getFile(question.id);
        if (attachEntry) {
          const file = attachEntry.file;
          const data = await readFileBase64(file);
          images.push({
            id: question.id,
            filename: file.name,
            mimeType: file.type,
            data,
            isAttachment: true,
          });
        }
      }
    }

    return { responses, images };
  }

  // Save interview snapshot
  async function saveInterview(options = {}) {
    const { submitted = false } = options;

    try {
      const payload = await buildPayload();
      const response = await fetch("/save", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          token: sessionToken,
          responses: payload.responses,
          images: payload.images,
          submitted,
        }),
      });
      const result = await response.json();
      if (result.ok) {
        showSaveSuccess(result.relativePath);
        return true;
      } else {
        if (!submitted) showSaveError(result.error);
        return false;
      }
    } catch (err) {
      if (!submitted) {
        showSaveError("Failed to save interview");
      }
      return false;
    }
  }

  function showSaveSuccess(savePath) {
    const toast = document.getElementById("save-toast");
    if (!toast) return;
    toast.textContent = `Saved to ${savePath}`;
    toast.className = "save-toast success";
    setTimeout(() => toast.classList.add("hidden"), 3000);
  }

  function showSaveError(message) {
    const toast = document.getElementById("save-toast");
    if (!toast) return;
    toast.textContent = message || "Save failed";
    toast.className = "save-toast error";
    setTimeout(() => toast.classList.add("hidden"), 3000);
  }

  async function submitForm(event) {
    event.preventDefault();
    clearGlobalError();
    clearFieldErrors();

    submitBtn.disabled = true;

    try {
      const payload = await buildPayload();
      const response = await fetch("/submit", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: sessionToken, ...payload }),
      });

      const result = await response.json().catch(() => ({ ok: false, error: "Invalid server response" }));

      if (!response.ok || !result.ok) {
        if (result.field) {
          setFieldError(result.field, result.error || "Invalid input");
        } else {
          showGlobalError(result.error || "Submission failed.");
        }
        submitBtn.disabled = false;
        return;
      }

      // Auto-save on successful submit (fire-and-forget)
      // Note: data is window.__INTERVIEW_DATA__, result is server response
      if (data.autoSaveOnSubmit !== false) {
        saveInterview({ submitted: true });
      }

      clearProgress();
      stopHeartbeat();
      stopQueuePolling();
      session.ended = true;
      successOverlay.classList.remove("hidden");
      setTimeout(() => {
        window.close();
      }, 800);
    } catch (err) {
      if (isNetworkError(err)) {
        showSessionExpired();
      } else {
        showGlobalError("Failed to submit responses.");
        submitBtn.disabled = false;
      }
    }
  }

  function init() {
    initTheme();
    clearReloadIntent();

    const isMac = navigator.platform.toUpperCase().indexOf("MAC") >= 0;
    const modKey = document.querySelector(".mod-key");
    if (modKey) {
      modKey.textContent = isMac ? "⌘" : "Ctrl";
    }
    
    setText(titleEl, data.title || "Interview");
    setText(descriptionEl, data.description || "");

    const sessionProjectEl = document.getElementById("session-project");
    const sessionIdEl = document.getElementById("session-id");
    if (sessionProjectEl && cwd) {
      const pathDisplay = cwd.length > 40 ? "..." + cwd.slice(-37) : cwd;
      const branchSuffix = gitBranch ? ` (${gitBranch})` : "";
      sessionProjectEl.textContent = pathDisplay + branchSuffix;
    }
    if (sessionIdEl && sessionId) {
      sessionIdEl.textContent = sessionId.slice(0, 8);
    }
    const projectName = cwd.split("/").filter(Boolean).pop() || "interview";
    const shortId = sessionId.slice(0, 8);
    document.title = `${projectName}${gitBranch ? ` (${gitBranch})` : ""} | ${shortId}`;

    questions.forEach((question, index) => {
      containerEl.appendChild(createQuestionCard(question, index));
    });

    // Pre-populate: savedAnswers takes precedence over localStorage
    if (data.savedAnswers && Array.isArray(data.savedAnswers)) {
      populateFromSavedAnswers(data.savedAnswers);
      initStorageKeyOnly();
    } else {
      initStorage();
    }

    startHeartbeat();
    startQueuePolling();

    formEl.addEventListener("submit", submitForm);

    // Wire up save buttons
    const saveBtnHeader = document.getElementById("save-btn-header");
    const saveBtnFooter = document.getElementById("save-btn-footer");
    if (saveBtnHeader) {
      saveBtnHeader.addEventListener("click", () => saveInterview());
    }
    if (saveBtnFooter) {
      saveBtnFooter.addEventListener("click", () => saveInterview());
    }
    if (queueToastClose) {
      queueToastClose.addEventListener("click", () => {
        queueState.dismissed = true;
        queueToast?.classList.add("hidden");
      });
    }

    if (queueSessionSelect && queueOpenBtn) {
      queueSessionSelect.addEventListener("change", () => {
        const selectedOption = queueSessionSelect.options[queueSessionSelect.selectedIndex];
        queueOpenBtn.disabled = !queueSessionSelect.value || selectedOption?.disabled;
      });
      queueOpenBtn.addEventListener("click", () => {
        const url = queueSessionSelect.value;
        if (!url) return;
        const selectedOption = queueSessionSelect.options[queueSessionSelect.selectedIndex];
        if (selectedOption?.disabled) return;
        window.open(url, "_blank", "noopener");
      });
    }
    window.addEventListener("pagehide", (event) => {
      if (session.ended) return;
      if (event.persisted) return;
      if (hasReloadIntent()) return;
      sendCancelBeacon("user");
    });

    window.addEventListener(
      "keydown",
      (event) => {
        const key = event.key.toLowerCase();
        if ((event.metaKey || event.ctrlKey) && key === "r") {
          markReloadIntent();
        } else if (event.key === "F5") {
          markReloadIntent();
        }
      },
      true
    );
    submitBtn.addEventListener("keydown", (e) => {
      if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        e.preventDefault();
        e.stopImmediatePropagation();
        focusQuestion(nav.cards.length - 1, 'prev');
      }
    });
    
    closeTabBtn.addEventListener("click", async () => {
      if (timers.countdown) clearInterval(timers.countdown);
      await cancelInterview("user");
      window.close();
    });

    stayBtn.addEventListener("click", () => {
      if (timers.countdown) clearInterval(timers.countdown);
      expiredOverlay.classList.remove("visible");
      expiredOverlay.classList.add("hidden");
      
      session.expired = false;
      submitBtn.disabled = false;
      
      if (timeout > 0) {
        startCountdownDisplay();
        timers.expiration = setTimeout(() => {
          showSessionExpired();
        }, timeout * 1000);
      }
    });

    document.addEventListener("keydown", (e) => {
      if (expiredOverlay.classList.contains("visible")) {
        if (e.key === "Tab") {
          e.preventDefault();
          e.stopPropagation();
          if (document.activeElement === stayBtn) {
            closeTabBtn.focus();
          } else {
            stayBtn.focus();
          }
        }
      }
    }, true);
    document.addEventListener("paste", handlePaste);

    if (timeout > 0) {
      startCountdownDisplay();
      timers.expiration = setTimeout(() => {
        showSessionExpired();
      }, timeout * 1000);
      
      ["click", "keydown", "input", "change"].forEach(event => {
        formEl.addEventListener(event, refreshCountdown, { passive: true });
      });
      document.addEventListener("mousemove", refreshCountdown, { passive: true });
    }

    initQuestionNavigation();
  }

  window.__INTERVIEW_API__ = {
    questions,
    nav,
    formEl,
    sessionToken,
    data,
    focusQuestion,
    getQuestionValue,
    debounceSave,
    escapeSelector,
    populateForm,
    updateDoneState,
    setFieldError,
    clearFieldErrors,
  };

  init();
})();
