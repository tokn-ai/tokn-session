import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { creationModelScope, useSessionCreationModels } from "./useSessionCreationModels";
import type { ListSessionCreationModelsRequest, ModelChoice, SessionCreationModels } from "./types";

afterEach(cleanup);
const request: ListSessionCreationModelsRequest = { project_id: "repo", provider: "codex" };
const choice: ModelChoice = { model: { model_provider: "openai", model_id: "chosen", display_name: "Chosen model" }, supports_images: true };
const catalog = (overrides: Partial<SessionCreationModels> = {}): SessionCreationModels => ({ models: [choice], catalog_revision: "catalog-one",
  runtime_profile_id: "local", runtime_revision: 3, reason: null, ...overrides });
const scope = (target = request) => creationModelScope(target, "local", 3);
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>((done) => { resolve = done; }); return { promise, resolve }; }

it("loads only on demand and records the verified choice with its resolved runtime proof", async () => {
  const load = vi.fn(async () => catalog());
  const { result } = renderHook(() => useSessionCreationModels(request, scope(), true, load));
  expect(load).not.toHaveBeenCalled();
  expect(result.current.selectionFor(choice)).toBeNull();
  expect(result.current.selectionValid(null)).toBe(true);
  await act(() => result.current.loadChoices());
  expect(load).toHaveBeenCalledExactlyOnceWith(request);
  expect(result.current.selectionFor({ ...choice, supports_images: false })).toEqual({ choice, catalog_revision: "catalog-one",
    runtime_profile_id: "local", runtime_revision: 3, scope_key: scope() });
  expect(result.current.selectionFor({ ...choice, model: { ...choice.model, model_id: "invented" } })).toBeNull();
});

it.each([
  { ...request, project_id: "other" },
  { ...request, provider: "pi" as const },
  { ...request, runtime_profile_id: "explicit", runtime_revision: 3 },
  { ...request, runtime_profile_id: "explicit", runtime_revision: 4 },
  { ...request, runtime_plan_id: "automatic-changed" },
])("retires late catalogs after changing draft scope to %j", async (next) => {
  const pending = deferred<SessionCreationModels>();
  const load = vi.fn(() => pending.promise);
  const { result, rerender } = renderHook(({ target }) => useSessionCreationModels(target, scope(target), true, load), { initialProps: { target: request } });
  let reading!: Promise<void>;
  act(() => { reading = result.current.loadChoices(); });
  const staleSelect = result.current.selectionFor;
  rerender({ target: next });
  await act(async () => { pending.resolve(catalog()); await reading; });
  expect(result.current.choices).toBeNull();
  expect(staleSelect(choice)).toBeNull();
  expect(result.current.selectionFor(choice)).toBeNull();
});

it("retires catalogs from a previous machine callback even after returning to the same scope", async () => {
  const pending = deferred<SessionCreationModels>();
  const first = vi.fn(() => pending.promise);
  const second = vi.fn(async () => catalog({ catalog_revision: "other-machine" }));
  const { result, rerender } = renderHook(({ load }) => useSessionCreationModels(request, scope(), true, load), { initialProps: { load: first } });
  let reading!: Promise<void>;
  act(() => { reading = result.current.loadChoices(); });
  rerender({ load: second });
  rerender({ load: first });
  await act(async () => { pending.resolve(catalog()); await reading; });
  expect(result.current.choices).toBeNull();
});

it("keeps runtime default available after errors or unsupported discovery", async () => {
  const load = vi.fn().mockRejectedValueOnce(new Error("Catalog unavailable")).mockResolvedValueOnce(catalog({ models: [], catalog_revision: null, reason: "Unsupported" }));
  const { result } = renderHook(() => useSessionCreationModels(request, scope(), true, load));
  await act(() => result.current.loadChoices());
  expect(result.current.error).toBe("Catalog unavailable");
  expect(result.current.selectionValid(null)).toBe(true);
  expect(result.current.selectionFor(choice)).toBeNull();
  await act(() => result.current.loadChoices());
  expect(result.current.reason).toBe("Unsupported");
  expect(result.current.selectionValid(null)).toBe(true);
  expect(result.current.can_select).toBe(false);
});

it("invalidates a selected choice when a refreshed catalog or automatic runtime changes", async () => {
  const load = vi.fn().mockResolvedValueOnce(catalog()).mockResolvedValueOnce(catalog({ catalog_revision: "catalog-two" }));
  const { result, rerender } = renderHook(({ revision }) => useSessionCreationModels(request,
    creationModelScope(request, "local", revision), true, load), { initialProps: { revision: 3 } });
  await act(() => result.current.loadChoices());
  const selected = result.current.selectionFor(choice)!;
  expect(result.current.selectionValid(selected)).toBe(true);
  await act(() => result.current.loadChoices());
  expect(result.current.selectionValid(selected)).toBe(false);
  const refreshed = result.current.selectionFor(choice)!;
  rerender({ revision: 4 });
  expect(result.current.selectionValid(refreshed)).toBe(false);
});

it("pins automatic catalogs without saved profiles and rejects mismatched runtime-plan responses", async () => {
  const target = { ...request, runtime_plan_id: "automatic-one" };
  const plan_scope = scope(target);
  const load = vi.fn().mockResolvedValueOnce(catalog({ runtime_profile_id: null, runtime_revision: null, runtime_plan_id: "automatic-one" }))
    .mockResolvedValueOnce(catalog({ runtime_profile_id: null, runtime_revision: null, runtime_plan_id: "automatic-two" }));
  const { result } = renderHook(() => useSessionCreationModels(target, plan_scope, true, load));
  await act(() => result.current.loadChoices());
  const selected = result.current.selectionFor(choice)!;
  expect(selected).toMatchObject({ runtime_profile_id: null, runtime_revision: null, runtime_plan_id: "automatic-one" });
  expect(load).toHaveBeenCalledWith(target);
  await act(() => result.current.loadChoices());
  expect(result.current.error).toMatch(/runtime configuration changed/);
  expect(result.current.selectionValid(selected)).toBe(false);
  expect(result.current.selectionFor(choice)).toBeNull();
  expect(result.current.selectionValid(null)).toBe(true);
});

it("does not read models after unmount or while the workspace is unavailable", async () => {
  const load = vi.fn(async () => catalog());
  const { result, unmount } = renderHook(() => useSessionCreationModels(request, scope(), false, load));
  await act(() => result.current.loadChoices());
  unmount();
  await result.current.loadChoices();
  expect(load).not.toHaveBeenCalled();
});
