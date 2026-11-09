const WS_URL = "ws://127.0.0.1:17384";
const BACKOFF_SCHEDULE_MS = [100, 250, 500, 1000];
const KEEPALIVE_INTERVAL_MS = 20_000;

/**
 * Compute capped exponential backoff in milliseconds.
 * @param {number} attempt
 * @returns {number}
 */
export function nextBackoffMs(attempt) {
  const idx = Math.min(Math.max(0, attempt), BACKOFF_SCHEDULE_MS.length - 1);
  return BACKOFF_SCHEDULE_MS[idx];
}

/**
 * Convert a Chrome `tabs.Tab` object into the `WireTab` schema expected by the Rust daemon.
 * @param {chrome.tabs.Tab} tab
 * @param {string | null} [previewUrl]
 */
export function serializeChromeTab(tab, previewUrl = null) {
  const favIconUrl =
    typeof tab.favIconUrl === "string" && tab.favIconUrl.trim().length > 0
      ? tab.favIconUrl
      : null;

  return {
    tab_id: tab.id ?? -1,
    window_id: tab.windowId ?? -1,
    index: tab.index ?? 0,
    title: tab.title ?? "",
    url: tab.url || tab.pendingUrl || "",
    fav_icon_url: favIconUrl,
    preview_url: previewUrl ?? null,
    active: Boolean(tab.active),
    pinned: Boolean(tab.pinned),
    audible: Boolean(tab.audible),
    muted: Boolean(tab.mutedInfo && tab.mutedInfo.muted),
    last_accessed:
      typeof tab.lastAccessed === "number" ? tab.lastAccessed : null,
  };
}

/**
 * Load or initialize a persistent `profile_id` and `profile_name` in `chrome.storage.local`.
 * @param {typeof chrome} chromeApi
 */
async function ensureProfileIdentity(chromeApi) {
  const stored = await chromeApi.storage.local.get([
    "profile_id",
    "profile_name",
  ]);
  let profileId = stored.profile_id;
  let profileName = stored.profile_name;

  if (!profileId) {
    profileId =
      typeof crypto !== "undefined" && crypto.randomUUID
        ? crypto.randomUUID()
        : `profile-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }
  if (!profileName) {
    profileName = `Profile ${profileId.slice(0, 4)}`;
  }

  if (!stored.profile_id || !stored.profile_name) {
    await chromeApi.storage.local.set({
      profile_id: profileId,
      profile_name: profileName,
    });
  }

  return { profileId, profileName };
}

/**
 * Create the MV3 WebSocket bridge controller.
 */
export function createBridgeController({
  chromeApi = globalThis.chrome,
  WebSocketImpl = globalThis.WebSocket,
  wsUrl = WS_URL,
  nowFn = () => Date.now(),
  enableKeepaliveTimer = true,
  captureSettleMs = typeof process !== "undefined" ? 0 : 120,
} = {}) {
  let ws = null;
  let profileId = null;
  let profileName = null;
  let reconnectAttempt = 0;
  let reconnectTimer = null;
  let keepaliveTimer = null;
  let listenersRegistered = false;
  let captureTimer = null;
  let captureRetryTimer = null;
  let pendingCapture = null;
  let lastCaptureTimeMs = 0;
  const MIN_CAPTURE_INTERVAL_MS = 520;
  const tabPreviews = new Map();

  function sendJson(payload) {
    if (ws && ws.readyState === WebSocketImpl.OPEN) {
      try {
        ws.send(JSON.stringify(payload));
      } catch (_err) {
        // Socket will close and trigger reconnect.
      }
    }
  }

  async function downscaleDataUrlIfSupported(dataUrl) {
    if (
      typeof OffscreenCanvas === "undefined" ||
      typeof createImageBitmap === "undefined" ||
      typeof fetch === "undefined"
    ) {
      return dataUrl;
    }
    try {
      const res = await fetch(dataUrl);
      const blob = await res.blob();
      const bitmap = await createImageBitmap(blob);
      const targetWidth = Math.min(560, bitmap.width);
      const targetHeight = Math.max(
        1,
        Math.round((bitmap.height * targetWidth) / bitmap.width)
      );
      const canvas = new OffscreenCanvas(targetWidth, targetHeight);
      const ctx = canvas.getContext("2d");
      ctx.drawImage(bitmap, 0, 0, targetWidth, targetHeight);
      bitmap.close();
      const outBlob = await canvas.convertToBlob({
        type: "image/jpeg",
        quality: 0.76,
      });
      const buf = await outBlob.arrayBuffer();
      const bytes = new Uint8Array(buf);
      let binary = "";
      for (let i = 0; i < bytes.length; i++) {
        binary += String.fromCharCode(bytes[i]);
      }
      return `data:image/jpeg;base64,${btoa(binary)}`;
    } catch (_err) {
      return dataUrl;
    }
  }

  async function captureActiveTabPreview(windowId, tabId) {
    if (
      !profileId ||
      typeof windowId !== "number" ||
      windowId < 0 ||
      typeof tabId !== "number" ||
      typeof chromeApi.tabs.captureVisibleTab !== "function"
    ) {
      return;
    }

    try {
      const rawDataUrl = await chromeApi.tabs.captureVisibleTab(windowId, {
        format: "jpeg",
        quality: 70,
      });
      lastCaptureTimeMs = nowFn();
      if (
        typeof rawDataUrl === "string" &&
        rawDataUrl.startsWith("data:image/")
      ) {
        const dataUrl =
          typeof OffscreenCanvas !== "undefined" &&
          typeof createImageBitmap !== "undefined"
            ? await downscaleDataUrlIfSupported(rawDataUrl)
            : rawDataUrl;
        tabPreviews.set(tabId, dataUrl);
        sendJson({
          type: "tab_preview_updated",
          profile_id: profileId,
          tab_id: tabId,
          preview_url: dataUrl,
        });
      }
    } catch (_err) {
      // If rate-limited by Chrome's MAX_CAPTURE_VISIBLE_TAB_CALLS_PER_SECOND (2/sec)
      // during rapid Ctrl+Tab cycling, schedule a trailing retry so the final tab gets captured.
      if (captureSettleMs > 0) {
        pendingCapture = { windowId, tabId };
        if (captureRetryTimer === null) {
          captureRetryTimer = setTimeout(() => {
            captureRetryTimer = null;
            if (pendingCapture) {
              const next = pendingCapture;
              pendingCapture = null;
              captureActiveTabPreview(next.windowId, next.tabId);
            }
          }, MIN_CAPTURE_INTERVAL_MS);
        }
      }
    }
  }

  function scheduleTabPreviewCapture(windowId, tabId) {
    if (captureSettleMs <= 0) {
      return captureActiveTabPreview(windowId, tabId);
    }
    pendingCapture = { windowId, tabId };
    if (captureTimer !== null) {
      clearTimeout(captureTimer);
    }
    const elapsed = nowFn() - lastCaptureTimeMs;
    const waitMs = Math.max(
      captureSettleMs,
      elapsed < MIN_CAPTURE_INTERVAL_MS ? MIN_CAPTURE_INTERVAL_MS - elapsed : 0
    );
    captureTimer = setTimeout(() => {
      captureTimer = null;
      if (pendingCapture) {
        const next = pendingCapture;
        pendingCapture = null;
        captureActiveTabPreview(next.windowId, next.tabId);
      }
    }, waitMs);
  }

  async function sendSnapshot() {
    if (!profileId) return;
    const allTabs = await chromeApi.tabs.query({});
    const validTabs = allTabs.filter((t) => typeof t.id === "number");
    const wireTabs = validTabs.map((t) =>
      serializeChromeTab(t, tabPreviews.get(t.id) ?? null)
    );

    sendJson({
      type: "snapshot",
      profile_id: profileId,
      profile_name: profileName,
      tabs: wireTabs,
    });

    for (const tab of validTabs) {
      if (tab.active && typeof tab.windowId === "number") {
        await captureActiveTabPreview(tab.windowId, tab.id);
      }
    }
  }

  async function handleIncomingMessage(rawData) {
    let msg;
    try {
      msg = JSON.parse(rawData);
    } catch (_err) {
      return;
    }

    if (!msg || typeof msg.type !== "string") return;

    if (msg.type === "activate_tab") {
      const { window_id, tab_id } = msg;
      try {
        await Promise.all([
          chromeApi.tabs.update(tab_id, { active: true }),
          chromeApi.windows.update(window_id, {
            focused: true,
            drawAttention: false,
          }),
        ]);
      } catch (_err) {
        // Tab or window may have been closed concurrently.
      }
      return;
    }

    // Daemon found no open tab for the user's query: open it in a new tab instead.
    if (msg.type === "open_url") {
      if (typeof msg.url !== "string" || msg.url.length === 0) return;
      try {
        await chromeApi.tabs.create({ url: msg.url, active: true });
      } catch (_err) {
        // Invalid URL or no window available.
      }
      return;
    }

    if (msg.type === "search_web") {
      if (typeof msg.text !== "string" || msg.text.trim().length === 0) return;
      try {
        if (chromeApi.search && typeof chromeApi.search.query === "function") {
          // Uses the user's default search engine (requires the "search" permission).
          await chromeApi.search.query({ text: msg.text, disposition: "NEW_TAB" });
        } else {
          await chromeApi.tabs.create({
            url: `https://www.google.com/search?q=${encodeURIComponent(msg.text)}`,
            active: true,
          });
        }
      } catch (_err) {
        // Search API unavailable or no window available.
      }
    }
  }

  function clearKeepalive() {
    if (keepaliveTimer !== null) {
      clearInterval(keepaliveTimer);
      keepaliveTimer = null;
    }
  }

  function scheduleReconnect() {
    clearKeepalive();
    if (reconnectTimer !== null) return;
    const delay = nextBackoffMs(reconnectAttempt);
    reconnectAttempt += 1;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  }

  function connect() {
    if (
      ws &&
      (ws.readyState === WebSocketImpl.OPEN ||
        ws.readyState === WebSocketImpl.CONNECTING)
    ) {
      return;
    }

    try {
      ws = new WebSocketImpl(wsUrl);
    } catch (_err) {
      scheduleReconnect();
      return;
    }

    ws.onopen = async () => {
      reconnectAttempt = 0;
      clearKeepalive();
      if (enableKeepaliveTimer) {
        keepaliveTimer = setInterval(() => {
          sendJson({ type: "ping" });
        }, KEEPALIVE_INTERVAL_MS);
      }
      await sendSnapshot();
    };

    ws.onmessage = async (event) => {
      await handleIncomingMessage(event.data);
    };

    ws.onerror = () => {
      // Let onclose handle reconnection backoff.
    };

    ws.onclose = () => {
      ws = null;
      scheduleReconnect();
    };
  }

  function registerChromeListeners() {
    if (listenersRegistered) return;
    listenersRegistered = true;

    chromeApi.tabs.onCreated.addListener((tab) => {
      if (typeof tab.id !== "number") return;
      sendJson({
        type: "tab_upserted",
        profile_id: profileId,
        tab: serializeChromeTab(tab, tabPreviews.get(tab.id) ?? null),
      });
    });

    chromeApi.tabs.onUpdated.addListener((_tabId, changeInfo, tab) => {
      if (!tab || typeof tab.id !== "number") return;
      sendJson({
        type: "tab_upserted",
        profile_id: profileId,
        tab: serializeChromeTab(tab, tabPreviews.get(tab.id) ?? null),
      });
      if (
        tab.active &&
        typeof tab.windowId === "number" &&
        changeInfo &&
        changeInfo.status === "complete"
      ) {
        scheduleTabPreviewCapture(tab.windowId, tab.id);
      }
    });

    chromeApi.tabs.onRemoved.addListener((tabId) => {
      tabPreviews.delete(tabId);
      sendJson({
        type: "tab_removed",
        profile_id: profileId,
        tab_id: tabId,
      });
    });

    chromeApi.tabs.onActivated.addListener(async (activeInfo) => {
      sendJson({
        type: "tab_activated",
        profile_id: profileId,
        window_id: activeInfo.windowId,
        tab_id: activeInfo.tabId,
        timestamp_ms: nowFn(),
      });
      await scheduleTabPreviewCapture(activeInfo.windowId, activeInfo.tabId);
    });

    chromeApi.windows.onFocusChanged.addListener(async (windowId) => {
      if (
        windowId === chromeApi.windows.WINDOW_ID_NONE ||
        typeof windowId !== "number" ||
        windowId < 0
      ) {
        return;
      }
      try {
        const tabs = await chromeApi.tabs.query({
          active: true,
          windowId,
        });
        if (tabs && tabs.length > 0 && typeof tabs[0].id === "number") {
          sendJson({
            type: "tab_activated",
            profile_id: profileId,
            window_id: windowId,
            tab_id: tabs[0].id,
            timestamp_ms: nowFn(),
          });
        }
      } catch (_err) {
        // Ignore transient window lookup errors.
      }
    });

    chromeApi.windows.onRemoved.addListener((windowId) => {
      sendJson({
        type: "window_removed",
        profile_id: profileId,
        window_id: windowId,
      });
    });
  }

  async function start() {
    const identity = await ensureProfileIdentity(chromeApi);
    profileId = identity.profileId;
    profileName = identity.profileName;
    registerChromeListeners();
    connect();
  }

  return {
    start,
    connect,
    sendSnapshot,
  };
}

if (
  typeof globalThis.chrome !== "undefined" &&
  globalThis.chrome.tabs &&
  globalThis.chrome.storage
) {
  const controller = createBridgeController();
  controller.start();
}
