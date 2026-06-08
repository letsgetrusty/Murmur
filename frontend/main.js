// Overlay is render-only for Phase 0. Show/hide is driven by the Rust backend
// in response to global hotkeys; this script exists so the page mounts cleanly.
window.addEventListener("DOMContentLoaded", () => {
  console.log("murmur overlay ready");
});
