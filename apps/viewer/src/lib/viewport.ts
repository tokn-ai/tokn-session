// iOS WebKit can resize only the visual viewport when the keyboard opens.
// Keep fixed app surfaces in that visible area without interfering with zoom.
export function installViewportSizing() {
  const viewport = window.visualViewport;
  const style = document.documentElement.style;
  const update = () => {
    if (viewport && viewport.scale !== 1) return;
    style.setProperty("--app-height", `${viewport?.height ?? window.innerHeight}px`);
    style.setProperty("--app-top", `${viewport?.offsetTop ?? 0}px`);
  };
  update();
  window.addEventListener("resize", update);
  viewport?.addEventListener("resize", update);
  viewport?.addEventListener("scroll", update);
  return () => {
    window.removeEventListener("resize", update);
    viewport?.removeEventListener("resize", update);
    viewport?.removeEventListener("scroll", update);
    style.removeProperty("--app-height");
    style.removeProperty("--app-top");
  };
}
