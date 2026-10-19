import { describe, expect, it } from "vitest";
import { normalizeTranslationFailure } from "./translationEngine";

describe("translation failure compatibility", () => {
  it.each([
    ["Legacy string failure.", { message: "Legacy string failure.", recovery: null }],
    [new Error("Legacy Error failure."), { message: "Legacy Error failure.", recovery: null }],
    [{ message: "Native engine failure.", recovery: "translation_settings" }, { message: "Native engine failure.", recovery: "translation_settings" }],
    [{ message: "Unsupported language.", recovery: null }, { message: "Unsupported language.", recovery: null }],
    [{ message: "Unknown recovery.", recovery: "open_url" }, { message: "Unknown recovery.", recovery: null }],
    [{ message: "Malformed recovery.", recovery: ["translation_settings"] }, { message: "Malformed recovery.", recovery: null }],
    [null, { message: "null", recovery: null }],
  ])("preserves the failure message and accepts only the known recovery: %j", (error, failure) => {
    expect(normalizeTranslationFailure(error)).toEqual(failure);
  });
});
