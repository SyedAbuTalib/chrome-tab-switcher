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
  // Second capture after a tab becomes visible: heavy SPAs (Calendar, Gmail, Figma) often
  // still show a stale or half-drawn frame at +120ms once Chrome un-throttles them.
  settleRecaptureMs = 900,
  // Chrome allows MAX_CAPTURE_VISIBLE_TAB_CALLS_PER_SECOND = 2.
  minCaptureIntervalMs = 520,
  // Backoff between failed captures. Discarded tabs reload on activation and can take a few
  // seconds to produce a frame; ~7.5s total covers cold reloads of heavy pages like Calendar.
  captureRetryScheduleMs = [520, 1000, 2000, 4000],
} = {}) {
  let ws = null;
  let profileId = null;
  let profileName = null;
  let reconnectAttempt = 0;
  let reconnectTimer = null;
  let keepaliveTimer = null;
  let listenersRegistered = false;

  // ---- Preview capture pipeline -------------------------------------------------------------
  // `captureVisibleTab(windowId)` photographs whatever tab is *currently* visible in a window,
  // so the queue is keyed by window and the active tab is resolved at capture time (never from
  // when the capture was requested — otherwise a quick tab switch attributes the new tab's
  // picture to the old tab). One global timer paces calls to respect Chrome's 2/sec quota.
  const pendingCaptures = new Map(); // windowId -> { dueAt, attempt, settle }
  const uncapturableTabs = new Map(); // tabId -> origin Chrome refuses to capture (policy, chrome://)
  const tabPreviews = new Map(); // tabId -> data URL of the last successful capture
  let captureQueueTimer = null;
  let captureInFlight = false;
  let lastCaptureTimeMs = 0;

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

  /** Errors Chrome will keep returning for this page no matter how often we retry. */
  function isPermanentCaptureError(message) {
    return /ExtensionsSettings policy|permission is not in effect|Cannot access (contents|a chrome)|cannot be scripted|must request permission/i.test(
      message
    );
  }

  function captureBlockKey(url) {
    try {
      return new URL(url).origin;
    } catch (_err) {
      return typeof url === "string" ? url : "";
    }
  }

  /**
   * Ask for a fresh thumbnail of whatever is visible in `windowId`.
   * - A fresh request (attempt 0) supersedes any pending retry/settle for that window and
   *   coalesces with an already-imminent fresh request.
   * - Retries only land if nothing newer is pending.
   * Without timers (tests), captures run immediately and nothing is rescheduled.
   */
  function requestCapture(
    windowId,
    { delayMs = captureSettleMs, attempt = 0, settle = false } = {}
  ) {
    if (typeof windowId !== "number" || windowId < 0) {
      return Promise.resolve();
    }
    if (captureSettleMs <= 0) {
      return attempt === 0 && !settle
        ? captureWindowPreview(windowId, 0, false)
        : Promise.resolve();
    }
    const dueAt = nowFn() + Math.max(0, delayMs);
    const existing = pendingCaptures.get(windowId);
    if (existing) {
      const existingIsFresh = existing.attempt === 0 && !existing.settle;
      if (attempt > 0 || settle) {
        // Something newer already wants this window: let it win.
        armCaptureQueue();
        return Promise.resolve();
      }
      if (existingIsFresh && existing.dueAt <= dueAt) {
        armCaptureQueue();
        return Promise.resolve();
      }
    }
    pendingCaptures.set(windowId, { dueAt, attempt, settle });
    armCaptureQueue();
    return Promise.resolve();
  }

  function armCaptureQueue() {
    if (captureQueueTimer !== null) {
      clearTimeout(captureQueueTimer);
      captureQueueTimer = null;
    }
    if (captureInFlight || pendingCaptures.size === 0) return;
    let nextDue = Infinity;
    for (const slot of pendingCaptures.values()) {
      if (slot.dueAt < nextDue) nextDue = slot.dueAt;
    }
    const earliestAllowed = lastCaptureTimeMs + minCaptureIntervalMs;
    const fireAt = Math.max(nextDue, earliestAllowed);
    captureQueueTimer = setTimeout(
      runCaptureQueue,
      Math.max(0, fireAt - nowFn())
    );
  }

  async function runCaptureQueue() {
    captureQueueTimer = null;
    if (captureInFlight) return;
    const now = nowFn();
    if (now - lastCaptureTimeMs < minCaptureIntervalMs) {
      armCaptureQueue();
      return;
    }
    let nextWindowId = null;
    let nextSlot = null;
    for (const [windowId, slot] of pendingCaptures) {
      if (slot.dueAt <= now && (nextSlot === null || slot.dueAt < nextSlot.dueAt)) {
        nextWindowId = windowId;
        nextSlot = slot;
      }
    }
    if (nextSlot === null) {
      armCaptureQueue();
      return;
    }
    pendingCaptures.delete(nextWindowId);
    captureInFlight = true;
    try {
      await captureWindowPreview(nextWindowId, nextSlot.attempt, nextSlot.settle);
    } finally {
      captureInFlight = false;
      armCaptureQueue();
    }
  }

  async function isWindowFocused(windowId) {
    if (typeof chromeApi.windows.get !== "function") return true;
    try {
      const win = await chromeApi.windows.get(windowId);
      return Boolean(win && win.focused);
    } catch (_err) {
      return false;
    }
  }

  /**
   * Capture the tab currently visible in `windowId` and attribute the image to *that* tab.
   * On success, schedule one "settle" re-capture so heavy SPAs that were still painting at
   * +120ms get a second shot. On failure, classify: permanent errors (enterprise
   * ExtensionSettings policy, chrome:// pages) are remembered per tab+origin and never retried;
   * transient ones (rate limit, window not yet on screen, reloading tab with no frame yet)
   * back off through `captureRetryScheduleMs`.
   */
  async function captureWindowPreview(windowId, attempt = 0, settle = false) {
    if (!profileId || typeof chromeApi.tabs.captureVisibleTab !== "function") {
      return;
    }
    let tab = null;
    try {
      const tabs = await chromeApi.tabs.query({ active: true, windowId });
      tab = tabs && tabs[0];
    } catch (_err) {
      return; // window is gone
    }
    if (!tab || typeof tab.id !== "number") return;
    if (uncapturableTabs.get(tab.id) === captureBlockKey(tab.url)) return;
    // Follow-ups are only worth it while the window is still the one the user is looking at;
    // capturing an occluded window just burns quota and produces "view is invisible" noise.
    if ((attempt > 0 || settle) && !(await isWindowFocused(windowId))) return;

    lastCaptureTimeMs = nowFn();
    try {
      const rawDataUrl = await chromeApi.tabs.captureVisibleTab(windowId, {
        format: "jpeg",
        quality: 70,
      });
      if (
        typeof rawDataUrl === "string" &&
        rawDataUrl.startsWith("data:image/")
      ) {
        const dataUrl =
          typeof OffscreenCanvas !== "undefined" &&
          typeof createImageBitmap !== "undefined"
            ? await downscaleDataUrlIfSupported(rawDataUrl)
            : rawDataUrl;
        tabPreviews.set(tab.id, dataUrl);
        sendJson({
          type: "tab_preview_updated",
          profile_id: profileId,
          tab_id: tab.id,
          preview_url: dataUrl,
        });
      }
      if (!settle && settleRecaptureMs > 0) {
        requestCapture(windowId, { delayMs: settleRecaptureMs, settle: true });
      }
    } catch (err) {
      const message = err && err.message ? err.message : String(err);
      const permanent = isPermanentCaptureError(message);
      const willRetry =
        !permanent &&
        captureSettleMs > 0 &&
        attempt < captureRetryScheduleMs.length;
      if (permanent) {
        uncapturableTabs.set(tab.id, captureBlockKey(tab.url));
      }
      sendJson({
        type: "preview_capture_failed",
        profile_id: profileId,
        tab_id: tab.id,
        window_id: windowId,
        url: typeof tab.url === "string" ? tab.url : "",
        attempt: attempt + 1,
        permanent,
        will_retry: willRetry,
        error: message,
      });
      if (willRetry) {
        requestCapture(windowId, {
          delayMs: captureRetryScheduleMs[attempt],
          attempt: attempt + 1,
        });
      }
    }
  }

  /**
   * Snapshot whichever tab is currently visible in the focused window. Used when the daemon
   * summons the switcher so cards reflect the page as the user *left* it, not as they arrived.
   */
  async function captureFocusedWindowPreview() {
    try {
      const tabs = await chromeApi.tabs.query({
        active: true,
        lastFocusedWindow: true,
      });
      const tab = tabs && tabs[0];
      if (tab && typeof tab.windowId === "number") {
        await requestCapture(tab.windowId, { delayMs: 0 });
      }
    } catch (_err) {
      // No focused window (e.g. Chrome in background with no windows).
    }
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

    // One capture per window, paced by the queue so a multi-window reconnect doesn't blow
    // through Chrome's 2/sec quota the way back-to-back calls did.
    for (const tab of validTabs) {
      if (tab.active && typeof tab.windowId === "number") {
        await requestCapture(tab.windowId, { delayMs: 0 });
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

    // Daemon is about to show the switcher: refresh the thumbnail of the tab the user is
    // leaving so cards show pages as they were left, not as they were arrived at.
    if (msg.type === "capture_preview") {
      await captureFocusedWindowPreview();
      return;
    }

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
        requestCapture(tab.windowId);
      }
    });

    chromeApi.tabs.onRemoved.addListener((tabId) => {
      tabPreviews.delete(tabId);
      uncapturableTabs.delete(tabId);
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
      await requestCapture(activeInfo.windowId);
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
          // Switching *windows* (not tabs) never fires tabs.onActivated, so without this the
          // front tab of every other window keeps a frozen thumbnail forever.
          await requestCapture(windowId);
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
