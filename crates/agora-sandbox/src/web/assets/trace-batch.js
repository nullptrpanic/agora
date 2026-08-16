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
    let pendingTimer = null;

    function cancelPending() {
      if (pendingTimer === null) return;
      clearTimer(pendingTimer);
      pendingTimer = null;
    }

    function enforceBound() {
      let evicted = false;
      while (events.size > maxEvents) {
        events.delete(events.keys().next().value);
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
      events.set(keyOf(event), event);
      const evicted = enforceBound();
      schedule();
      return evicted;
    }

    function replace(nextEvents) {
      cancelPending();
      events.clear();
      for (const event of nextEvents) events.set(keyOf(event), event);
      enforceBound();
    }

    function clear() {
      cancelPending();
      events.clear();
    }

    function flush() {
      cancelPending();
      onFlush();
    }

    function values() {
      return [...events.values()];
    }

    return Object.freeze({
      append,
      replace,
      clear,
      flush,
      values,
      get size() {
        return events.size;
      },
    });
  }

  return Object.freeze({ create });
});
