((root, factory) => {
  "use strict";

  const timelineFollow = factory();
  if (typeof module === "object" && module.exports) module.exports = timelineFollow;
  else root.AgoraTimelineFollow = timelineFollow;
})(typeof globalThis === "undefined" ? this : globalThis, () => {
  "use strict";

  const BOTTOM_THRESHOLD_PX = 24;

  function isAtBottom(container) {
    return container.scrollHeight - container.clientHeight - container.scrollTop <= BOTTOM_THRESHOLD_PX;
  }

  function restoreAfterRender(container, following, previousScrollTop) {
    container.scrollTop = following ? container.scrollHeight : previousScrollTop;
  }

  return Object.freeze({ isAtBottom, restoreAfterRender });
});
