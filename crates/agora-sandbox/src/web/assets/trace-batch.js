((root, factory) => {
  "use strict";

  const traceBatch = factory();
  if (typeof module === "object" && module.exports) module.exports = traceBatch;
  else root.AgoraTraceBatch = traceBatch;
})(typeof globalThis === "undefined" ? this : globalThis, () => {
  "use strict";

  function create({
    keyOf,
    onFlush,
    delayMs = 1000,
    maxEvents = 5000,
    setTimer = setTimeout,
    clearTimer = clearTimeout,
  }) {
    const events = new Map();
    const pending = new Map();
    const evictedKeys = [];
    let pendingTimer = null;

    function cancelPending() {
      if (pendingTimer === null) return;
      clearTimer(pendingTimer);
      pendingTimer = null;
    }

    function enforceBound() {
      let evicted = false;
      while (events.size > maxEvents) {
        const key = events.keys().next().value;
        events.delete(key);
        pending.delete(key);
        evictedKeys.push(key);
        evicted = true;
      }
      return evicted;
    }

    function schedule() {
      if (pendingTimer !== null) return;
      pendingTimer = setTimer(() => {
        pendingTimer = null;
        onFlush();
      }, delayMs);
    }

    function append(event) {
      const key = keyOf(event);
      events.set(key, event);
      pending.set(key, event);
      const evicted = enforceBound();
      schedule();
      return evicted;
    }

    function replace(nextEvents) {
      cancelPending();
      events.clear();
      pending.clear();
      evictedKeys.length = 0;
      for (const event of nextEvents) events.set(keyOf(event), event);
      enforceBound();
      evictedKeys.length = 0;
    }

    function clear() {
      cancelPending();
      events.clear();
      pending.clear();
      evictedKeys.length = 0;
    }

    function flush() {
      cancelPending();
      onFlush();
    }

    function values() {
      return [...events.values()];
    }

    function takeChanges() {
      const changes = {
        appended: [...pending.values()],
        evictedKeys: evictedKeys.splice(0),
      };
      pending.clear();
      return changes;
    }

    return Object.freeze({
      append,
      replace,
      clear,
      flush,
      values,
      takeChanges,
      get size() {
        return events.size;
      },
    });
  }

  return Object.freeze({ create });
});
