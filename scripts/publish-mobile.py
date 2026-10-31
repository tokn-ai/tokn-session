#!/usr/bin/env python3
"""Publish verified mobile outputs, retaining each platform's last good build."""

import hashlib
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen


START = "<!-- mobile-builds:start -->"
END = "<!-- mobile-builds:end -->"
STATE = re.compile(r"<!-- mobile-builds-state:(.*?) -->", re.DOTALL)
PLATFORMS = {
  "android": ("Android ARM64 debug", "android-arm64-debug", "android/app-universal-debug.apk", ".apk"),
  "ios": ("iOS ARM64 simulator", "ios-simulator-arm64", "ios/tokn-sessions-ios-simulator-arm64.tar.gz", ".tar.gz"),
}


@dataclass
class Context:
  repository: str
  channel: str
  tag: str
  sha: str
  run_id: int
  run_number: int
  run_attempt: int
  server_url: str
  artifacts: Path
  results: dict
  downloads: dict
  built_at: str

  @property
  def run_url(self):
    return f"{self.server_url}/{self.repository}/actions/runs/{self.run_id}"


@dataclass
class Outcome:
  errors: list
  summary: str


class GitHub:
  def __init__(self, repository, token, api_url="https://api.github.com"):
    self.root = f"{api_url.rstrip('/')}/repos/{repository}"
    self.token = token

  def request(self, method, path, data=None, binary=False):
    url = path if path.startswith("https://") else f"{self.root}/{path}"
    headers = {
      "Authorization": f"Bearer {self.token}",
      "Accept": "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
      "Content-Type": "application/octet-stream" if binary else "application/json",
      "User-Agent": "tokn-session-mobile-publisher",
    }
    payload = data if binary or data is None else json.dumps(data).encode()
    try:
      with urlopen(Request(url, data=payload, method=method, headers=headers), timeout=120) as response:
        body = response.read()
        return json.loads(body) if body else None
    except HTTPError as error:
      if error.code == 404 and method == "GET":
        return None
      raise RuntimeError(f"GitHub {method} {url}: HTTP {error.code}") from error

  def get_release(self, tag):
    release = self.request("GET", f"releases/tags/{quote(tag, safe='')}")
    if release is not None:
      assets = []
      page = 1
      while True:
        batch = self.request("GET", f"releases/{release['id']}/assets?per_page=100&page={page}")
        if batch is None:
          raise RuntimeError("Release disappeared while listing assets")
        assets.extend(batch)
        if len(batch) < 100:
          break
        page += 1
      release["assets"] = assets
    return release

  def resolve_tag(self, tag):
    reference = self.request("GET", f"git/ref/tags/{quote(tag, safe='')}")
    if reference is None:
      raise RuntimeError(f"Release tag {tag!r} does not exist")
    target = reference["object"]
    for _ in range(10):
      if target["type"] == "commit":
        return target["sha"]
      if target["type"] != "tag":
        break
      target = self.request("GET", f"git/tags/{target['sha']}")["object"]
    raise RuntimeError("Release tag does not resolve to a commit")

  def create_nightly(self, sha):
    return self.request("POST", "releases", {
      "tag_name": "nightly", "target_commitish": sha,
      "name": "Nightly mobile builds", "body": "", "prerelease": True,
      "make_latest": "false",
    })

  def move_nightly(self, sha):
    self.request("PATCH", "git/refs/tags/nightly", {"sha": sha, "force": True})

  def upload_asset(self, release, path, name, label):
    url = release["upload_url"].split("{", 1)[0]
    url += "?" + urlencode({"name": name, "label": label})
    return self.request("POST", url, path.read_bytes(), binary=True)

  def delete_asset(self, asset_id):
    self.request("DELETE", f"releases/assets/{asset_id}")

  def update_release(self, release_id, body):
    self.request("PATCH", f"releases/{release_id}", {"body": body})


def read_state(body):
  if START not in body and END not in body:
    return {"version": 1, "platforms": {}, "latest": None}, body, ""
  if body.count(START) != 1 or body.count(END) != 1:
    raise RuntimeError("Mobile release notes contain invalid managed markers")
  prefix, rest = body.split(START)
  block, suffix = rest.split(END)
  match = STATE.search(block)
  if not match:
    raise RuntimeError("Mobile release notes are missing their publication state")
  state = json.loads(match[1])
  if state.get("version") != 1 or not isinstance(state.get("platforms"), dict):
    raise RuntimeError("Unsupported mobile publication state")
  return state, prefix, suffix


def render_body(context, state, prefix, suffix):
  latest = state["latest"]
  run_url = f"{context.server_url}/{context.repository}/actions/runs/{latest['run_id']}"
  lines = [START, "### Mobile builds", "",
    f"Latest attempt: `{latest['sha']}` · [Build log]({run_url})", "",
    "Downloads retain the last successful build for each platform.",
    "Android is a development APK. The iOS archive runs in the simulator, not on an iPhone.", "",
    "| Platform | Latest attempt | Download | Source commit | Build date (UTC) |",
    "| --- | --- | --- | --- | --- |",
  ]
  checksums = []
  for platform, (title, _, _, _) in PLATFORMS.items():
    entry = state["platforms"].get(platform)
    result = latest["results"].get(platform, "unavailable")
    if entry:
      download = (f"{context.server_url}/{context.repository}/releases/download/"
        f"{quote(context.tag, safe='')}/{quote(entry['name'], safe='')}")
      source = f"{context.server_url}/{context.repository}/commit/{entry['sha']}"
      lines.append(f"| {title} | {result} | [Download]({download}) | [{entry['sha'][:12]}]({source}) | {entry['built_at']} |")
      checksums.append(f"{entry['sha256']}  {entry['name']}")
    else:
      lines.append(f"| {title} | {result} | Unavailable | — | — |")
  if checksums:
    lines.extend(["", "<details><summary>SHA-256 checksums</summary>", "", "```text", *checksums, "```", "", "</details>"])
  lines.extend(["", "<!-- mobile-builds-state:" + json.dumps(state, separators=(",", ":")) + " -->", END])
  return "\n\n".join(part for part in (prefix.rstrip(), "\n".join(lines), suffix.lstrip()) if part)


def payload(context, platform):
  path = context.artifacts / PLATFORMS[platform][2]
  if path.is_symlink() or any(parent.is_symlink() for parent in path.parents):
    raise RuntimeError(f"{platform}: artifact must not be a symlink")
  if not path.is_file() or path.stat().st_size == 0:
    raise RuntimeError(f"{platform}: expected nonempty artifact {path}")
  return path


def check_release(context, github, release):
  if release.get("tag_name") != context.tag or release.get("draft"):
    raise RuntimeError("Expected an existing published release for the selected tag")
  if release.get("immutable"):
    raise RuntimeError("Release is immutable; attach assets to a draft before publishing instead")
  if context.channel == "release" and github.resolve_tag(context.tag) != context.sha:
    raise RuntimeError("Release tag moved: refusing to publish artifacts from a different commit")
  if context.channel == "nightly" and not release.get("prerelease"):
    raise RuntimeError("The nightly release must be a prerelease")


def publish(context, github):
  errors = []
  messages = []
  candidates = {}
  results = dict(context.results)
  for platform in PLATFORMS:
    if context.results.get(platform) != "success":
      messages.append(f"{platform}: build {context.results.get(platform, 'unavailable')}; retained previous download")
      continue
    try:
      if context.downloads.get(platform) != "success":
        raise RuntimeError(f"{platform}: artifact download failed")
      candidates[platform] = payload(context, platform)
    except (OSError, RuntimeError) as error:
      errors.append(str(error))
      results[platform] = "artifact unavailable"
  try:
    release = github.get_release(context.tag)
    if release is None:
      if context.channel != "nightly":
        raise RuntimeError("The selected published release does not exist")
      if not candidates:
        return Outcome(errors, "\n".join(messages + errors + ["No successful artifacts; no nightly release created."]))
      release = github.create_nightly(context.sha)
    check_release(context, github, release)
    state, prefix, suffix = read_state(release.get("body") or "")
    if context.channel == "release" and state.get("sha", context.sha) != context.sha:
      raise RuntimeError("Release publication state belongs to a different commit")
    if context.channel == "release":
      state["sha"] = context.sha
  except (OSError, RuntimeError, ValueError) as error:
    return Outcome(errors + [str(error)], "\n".join(messages + errors + [str(error)]))

  assets = {asset["name"]: asset for asset in release.get("assets", [])}
  obsolete = []
  for platform, path in candidates.items():
    title, slug, _, extension = PLATFORMS[platform]
    previous = state["platforms"].get(platform)
    if context.channel == "nightly" and previous and previous["run_number"] > context.run_number:
      messages.append(f"{platform}: ignored older run {context.run_number}")
      continue
    if context.channel == "nightly":
      name = f"tokn-sessions-{slug}-{context.sha[:12]}-{context.run_id}-{context.run_attempt}{extension}"
    else:
      name = f"tokn-sessions-{context.tag.replace('/', '-')}-{slug}{extension}"
    try:
      # An already recorded successful output is never replaced on a retry.
      if previous and previous["name"] in assets and (
        context.channel == "release" or previous["run_number"] == context.run_number
      ):
        messages.append(f"{platform}: retained existing successful asset")
        continue
      checksum = hashlib.sha256(path.read_bytes()).hexdigest()
      label = f"{title}; commit {context.sha}"
      existing = assets.get(name)
      if existing:
        if existing.get("state") != "uploaded" or existing.get("label") != label:
          raise RuntimeError(f"{platform}: conflicting existing asset {name}; will not overwrite it")
        # A stable retry retains the first successful build even when timestamps differ.
        if context.channel == "nightly" and existing.get("digest") != f"sha256:{checksum}":
          raise RuntimeError(f"{platform}: existing nightly asset has a different checksum")
        asset = existing
        checksum = asset.get("digest", "").removeprefix("sha256:")
        if not re.fullmatch(r"[a-f0-9]{64}", checksum):
          raise RuntimeError(f"{platform}: existing asset has no verifiable SHA-256 digest")
      else:
        asset = github.upload_asset(release, path, name, label)
        if asset.get("state") != "uploaded" or asset.get("digest", f"sha256:{checksum}") != f"sha256:{checksum}":
          raise RuntimeError(f"{platform}: uploaded asset failed digest verification")
      state["platforms"][platform] = {
        "name": name, "asset_id": asset["id"], "sha": context.sha,
        "sha256": checksum, "run_id": context.run_id, "run_number": context.run_number,
        "built_at": existing.get("created_at", context.built_at) if existing else context.built_at,
      }
      if previous and previous["name"] in assets and previous["asset_id"] != asset["id"]:
        obsolete.append(previous["asset_id"])
      messages.append(f"{platform}: published {name}")
    except (OSError, RuntimeError, ValueError) as error:
      errors.append(str(error))
      results[platform] = "publication failed"

  # Completion order differs from dispatch order. Status and each retained platform
  # have separate clocks so a late success can fill a gap without rolling back notes.
  latest = state.get("latest")
  newest = latest is None or (context.run_number, context.run_attempt) >= (
    latest["run_number"], latest.get("run_attempt", 1)
  )
  if newest:
    state["latest"] = {
      "sha": context.sha, "run_id": context.run_id, "run_number": context.run_number,
      "run_attempt": context.run_attempt, "built_at": context.built_at, "results": results,
    }
  try:
    check_release(context, github, release)
    github.update_release(release["id"], render_body(context, state, prefix, suffix))
    # Persist the ordering clock before moving the rolling tag. A failed notes
    # update must never let a delayed run move that tag back to an older commit.
    if context.channel == "nightly" and newest:
      github.move_nightly(context.sha)
    # Only remove a previous download after its replacement and metadata are durable.
    for asset_id in obsolete:
      try:
        github.delete_asset(asset_id)
      except (OSError, RuntimeError) as error:
        errors.append(f"Could not remove superseded asset {asset_id}: {error}")
  except (OSError, RuntimeError, ValueError) as error:
    errors.append(str(error))
  return Outcome(errors, "\n".join(messages + [f"Error: {error}" for error in errors]))


def parse_context(env):
  channel, tag, sha = env["MOBILE_CHANNEL"], env["MOBILE_TAG"], env["MOBILE_SHA"]
  if channel not in ("nightly", "release") or (tag == "nightly") != (channel == "nightly"):
    raise ValueError("Invalid mobile publication channel/tag")
  if not re.fullmatch(r"[a-f0-9]{40}", sha):
    raise ValueError("Expected an exact source commit SHA")
  if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,127}", tag):
    raise ValueError("Expected a release tag containing letters, numbers, dots, slashes or dashes")
  if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", env["GITHUB_REPOSITORY"]):
    raise ValueError("Invalid GitHub repository")
  return Context(
    repository=env["GITHUB_REPOSITORY"], channel=channel, tag=tag, sha=sha,
    run_id=int(env["GITHUB_RUN_ID"]), run_number=int(env["GITHUB_RUN_NUMBER"]),
    run_attempt=int(env["GITHUB_RUN_ATTEMPT"]), server_url=env["GITHUB_SERVER_URL"].rstrip("/"),
    artifacts=Path(env["MOBILE_ARTIFACT_DIR"]),
    results={platform: env[f"MOBILE_{platform.upper()}_RESULT"] for platform in PLATFORMS},
    downloads={platform: env[f"MOBILE_{platform.upper()}_DOWNLOAD"] for platform in PLATFORMS},
    built_at=env["MOBILE_BUILT_AT"],
  )


def main():
  try:
    context = parse_context(os.environ)
    github = GitHub(context.repository, os.environ["GH_TOKEN"], os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    outcome = publish(context, github)
  except (KeyError, OSError, RuntimeError, ValueError) as error:
    outcome = Outcome([str(error)], f"Mobile publication failed: {error}")
  print(outcome.summary)
  if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(summary, "a") as output:
      output.write("### Mobile publication\n\n" + outcome.summary + "\n")
  return 1 if outcome.errors else 0


if __name__ == "__main__":
  sys.exit(main())
