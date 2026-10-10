import { expect, it } from "vitest";
import { parseNativePasskeyRequest } from "./NativePasskey";
import { encodeBase64Url } from "../lib/hub";
const encode = (value: unknown) => `#request=${encodeBase64Url(new TextEncoder().encode(JSON.stringify(value)).buffer)}`;
const request = { operation: "login", options: { publicKey: { challenge: "abc" } }, callback_url: `http://127.0.0.1:5555/passkey/${"A".repeat(43)}` };
it("accepts only the app's bounded numeric-loopback one-shot return address", () => {
  expect(parseNativePasskeyRequest(encode(request))).toEqual(request);
  for (const callback_url of ["https://evil.example", "http://localhost:5555/passkey/token", "http://127.0.0.1:5555/other", `${request.callback_url}?redirect=https://evil.example`]) {
    expect(() => parseNativePasskeyRequest(encode({ ...request, callback_url }))).toThrow("return address");
  }
  expect(() => parseNativePasskeyRequest("x".repeat(128 * 1024 + 1))).toThrow("size limit");
});
