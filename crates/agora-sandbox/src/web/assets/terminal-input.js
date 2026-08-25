(function (root, factory) {
  const terminalInput = factory();
  if (typeof module === "object" && module.exports) module.exports = terminalInput;
  else root.AgoraTerminalInput = terminalInput;
})(typeof globalThis === "object" ? globalThis : this, function () {
  "use strict";

  function canForward(authenticated, replaying, connected) {
    return authenticated && !replaying && connected;
  }

  function createReplayBarrier(onComplete) {
    let generation = 0;
    let pendingWrites = 0;
    let endReceived = false;
    let active = false;

    function completeIfReady(expectedGeneration) {
      if (!active || generation !== expectedGeneration || !endReceived || pendingWrites !== 0) {
        return;
      }
      active = false;
      onComplete();
    }

    function start() {
      generation += 1;
      pendingWrites = 0;
      endReceived = false;
      active = true;
    }

    function write(operation) {
      if (!active) {
        operation();
        return;
      }
      const writeGeneration = generation;
      pendingWrites += 1;
      let completed = false;
      const done = () => {
        if (completed) return;
        completed = true;
        if (!active || generation !== writeGeneration) return;
        pendingWrites -= 1;
        completeIfReady(writeGeneration);
      };
      try {
        operation(done);
      } catch (error) {
        done();
        throw error;
      }
    }

    function end() {
      if (!active) return;
      endReceived = true;
      completeIfReady(generation);
    }

    return Object.freeze({
      start,
      write,
      end,
      get active() {
        return active;
      },
    });
  }

  return Object.freeze({ canForward, createReplayBarrier });
});
