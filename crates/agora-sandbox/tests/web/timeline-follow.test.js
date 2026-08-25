"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");
const { isAtBottom, restoreAfterRender } = require("../../src/web/assets/timeline-follow.js");

test("bottom detection uses the documented 24 pixel threshold", () => {
  assert.equal(isAtBottom({ scrollTop: 476, clientHeight: 500, scrollHeight: 1000 }), true);
  assert.equal(isAtBottom({ scrollTop: 475, clientHeight: 500, scrollHeight: 1000 }), false);
});

test("render restoration follows new events or preserves paused history", () => {
  const container = { scrollTop: 0, scrollHeight: 1600 };
  restoreAfterRender(container, true, 220);
  assert.equal(container.scrollTop, 1600);

  restoreAfterRender(container, false, 220);
  assert.equal(container.scrollTop, 220);
});
