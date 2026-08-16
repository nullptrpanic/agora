"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");
const { create } = require("../../src/web/assets/trace-batch.js");

function fakeTimers() {
  let nextId = 1;
  const pending = new Map();
  const scheduled = [];
  const cancelled = [];

  return {
    scheduled,
    cancelled,
    setTimer(callback, delay) {
      const id = nextId;
      nextId += 1;
      pending.set(id, callback);
      scheduled.push({ id, delay });
      return id;
    },
    clearTimer(id) {
      cancelled.push(id);
      pending.delete(id);
    },
    run(id) {
      const callback = pending.get(id);
      assert.ok(callback, `timer ${id} is pending`);
      pending.delete(id);
      callback();
    },
    has(id) {
      return pending.has(id);
    },
  };
}

function createBatch(timers, options = {}) {
  let flushes = 0;
  const batch = create({
    keyOf: (event) => event.id,
    onFlush: () => {
      flushes += 1;
    },
    delayMs: 1000,
    maxEvents: 5000,
    setTimer: timers.setTimer,
    clearTimer: timers.clearTimer,
    ...options,
  });
  return { batch, flushes: () => flushes };
}

test("a live burst schedules one render for the one-second boundary", () => {
  const timers = fakeTimers();
  const { batch, flushes } = createBatch(timers);

  batch.append({ id: "one" });
  batch.append({ id: "two" });

  assert.equal(batch.size, 2);
  assert.deepEqual(timers.scheduled, [{ id: 1, delay: 1000 }]);
  assert.equal(flushes(), 0);

  timers.run(1);
  assert.equal(flushes(), 1);

  batch.append({ id: "three" });
  assert.deepEqual(timers.scheduled, [
    { id: 1, delay: 1000 },
    { id: 2, delay: 1000 },
  ]);
});

test("duplicate replacement preserves order and the bound keeps newest events", () => {
  const timers = fakeTimers();
  const { batch } = createBatch(timers, { maxEvents: 3 });

  assert.equal(batch.append({ id: "one", value: 1 }), false);
  assert.equal(batch.append({ id: "two", value: 2 }), false);
  assert.equal(batch.append({ id: "two", value: 20 }), false);
  assert.equal(batch.append({ id: "three", value: 3 }), false);
  assert.equal(batch.append({ id: "four", value: 4 }), true);

  assert.equal(batch.size, 3);
  assert.deepEqual(batch.values(), [
    { id: "two", value: 20 },
    { id: "three", value: 3 },
    { id: "four", value: 4 },
  ]);
  assert.equal(timers.scheduled.length, 1);
});

test("replace clear and immediate flush cancel stale scheduled work", () => {
  const timers = fakeTimers();
  const { batch, flushes } = createBatch(timers, { maxEvents: 2 });

  batch.append({ id: "old" });
  batch.replace([{ id: "new-one" }, { id: "new-two" }, { id: "new-three" }]);

  assert.equal(timers.has(1), false);
  assert.deepEqual(timers.cancelled, [1]);
  assert.deepEqual(batch.values(), [{ id: "new-two" }, { id: "new-three" }]);
  assert.equal(flushes(), 0);

  batch.append({ id: "new-four" });
  batch.flush();
  assert.equal(timers.has(2), false);
  assert.deepEqual(timers.cancelled, [1, 2]);
  assert.equal(flushes(), 1);

  batch.append({ id: "discarded" });
  batch.clear();
  assert.equal(timers.has(3), false);
  assert.deepEqual(timers.cancelled, [1, 2, 3]);
  assert.equal(batch.size, 0);
  assert.deepEqual(batch.values(), []);
  assert.equal(flushes(), 1);
});
