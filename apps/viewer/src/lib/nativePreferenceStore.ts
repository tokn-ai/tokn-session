import { useSyncExternalStore } from "react";
import type { NativePreferenceUpdate } from "./types";

export interface NativePreferenceSaveStatus {
  saving: boolean;
  error: string | null;
}

const IDLE: NativePreferenceSaveStatus = Object.freeze({ saving: false, error: null });

/** One ordered writer per preference file; display snapshots remain synchronous. */
export class NativePreferenceStore<T> {
  private value: T | undefined;
  private write: ((value: T, write_id: string) => Promise<NativePreferenceUpdate<T>>) | undefined;
  private edit_revision = 0;
  private saved_revision = 0;
  private native_revision = 0;
  private latest_native: T | undefined;
  private generation = 0;
  private running = false;
  private status = IDLE;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly equals: (left: T, right: T) => boolean, private readonly changed: () => void) {}

  read(): T | undefined { return this.value; }

  initialize(value: T, write: (value: T, write_id: string) => Promise<NativePreferenceUpdate<T>>, revision: number) {
    this.generation++;
    this.value = Object.freeze(value);
    this.write = write;
    this.edit_revision = 0;
    this.saved_revision = 0;
    this.native_revision = revision;
    this.latest_native = this.value;
    this.running = false;
    this.setStatus(IDLE);
    this.changed();
  }

  update(value: T) {
    if (this.value !== undefined && this.equals(this.value, value)) return;
    this.value = Object.freeze(value);
    this.edit_revision++;
    this.changed();
    void this.flush();
  }

  /** Native revisions order events and results across this app's windows. */
  receive(update: NativePreferenceUpdate<T>) {
    if (!this.acceptNative(update)) return;
    if (this.edit_revision !== this.saved_revision || this.running) return;
    this.publishValue(update.preferences);
  }

  retry = () => {
    if (this.value === undefined || !this.write || this.running) return;
    if (this.edit_revision === this.saved_revision) this.edit_revision++;
    void this.flush();
  };

  getStatus = () => this.status;

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  private acceptNative(update: NativePreferenceUpdate<T>): boolean {
    if (update.revision <= this.native_revision) return false;
    this.native_revision = update.revision;
    this.latest_native = Object.freeze(update.preferences);
    return true;
  }

  private publishValue(value: T) {
    if (this.value !== undefined && this.equals(this.value, value)) return;
    this.value = Object.freeze(value);
    this.changed();
  }

  private setStatus(next: NativePreferenceSaveStatus) {
    if (next.saving === this.status.saving && next.error === this.status.error) return;
    this.status = Object.freeze(next);
    for (const listener of this.listeners) listener();
  }

  private async flush() {
    if (this.running || !this.write || this.value === undefined) return;
    const generation = this.generation;
    this.running = true;
    this.setStatus({ saving: true, error: null });
    try {
      while (this.edit_revision !== this.saved_revision) {
        const revision = this.edit_revision;
        const value = this.value;
        try {
          const write_id = crypto.randomUUID();
          const saved = await this.write(value, write_id);
          if (generation !== this.generation) return;
          if (saved.write_id !== write_id) throw new Error("Native preference save returned a different write identity.");
          this.acceptNative(saved);
          this.saved_revision = revision;
          // A newer queued draft remains authoritative until its own save. Once
          // acknowledged, the newest native revision is the winner even if its
          // event arrived before an older request result on another channel.
          if (revision === this.edit_revision && this.latest_native !== undefined) this.publishValue(this.latest_native);
        } catch (error) {
          if (generation !== this.generation) return;
          // An older failed write must not strand a newer queued edit.
          if (revision !== this.edit_revision) continue;
          this.setStatus({ saving: false, error: error instanceof Error ? error.message : String(error) });
          return;
        }
      }
      this.setStatus(IDLE);
    } finally {
      if (generation === this.generation) {
        this.running = false;
        // A subscriber may synchronously edit when the idle status is published.
        if (!this.status.error && this.edit_revision !== this.saved_revision) void this.flush();
      }
    }
  }
}

export function useNativePreferenceSaveStatus<T>(store: NativePreferenceStore<T>) {
  return useSyncExternalStore(store.subscribe, store.getStatus, store.getStatus);
}
