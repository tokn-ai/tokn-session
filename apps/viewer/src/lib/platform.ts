/** Set by the native mobile shell before the frontend loads. Never infer local
 * machine capability from screen size or a phone's user agent. */
export function isMobileApp(): boolean {
  return "__TOKN_MOBILE__" in window && window.__TOKN_MOBILE__ === true;
}
