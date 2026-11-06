import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export interface CommandResult {
  status: number;
  stdout: string;
  stderr: string;
}

export type CommandRunner = (args: string[], input?: string) => CommandResult;

export interface PublishOptions {
  repo: string;
  sha: string;
  ref: string;
  artifact_dir: string;
  dry_run?: boolean;
}

const SHA = /^[a-f0-9]{40}$/;
const ASSETS = ["tokn-session.apel", "tokn-session.sha256"];

function runGh(args: string[], input?: string): CommandResult {
  const result = spawnSync("gh", args, {
    encoding: "utf8", input, timeout: 120_000, maxBuffer: 4 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  return { status: result.status ?? 1, stdout: result.stdout, stderr: result.stderr };
}

function api(run: CommandRunner, endpoint: string, method = "GET", payload?: unknown, missing_ok = false): unknown {
  const args = ["api", "--include", "--method", method, "--header", "Cache-Control: no-cache", endpoint];
  if (payload !== undefined) args.push("--input", "-");
  const result = run(args, payload === undefined ? undefined : JSON.stringify(payload));
  const separator = /\r?\n\r?\n/.exec(result.stdout);
  const status = /^HTTP\/[\d.]+\s+(\d{3})(?:\s|$)/.exec(result.stdout);
  if (!separator || !status) throw new Error("GitHub CLI did not return an HTTP response; publication stopped.");
  const code = Number(status[1]);
  if (code === 404 && missing_ok) return null;
  if (result.status !== 0 || code < 200 || code >= 300) {
    throw new Error(`GitHub API returned HTTP ${code} for ${endpoint}; publication stopped.`);
  }
  try {
    return JSON.parse(result.stdout.slice(separator.index + separator[0].length));
  } catch {
    throw new Error(`GitHub API returned invalid JSON for ${endpoint}; publication stopped.`);
  }
}

function object(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`Invalid GitHub ${label}; publication stopped.`);
  }
  return value as Record<string, unknown>;
}

function nightlyRelease(run: CommandRunner, repo: string): unknown {
  const published = api(run, `repos/${repo}/releases/tags/nightly`, "GET", undefined, true);
  if (published !== null) return published;
  // REST's tag lookup excludes drafts. Query the exact pending tag rather than
  // treating an interrupted first publication as permission to create a second release.
  const [owner, name] = repo.split("/");
  const response = object(api(run, "graphql", "POST", {
    query: "query NightlyDraft($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { release(tagName: \"nightly\") { id: databaseId tag_name: tagName draft: isDraft } } }",
    variables: { owner, name },
  }), "draft lookup");
  if ("errors" in response && (!Array.isArray(response.errors) || response.errors.length > 0)) {
    throw new Error("GitHub GraphQL draft lookup failed; publication stopped.");
  }
  const repository = object(object(response.data, "draft lookup data").repository, "draft lookup repository");
  if (!("release" in repository)) throw new Error("Invalid GitHub draft lookup; publication stopped.");
  if (repository.release === null) return null;
  const draft = object(repository.release, "draft release");
  if (draft.draft !== true) throw new Error("Invalid GitHub draft release; publication stopped.");
  return draft;
}

function refSha(value: unknown, ref: string): string {
  const response = object(value, "reference");
  const target = object(response.object, "reference target");
  if (response.ref !== ref || typeof target.sha !== "string" || !SHA.test(target.sha)) {
    throw new Error("Invalid GitHub reference; publication stopped.");
  }
  return target.sha;
}

function releaseCommand(run: CommandRunner, repo: string, args: string[]) {
  const result = run(["release", ...args, "--repo", repo]);
  if (result.status !== 0) {
    const detail = result.stderr.trim().slice(0, 2048);
    throw new Error(`GitHub release ${args[0]} failed; publication stopped.${detail ? `\n${detail}` : ""}`);
  }
}

export async function publishNightly(options: PublishOptions, run: CommandRunner = runGh): Promise<"published" | "stale" | "dry_run"> {
  const { repo, sha, ref, artifact_dir, dry_run } = options;
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(repo) || !SHA.test(sha)) {
    throw new Error("A GitHub repository and full commit SHA are required.");
  }
  if (ref !== "refs/heads/main") throw new Error("Nightly publication is only allowed from main.");
  const paths = ASSETS.map((name) => join(artifact_dir, name));
  const [bundle, checksum] = await Promise.all([readFile(paths[0]), readFile(paths[1], "utf8")]);
  const expected = /^([a-f0-9]{64})  tokn-session\.apel\r?\n?$/.exec(checksum);
  if (!bundle.length || !expected || createHash("sha256").update(bundle).digest("hex") !== expected[1]) {
    throw new Error("The applet package does not match its SHA-256 sidecar; publication stopped.");
  }
  const release = nightlyRelease(run, repo);
  if (release !== null) {
    const current = object(release, "release");
    if (current.tag_name !== "nightly" || !Number.isSafeInteger(current.id) || Number(current.id) <= 0) {
      throw new Error("Invalid GitHub nightly release; publication stopped.");
    }
  }
  const tag = api(run, `repos/${repo}/git/ref/tags/nightly`, "GET", undefined, true);
  if (tag !== null) refSha(tag, "refs/tags/nightly");
  // Queued or retried older builds must not replace the assets of newer main.
  // The publisher job serializes mutations; check freshness after all preflight reads.
  if (refSha(api(run, `repos/${repo}/git/ref/heads/main`), "refs/heads/main") !== sha) return "stale";
  if (dry_run) return "dry_run";

  if (tag === null) {
    api(run, `repos/${repo}/git/refs`, "POST", { ref: "refs/tags/nightly", sha });
  }
  const notes = [
    "Tokn Sessions for Apell.",
    `Built from commit \`${sha}\`.`,
    `Install or update from \`${repo}@nightly\` in Apell.`,
    "The SHA-256 sidecar verifies the accompanying applet package.",
  ].join("\n\n");
  const metadata = ["nightly", "--title", "Nightly Apell applet", "--notes", notes, "--prerelease", "--latest=false"];
  if (release === null) {
    releaseCommand(run, repo, ["create", ...metadata, "--target", sha, "--verify-tag", "--draft"]);
  }
  // Clobber only our two stable asset names, retaining the release ID and all
  // unrelated assets. Existing tag pointers and published notes advance only
  // after uploads succeed; a newly created release remains a draft until then.
  releaseCommand(run, repo, ["upload", "nightly", ...paths, "--clobber"]);
  api(run, `repos/${repo}/git/refs/tags/nightly`, "PATCH", { sha, force: true });
  releaseCommand(run, repo, ["edit", ...metadata, "--draft=false"]);
  return "published";
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const dry_run = process.argv.slice(2).includes("--dry-run");
    if (process.argv.slice(2).some((argument) => argument !== "--dry-run")) throw new Error("Only --dry-run is supported.");
    if (!dry_run && !process.env.GH_TOKEN) throw new Error("GH_TOKEN is required for nightly publication.");
    const result = await publishNightly({
      repo: process.env.GITHUB_REPOSITORY ?? "",
      sha: process.env.GITHUB_SHA ?? "",
      ref: process.env.GITHUB_REF ?? "",
      artifact_dir: resolve(dirname(fileURLToPath(import.meta.url)), "../../../artifacts"),
      dry_run,
    });
    console.log(result === "stale" ? "Skipped stale nightly build; main has advanced." : result === "dry_run" ? "Verified nightly assets and main; dry run made no changes." : "Published Tokn Sessions to nightly.");
  } catch (error) {
    console.error(error instanceof Error ? error.message : "Nightly publication failed.");
    process.exitCode = 1;
  }
}
