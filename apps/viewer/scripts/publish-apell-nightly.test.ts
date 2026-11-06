import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { publishNightly, type CommandResult, type CommandRunner, type PublishOptions } from "./publish-apell-nightly";

const directories: string[] = [];
afterEach(async () => { await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true }))); });

const sha = "a".repeat(40);
const previous = "b".repeat(40);
const repo = "example/project";
const ref = (name: string, value = previous) => ({ ref: `refs/${name}`, object: { sha: value, type: "commit" } });
const response = (value: unknown, code = 200): CommandResult => ({ status: code < 400 ? 0 : 1, stdout: `HTTP/2.0 ${code} Result\r\nContent-Type: application/json\r\n\r\n${JSON.stringify(value)}`, stderr: "" });

async function fixture(options: { release_missing?: boolean; tag_missing?: boolean; main_sha?: string; draft?: boolean } = {}) {
  const artifact_dir = await mkdtemp(join(tmpdir(), "apell-publish-test-"));
  directories.push(artifact_dir);
  const bundle = Buffer.from("fixture applet bytes");
  await writeFile(join(artifact_dir, "tokn-session.apel"), bundle);
  await writeFile(join(artifact_dir, "tokn-session.sha256"), `${createHash("sha256").update(bundle).digest("hex")}  tokn-session.apel\n`);
  const calls: { args: string[]; input?: string }[] = [];
  const overrides = new Map<string, CommandResult>();
  const run: CommandRunner = (args, input) => {
    calls.push({ args, input });
    const method = args[0] === "api" ? args[args.indexOf("--method") + 1] : "";
    const endpoint = args.find((argument) => argument === "graphql" || argument.startsWith("repos/")) ?? "";
    const override = overrides.get(args[0] === "api" ? `${method} ${endpoint}` : args.slice(0, 2).join(" "));
    if (override) return override;
    if (args[0] === "release") return { status: 0, stdout: "", stderr: "" };
    if (endpoint === "graphql") {
      return response({ data: { repository: { release: options.draft ? { id: 123, tag_name: "nightly", draft: true } : null } } });
    }
    if (method !== "GET") return response({});
    if (endpoint === `repos/${repo}/releases/tags/nightly`) {
      return options.release_missing || options.draft ? response({}, 404) : response({ id: 123, tag_name: "nightly", prerelease: true, draft: false, assets: [{ name: "legacy.apk" }] });
    }
    if (endpoint === `repos/${repo}/git/ref/tags/nightly`) return options.tag_missing ? response({}, 404) : response(ref("tags/nightly"));
    if (endpoint === `repos/${repo}/git/ref/heads/main`) return response(ref("heads/main", options.main_sha ?? sha));
    throw new Error(`Unexpected fixture command ${args.join(" ")}`);
  };
  const publish_options: PublishOptions = { repo, sha, ref: "refs/heads/main", artifact_dir };
  return { publish_options, calls, overrides, run };
}

function mutations<T extends { args: string[] }>(calls: T[]): T[] {
  return calls.filter(({ args }) => args[0] === "release" || (!args.includes("graphql") && args[args.indexOf("--method") + 1] !== "GET"));
}

it("updates only the two stable assets, preserving the release and moving the tag after upload", async () => {
  const value = await fixture();
  await expect(publishNightly(value.publish_options, value.run)).resolves.toBe("published");
  const writes = mutations(value.calls);
  expect(writes.map(({ args }) => args.slice(0, 2))).toEqual([["release", "upload"], ["api", "--include"], ["release", "edit"]]);
  const upload = writes[0].args;
  expect(upload).toContain(join(value.publish_options.artifact_dir, "tokn-session.apel"));
  expect(upload).toContain(join(value.publish_options.artifact_dir, "tokn-session.sha256"));
  expect(upload).toContain("--clobber");
  expect(upload.slice(-2)).toEqual(["--repo", repo]);
  const move = writes[1];
  expect(move.args).toContain(`repos/${repo}/git/refs/tags/nightly`);
  expect(JSON.parse(move.input!)).toEqual({ sha, force: true });
  const edit = writes[2].args;
  expect(edit).toContain("--prerelease");
  expect(edit).toContain("--latest=false");
  expect(edit).toContain("--draft=false");
  expect(edit[edit.indexOf("--notes") + 1]).toContain(`${repo}@nightly`);
  expect(edit[edit.indexOf("--notes") + 1]).toContain(sha);
  expect(value.calls.findIndex(({ args }) => args.includes(`repos/${repo}/git/ref/heads/main`))).toBe(value.calls.indexOf(writes[0]) - 1);
});

it("creates a missing tag and prerelease only after authoritative 404 responses", async () => {
  const value = await fixture({ release_missing: true, tag_missing: true });
  await publishNightly(value.publish_options, value.run);
  const writes = mutations(value.calls);
  expect(writes[0].args).toContain(`repos/${repo}/git/refs`);
  expect(JSON.parse(writes[0].input!)).toEqual({ ref: "refs/tags/nightly", sha });
  const create = writes.find(({ args }) => args[0] === "release" && args[1] === "create")!.args;
  expect(create).toContain("--verify-tag");
  expect(create).toContain("--prerelease");
  expect(create).toContain("--latest=false");
  expect(create).toContain("--draft");
  expect(create[create.indexOf("--target") + 1]).toBe(sha);
});

it("resumes a retained draft after an interrupted first upload and only publishes it at the end", async () => {
  const value = await fixture({ draft: true });
  await publishNightly(value.publish_options, value.run);
  const writes = mutations(value.calls);
  expect(writes.some(({ args }) => args[0] === "release" && args[1] === "create")).toBe(false);
  expect(writes.at(-1)!.args).toContain("--draft=false");
  const lookup = value.calls.find(({ args }) => args.includes("graphql"))!;
  expect(JSON.parse(lookup.input!)).toEqual({
    query: expect.stringContaining("id: databaseId tag_name: tagName draft: isDraft"),
    variables: { owner: "example", name: "project" },
  });
  expect(JSON.parse(lookup.input!).query).toMatch(/^query NightlyDraft/);
  expect(value.calls.findIndex(({ args }) => args.includes(`repos/${repo}/git/ref/heads/main`))).toBe(value.calls.indexOf(writes[0]) - 1);
});

it.each([
  { errors: [{ message: "denied" }], data: { repository: { release: null } } },
  { data: { repository: null } },
  { data: { repository: {} } },
  { data: { repository: { release: { id: 123, tag_name: "nightly", draft: false } } } },
])("does not create a release after a failed or malformed GraphQL draft lookup: %j", async (payload) => {
  const value = await fixture({ release_missing: true });
  value.overrides.set("POST graphql", response(payload));
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("publication stopped");
  expect(mutations(value.calls)).toEqual([]);
});

it("does not create a release after a draft lookup HTTP error", async () => {
  const value = await fixture({ release_missing: true });
  value.overrides.set("POST graphql", response({}, 503));
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("HTTP 503");
  expect(mutations(value.calls)).toEqual([]);
});

it("can recreate a missing tag without recreating its retained release", async () => {
  const value = await fixture({ tag_missing: true });
  await publishNightly(value.publish_options, value.run);
  expect(value.calls.some(({ args }) => args[0] === "release" && args[1] === "create")).toBe(false);
  expect(mutations(value.calls)[0].args).toContain(`repos/${repo}/git/refs`);
});

it("skips a delayed build when main has advanced without changing anything", async () => {
  const value = await fixture({ main_sha: previous });
  await expect(publishNightly(value.publish_options, value.run)).resolves.toBe("stale");
  expect(mutations(value.calls)).toEqual([]);
});

it("checks the real checksum before making any GitHub calls", async () => {
  const value = await fixture();
  await writeFile(join(value.publish_options.artifact_dir, "tokn-session.apel"), "modified bytes");
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("SHA-256");
  expect(value.calls).toEqual([]);
});

it("rejects a checksum sidecar naming a different file", async () => {
  const value = await fixture();
  await writeFile(join(value.publish_options.artifact_dir, "tokn-session.sha256"), `${"a".repeat(64)}  other.apel\n`);
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("SHA-256");
  expect(value.calls).toEqual([]);
});

it.each([401, 403, 429, 503])("does not interpret HTTP %s as a missing release", async (status) => {
  const value = await fixture();
  value.overrides.set(`GET repos/${repo}/releases/tags/nightly`, response({}, status));
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow(`HTTP ${status}`);
  expect(mutations(value.calls)).toEqual([]);
});

it("does not treat a tag lookup failure as permission to create a tag", async () => {
  const value = await fixture();
  value.overrides.set(`GET repos/${repo}/git/ref/tags/nightly`, response({}, 503));
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("HTTP 503");
  expect(mutations(value.calls)).toEqual([]);
});

it("fails closed on malformed JSON or unexpected release identity", async () => {
  for (const result of [
    { status: 0, stdout: "HTTP/2.0 200 OK\n\ninvalid", stderr: "" },
    response({ id: 123, tag_name: "stable" }),
  ]) {
    const value = await fixture();
    value.overrides.set(`GET repos/${repo}/releases/tags/nightly`, result);
    await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow("publication stopped");
    expect(mutations(value.calls)).toEqual([]);
  }
});

it("does not advance release metadata or the tag when an upload fails", async () => {
  const value = await fixture();
  value.overrides.set("release upload", { status: 1, stdout: "", stderr: "fixture failure" });
  await expect(publishNightly(value.publish_options, value.run)).rejects.toThrow(/upload failed[\s\S]*fixture failure/);
  expect(mutations(value.calls)).toHaveLength(1);
});

it("dry-run validates the artifacts and current main without mutations", async () => {
  const value = await fixture({ release_missing: true, tag_missing: true });
  await expect(publishNightly({ ...value.publish_options, dry_run: true }, value.run)).resolves.toBe("dry_run");
  expect(mutations(value.calls)).toEqual([]);
});

it("rejects non-main publication before even consulting GitHub", async () => {
  const value = await fixture();
  await expect(publishNightly({ ...value.publish_options, ref: "refs/heads/topic" }, value.run)).rejects.toThrow("only allowed from main");
  expect(value.calls).toEqual([]);
});
