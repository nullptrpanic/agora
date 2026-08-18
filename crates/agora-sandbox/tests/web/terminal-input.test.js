"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");
const {
  canForward,
  createReplayBarrier,
} = require("../../src/web/assets/terminal-input.js");

test("terminal input is never forwarded while historical output is replaying", () => {
  assert.equal(canForward(true, true, true), false);
});

test("terminal input requires an authenticated open connection outside replay", () => {
  assert.equal(canForward(true, false, true), true);
  assert.equal(canForward(false, false, true), false);
  assert.equal(canForward(true, false, false), false);
});

test("replay remains active until xterm finishes parsing every historical write", () => {
  let completed = 0;
  let finishWrite;
  const replay = createReplayBarrier(() => {
    completed += 1;
  });

  replay.start();
  replay.write((done) => {
    finishWrite = done;
  });
  replay.end();

  assert.equal(replay.active, true);
  assert.equal(completed, 0);
  finishWrite();
  assert.equal(replay.active, false);
  assert.equal(completed, 1);
});

test("a stale xterm callback cannot complete a newer replay generation", () => {
  let completed = 0;
  let finishOldWrite;
  let finishNewWrite;
  const replay = createReplayBarrier(() => {
    completed += 1;
  });

  replay.start();
  replay.write((done) => {
    finishOldWrite = done;
  });
  replay.start();
  replay.write((done) => {
    finishNewWrite = done;
  });
  replay.end();

  finishOldWrite();
  assert.equal(replay.active, true);
  assert.equal(completed, 0);
  finishNewWrite();
  assert.equal(replay.active, false);
  assert.equal(completed, 1);
});
