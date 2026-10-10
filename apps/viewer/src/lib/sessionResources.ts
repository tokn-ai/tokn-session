import type { EventDetail } from "./types";
export type ResourceKind = "group" | "tool" | "inspect";
export type ResourceCoverage = { status: "missing" | "loading" | "complete" | "stale" | "failed"; error?: string; generation?: string; revision?: string };
interface Resource extends ResourceCoverage { value?: EventDetail; bytes: number }

/** Resource coverage is independent of delivery level and disclosure state. */
export class SessionResources {
  private entries = new Map<string, Resource>();
  private key(session_key: string, kind: ResourceKind, event_key: string) { return JSON.stringify([session_key, kind, event_key]); }
  coverage(session_key: string, kind: ResourceKind, event_key: string): ResourceCoverage {
    const value = this.entries.get(this.key(session_key, kind, event_key));
    return value ? { status: value.status, generation: value.generation, revision: value.revision,
      ...(value.error ? { error: value.error } : {}) } : { status: "missing" };
  }
  set(session_key: string, kind: ResourceKind, event_key: string, coverage: ResourceCoverage, value?: EventDetail) {
    const key = this.key(session_key, kind, event_key);
    const previous = this.entries.get(key);
    this.entries.delete(key);
    const payload = value ?? previous?.value;
    this.entries.set(key, { ...coverage, value: payload, bytes: payload ? JSON.stringify(payload).length * 2 : 0 });
    while (this.entries.size > 256 || [...this.entries.values()].reduce((sum, entry) => sum + entry.bytes, 0) > 32 * 1024 * 1024) {
      this.entries.delete(this.entries.keys().next().value!);
    }
  }
  detail(session_key: string, kind: ResourceKind, event_key: string) {
    const key = this.key(session_key, kind, event_key), entry = this.entries.get(key);
    if (!entry || entry.status !== "complete") return null;
    this.entries.delete(key); this.entries.set(key, entry);
    return entry.value ?? null;
  }
  payloadKeys(session_key: string): string[] {
    return [...new Set([...this.entries.keys()].map((key) => JSON.parse(key) as string[])
      .filter(([owner, kind]) => owner === session_key && kind !== "group").map(([, , event_key]) => event_key))];
  }
  invalidate(session_key: string, event_key?: string) {
    for (const [key, entry] of this.entries) {
      const [owner, , event] = JSON.parse(key) as string[];
      if (owner === session_key && (!event_key || event === event_key)) this.entries.set(key, { ...entry, status: "stale" });
    }
  }
  remove(session_key: string) {
    for (const key of this.entries.keys()) if (JSON.parse(key)[0] === session_key) this.entries.delete(key);
  }
}
