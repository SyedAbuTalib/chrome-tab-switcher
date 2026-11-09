import test from "node:test";
import assert from "node:assert/strict";
import {
  serializeChromeTab,
  createBridgeController,
  nextBackoffMs,
} from "../extension/background.js";

test("serializeChromeTab converts Chrome tab object to WireTab schema", () => {
  const rawTab = {
    id: 42,
    windowId: 100,
    index: 3,
    title: "GitHub · Pull Request",
    url: "https://github.com/org/repo/pull/421",
    favIconUrl: "https://github.githubassets.com/favicons/favicon.svg",
    active: true,
    pinned: false,
    audible: true,
    mutedInfo: { muted: false },
    lastAccessed: 1700000000123.5,
  };

  const wire = serializeChromeTab(rawTab);
  assert.deepEqual(wire, {
    tab_id: 42,
    window_id: 100,
    index: 3,
    title: "GitHub · Pull Request",
    url: "https://github.com/org/repo/pull/421",
    fav_icon_url: "https://github.githubassets.com/favicons/favicon.svg",
    preview_url: null,
    active: true,
    pinned: false,
    audible: true,
    muted: false,
    last_accessed: 1700000000123.5,
  });
});

test("nextBackoffMs follows capped schedule 100 -> 250 -> 500 -> 1000", () => {
  assert.equal(nextBackoffMs(0), 100);
  assert.equal(nextBackoffMs(1), 250);
  assert.equal(nextBackoffMs(2), 500);
  assert.equal(nextBackoffMs(3), 1000);
  assert.equal(nextBackoffMs(10), 1000);
});

test("createBridgeController sends snapshot on connect, deltas on events, and handles activate_tab", async () => {
  const sentMessages = [];
  let wsInstance = null;
  const updatedTabs = [];
  const updatedWindows = [];
  const listeners = {
    onCreated: [],
    onUpdated: [],
    onRemoved: [],
    onActivated: [],
    onFocusChanged: [],
    onWindowRemoved: [],
  };

  const mockChrome = {
    storage: {
      local: {
        get: async () => ({
          profile_id: "prof-uuid-1",
          profile_name: "Work",
        }),
        set: async () => {},
      },
    },
    tabs: {
      query: async (filter) => {
        if (filter && filter.windowId === 10 && filter.active) {
          return [{ id: 102, windowId: 10 }];
        }
        return [
          {
            id: 101,
            windowId: 10,
            index: 0,
            title: "Docs",
            url: "https://docs.rs",
            favIconUrl: "",
            active: true,
            pinned: true,
            audible: false,
            mutedInfo: { muted: true },
            lastAccessed: 1700000000000,
          },
        ];
      },
      update: async (tabId, props) => {
        updatedTabs.push({ tabId, props });
      },
      captureVisibleTab: async (_windowId, _opts) =>
        "data:image/jpeg;base64,/9j/4AAQSkZJRg==",
      onCreated: { addListener: (fn) => listeners.onCreated.push(fn) },
      onUpdated: { addListener: (fn) => listeners.onUpdated.push(fn) },
      onRemoved: { addListener: (fn) => listeners.onRemoved.push(fn) },
      onActivated: { addListener: (fn) => listeners.onActivated.push(fn) },
    },
    windows: {
      WINDOW_ID_NONE: -1,
      update: async (windowId, props) => {
        updatedWindows.push({ windowId, props });
      },
      onFocusChanged: { addListener: (fn) => listeners.onFocusChanged.push(fn) },
      onRemoved: { addListener: (fn) => listeners.onWindowRemoved.push(fn) },
    },
  };

  class MockWebSocket {
    static OPEN = 1;
    constructor(url) {
      this.url = url;
      this.readyState = MockWebSocket.OPEN;
      wsInstance = this;
    }
    send(data) {
      sentMessages.push(JSON.parse(data));
    }
    close() {
      this.readyState = 3;
    }
  }

  const controller = createBridgeController({
    chromeApi: mockChrome,
    WebSocketImpl: MockWebSocket,
    nowFn: () => 1700000005000,
    enableKeepaliveTimer: false,
  });

  await controller.start();
  await wsInstance.onopen();

  assert.equal(sentMessages.length, 2);
  assert.equal(sentMessages[0].type, "snapshot");
  assert.equal(sentMessages[0].profile_id, "prof-uuid-1");
  assert.equal(sentMessages[0].profile_name, "Work");
  assert.equal(sentMessages[0].tabs.length, 1);
  assert.equal(sentMessages[0].tabs[0].tab_id, 101);
  assert.equal(sentMessages[0].tabs[0].muted, true);
  assert.equal(sentMessages[0].tabs[0].fav_icon_url, null);
  assert.deepEqual(sentMessages[1], {
    type: "tab_preview_updated",
    profile_id: "prof-uuid-1",
    tab_id: 101,
    preview_url: "data:image/jpeg;base64,/9j/4AAQSkZJRg==",
  });

  // Trigger onCreated
  listeners.onCreated[0]({
    id: 102,
    windowId: 10,
    index: 1,
    title: "New Tab",
    url: "chrome://newtab/",
    active: false,
    pinned: false,
    audible: false,
  });
  assert.equal(sentMessages.at(-1).type, "tab_upserted");
  assert.equal(sentMessages.at(-1).tab.tab_id, 102);

  // Trigger onActivated
  await listeners.onActivated[0]({ tabId: 102, windowId: 10 });
  assert.deepEqual(sentMessages.at(-2), {
    type: "tab_activated",
    profile_id: "prof-uuid-1",
    window_id: 10,
    tab_id: 102,
    timestamp_ms: 1700000005000,
  });
  assert.deepEqual(sentMessages.at(-1), {
    type: "tab_preview_updated",
    profile_id: "prof-uuid-1",
    tab_id: 102,
    preview_url: "data:image/jpeg;base64,/9j/4AAQSkZJRg==",
  });

  // Trigger windows.onFocusChanged
  await listeners.onFocusChanged[0](10);
  assert.deepEqual(sentMessages.at(-1), {
    type: "tab_activated",
    profile_id: "prof-uuid-1",
    window_id: 10,
    tab_id: 102,
    timestamp_ms: 1700000005000,
  });

  // Trigger onRemoved
  listeners.onRemoved[0](102, { windowId: 10, isWindowClosing: false });
  assert.deepEqual(sentMessages.at(-1), {
    type: "tab_removed",
    profile_id: "prof-uuid-1",
    tab_id: 102,
  });

  // Trigger windows.onRemoved
  listeners.onWindowRemoved[0](10);
  assert.deepEqual(sentMessages.at(-1), {
    type: "window_removed",
    profile_id: "prof-uuid-1",
    window_id: 10,
  });

  // Trigger incoming activate_tab from daemon
  await wsInstance.onmessage({
    data: JSON.stringify({
      type: "activate_tab",
      window_id: 10,
      tab_id: 101,
    }),
  });
  assert.deepEqual(updatedTabs, [{ tabId: 101, props: { active: true } }]);
  assert.deepEqual(updatedWindows, [
    { windowId: 10, props: { focused: true, drawAttention: false } },
  ]);
});

test("open_url and search_web open a new tab, preferring chrome.search for the default engine", async () => {
  const createdTabs = [];
  const searchQueries = [];
  let wsInstance = null;
  const noopEvent = { addListener: () => {} };

  const makeChrome = ({ withSearchApi }) => ({
    storage: {
      local: {
        get: async () => ({ profile_id: "prof-uuid-1", profile_name: "Work" }),
        set: async () => {},
      },
    },
    tabs: {
      query: async () => [],
      update: async () => {},
      create: async (props) => {
        createdTabs.push(props);
      },
      onCreated: noopEvent,
      onUpdated: noopEvent,
      onRemoved: noopEvent,
      onActivated: noopEvent,
    },
    windows: {
      WINDOW_ID_NONE: -1,
      update: async () => {},
      onFocusChanged: noopEvent,
      onRemoved: noopEvent,
    },
    ...(withSearchApi
      ? {
          search: {
            query: async (props) => {
              searchQueries.push(props);
            },
          },
        }
      : {}),
  });

  class MockWebSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = MockWebSocket.OPEN;
      wsInstance = this;
    }
    send() {}
    close() {}
  }

  const send = (payload) =>
    wsInstance.onmessage({ data: JSON.stringify(payload) });

  // With chrome.search available: URLs go to tabs.create, searches to search.query
  const withSearch = createBridgeController({
    chromeApi: makeChrome({ withSearchApi: true }),
    WebSocketImpl: MockWebSocket,
    enableKeepaliveTimer: false,
  });
  await withSearch.start();
  await wsInstance.onopen();

  await send({ type: "open_url", url: "https://github.com/org/repo" });
  assert.deepEqual(createdTabs, [
    { url: "https://github.com/org/repo", active: true },
  ]);

  await send({ type: "search_web", text: "rust gpui docs" });
  assert.deepEqual(searchQueries, [
    { text: "rust gpui docs", disposition: "NEW_TAB" },
  ]);
  assert.equal(createdTabs.length, 1);

  // Malformed payloads are ignored
  await send({ type: "open_url", url: "" });
  await send({ type: "search_web", text: "   " });
  await send({ type: "open_url" });
  assert.equal(createdTabs.length, 1);
  assert.equal(searchQueries.length, 1);

  // Without chrome.search (older Chrome / other browsers): fall back to a Google search tab
  createdTabs.length = 0;
  const withoutSearch = createBridgeController({
    chromeApi: makeChrome({ withSearchApi: false }),
    WebSocketImpl: MockWebSocket,
    enableKeepaliveTimer: false,
  });
  await withoutSearch.start();
  await wsInstance.onopen();

  await send({ type: "search_web", text: "zebra quokka" });
  assert.deepEqual(createdTabs, [
    {
      url: "https://www.google.com/search?q=zebra%20quokka",
      active: true,
    },
  ]);
});
