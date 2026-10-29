import { createUuid } from "./id";
import { browserTranslationEngine } from "./browserTranslation";
import { cancelTranslation, getTranslationStatus, openTranslationSettings, translateText } from "./tauri";
import { isMobileApp } from "./platform";
import { isDesktop } from "./transport";
import type { TranslationFailure, TranslationStatus } from "./types";

/** Tauri versions may reject with a structured failure, a string, or an Error. */
export function normalizeTranslationFailure(error: unknown): TranslationFailure {
  if (error !== null && typeof error === "object" && "message" in error && typeof error.message === "string") {
    return {
      message: error.message,
      recovery: "recovery" in error && error.recovery === "translation_settings" ? "translation_settings" : null,
    };
  }
  return { message: String(error), recovery: null };
}

/** Only the native engine boundary can attach a local settings recovery. */
export class NativeTranslationError extends Error {
  readonly recovery: TranslationFailure["recovery"];

  constructor(error: unknown) {
    const failure = normalizeTranslationFailure(error);
    super(failure.message);
    this.recovery = failure.recovery;
  }
}

export interface TranslationProgress {
  message: string;
  resume?: () => void;
}

/** A job owns its models and pending work until completion or cancellation. */
export interface TranslationJob {
  translate(texts: string[]): Promise<string[]>;
  /** Idempotent and non-throwing, including during pending model creation. */
  dispose(): void;
}

export interface TranslationEngine {
  label: string;
  description: string;
  getStatus(): Promise<TranslationStatus>;
  start(onProgress: (progress: TranslationProgress) => void): TranslationJob;
  openSettings?(): Promise<void>;
}

const appleTranslationEngine: TranslationEngine = {
  label: "Apple Translation",
  description: "Translate to Simplified Chinese with Apple Translation. macOS may ask to download languages.",
  getStatus() { return getTranslationStatus(); },
  openSettings() { return openTranslationSettings(); },
  start() {
    let disposed = false;
    let request_id: string | null = null;
    return {
      async translate(texts) {
        if (disposed) throw new Error("Translation cancelled.");
        const id = createUuid();
        request_id = id;
        try {
          const response = await translateText({ request_id: id, texts, target_language: "zh-Hans" })
            .catch((error: unknown) => { throw new NativeTranslationError(error); });
          if (disposed) throw new Error("Translation cancelled.");
          return response.texts;
        } finally {
          if (request_id === id) request_id = null;
        }
      },
      dispose() {
        disposed = true;
        if (request_id) void cancelTranslation(request_id).catch(() => {});
        request_id = null;
      },
    };
  },
};

export function createTranslationEngine(): TranslationEngine {
  return isDesktop() && !isMobileApp() ? appleTranslationEngine : browserTranslationEngine;
}
