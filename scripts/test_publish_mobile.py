"""Offline mobile publication tests; no GitHub API requests are made."""

import copy
import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit


SCRIPT = Path(__file__).with_name("publish-mobile.py")
SPEC = importlib.util.spec_from_file_location("publish_mobile", SCRIPT)
publisher = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = publisher
SPEC.loader.exec_module(publisher)


PAYLOADS = {
  "android": Path("android/app-universal-debug.apk"),
  "ios": Path("ios/tokn-sessions-ios-simulator-arm64.tar.gz"),
}


def platform_for(name):
  return "android" if name.endswith(".apk") else "ios"


def release(tag="nightly", body="", immutable=False):
  return {
    "id": 100,
    "tag_name": tag,
    "body": body,
    "draft": False,
    "prerelease": tag == "nightly",
    "immutable": immutable,
    "upload_url": "https://uploads.example.invalid/releases/100/assets{?name,label}",
    "assets": [],
  }


class FakeGitHub:
  def __init__(self, existing=None, tag_sha=None):
    self.release = copy.deepcopy(existing)
    self.tag_sha = tag_sha
    self.events = []
    self.failed_uploads = set()
    self.fail_updates = False
    self.fail_moves = False
    self.next_asset_id = 1

  def get_release(self, tag):
    if self.release is None or self.release["tag_name"] != tag:
      return None
    return copy.deepcopy(self.release)

  def resolve_tag(self, tag):
    return self.tag_sha

  def create_nightly(self, sha):
    self.events.append(("create", sha))
    if self.release is not None:
      raise AssertionError("A nightly release already exists")
    self.release = release()
    self.tag_sha = sha
    return copy.deepcopy(self.release)

  def upload_asset(self, target, path, name, label):
    self.events.append(("upload", name))
    platform = platform_for(name)
    if platform in self.failed_uploads:
      raise RuntimeError(f"mock {platform} upload failure")
    if any(asset["name"] == name for asset in self.release["assets"]):
      raise AssertionError(f"Asset name already exists: {name}")
    payload = path.read_bytes()
    asset = {
      "id": self.next_asset_id,
      "name": name,
      "label": label,
      "state": "uploaded",
      "size": len(payload),
      "digest": f"sha256:{hashlib.sha256(payload).hexdigest()}",
      "browser_download_url": f"https://example.invalid/releases/download/{target['tag_name']}/{name}",
    }
    self.next_asset_id += 1
    self.release["assets"].append(asset)
    return copy.deepcopy(asset)

  def delete_asset(self, asset_id):
    self.events.append(("delete", asset_id))
    assets = self.release["assets"]
    if not any(asset["id"] == asset_id for asset in assets):
      raise AssertionError(f"No asset {asset_id} to delete")
    self.release["assets"] = [asset for asset in assets if asset["id"] != asset_id]

  def update_release(self, release_id, body):
    self.events.append(("update", release_id))
    if self.fail_updates:
      raise RuntimeError("mock release notes update failure")
    if self.release["id"] != release_id:
      raise AssertionError("Unexpected release ID")
    self.release["body"] = body

  def move_nightly(self, sha):
    self.events.append(("move", sha))
    if self.fail_moves:
      raise RuntimeError("mock nightly tag move failure")
    self.tag_sha = sha

  def assets(self, platform):
    return [asset for asset in self.release["assets"] if platform_for(asset["name"]) == platform]


class Response(io.BytesIO):
  def __init__(self, payload):
    super().__init__(json.dumps(payload).encode())


class GitHubTests(unittest.TestCase):
  def setUp(self):
    self.github = publisher.GitHub("tokn-ai/tokn-session", "test-token")
    mock_urlopen = patch.object(publisher, "urlopen", side_effect=AssertionError("Unexpected network request"))
    self.urlopen = mock_urlopen.start()
    self.addCleanup(mock_urlopen.stop)

  def test_release_asset_pagination_collects_every_page(self):
    first = [{"id": index, "name": f"asset-{index}"} for index in range(100)]
    last = [{"id": 100, "name": "last-asset"}]
    self.urlopen.side_effect = [Response(release("v0.2/mobile")), Response(first), Response(last)]

    result = self.github.get_release("v0.2/mobile")

    self.assertEqual(result["assets"], first + last)
    self.assertEqual([call.args[0].full_url for call in self.urlopen.call_args_list], [
      "https://api.github.com/repos/tokn-ai/tokn-session/releases/tags/v0.2%2Fmobile",
      "https://api.github.com/repos/tokn-ai/tokn-session/releases/100/assets?per_page=100&page=1",
      "https://api.github.com/repos/tokn-ai/tokn-session/releases/100/assets?per_page=100&page=2",
    ])

  def test_annotated_tags_are_peeled_to_the_exact_commit(self):
    first, second, commit = "a" * 40, "b" * 40, "c" * 40
    self.urlopen.side_effect = [
      Response({"object": {"type": "tag", "sha": first}}),
      Response({"object": {"type": "tag", "sha": second}}),
      Response({"object": {"type": "commit", "sha": commit}}),
    ]

    self.assertEqual(self.github.resolve_tag("v0.2/mobile"), commit)

    self.assertEqual([call.args[0].full_url for call in self.urlopen.call_args_list], [
      "https://api.github.com/repos/tokn-ai/tokn-session/git/ref/tags/v0.2%2Fmobile",
      f"https://api.github.com/repos/tokn-ai/tokn-session/git/tags/{first}",
      f"https://api.github.com/repos/tokn-ai/tokn-session/git/tags/{second}",
    ])

  def test_missing_release_is_distinct_from_denied_access_and_failed_mutations(self):
    self.urlopen.side_effect = HTTPError("https://api.github.com/mock", 404, "not found", {}, io.BytesIO())
    self.assertIsNone(self.github.get_release("v0.2.0"))
    self.assertEqual(self.urlopen.call_count, 1)

    self.urlopen.side_effect = HTTPError("https://api.github.com/mock", 403, "forbidden", {}, io.BytesIO())
    with self.assertRaisesRegex(RuntimeError, "HTTP 403"):
      self.github.get_release("v0.2.0")

    self.urlopen.side_effect = HTTPError("https://api.github.com/mock", 404, "not found", {}, io.BytesIO())
    with self.assertRaisesRegex(RuntimeError, "HTTP 404"):
      self.github.create_nightly("a" * 40)

  def test_nightly_creation_and_binary_upload_preserve_api_contract(self):
    target = release()
    uploaded = {"id": 101, "name": "mobile +beta?.apk", "state": "uploaded"}
    self.urlopen.side_effect = [Response(target), Response(uploaded)]
    sha = "a" * 40

    created = self.github.create_nightly(sha)

    request = self.urlopen.call_args.args[0]
    self.assertEqual(request.get_method(), "POST")
    creation = json.loads(request.data)
    self.assertEqual(creation["tag_name"], "nightly")
    self.assertEqual(creation["target_commitish"], sha)
    self.assertIs(creation["prerelease"], True)
    self.assertEqual(creation["make_latest"], "false")
    with tempfile.TemporaryDirectory() as directory:
      path = Path(directory) / "payload.apk"
      payload = b"\x00binary APK bytes\xff\n"
      path.write_bytes(payload)
      name = "mobile +beta?/arm64.apk"
      label = "Android: verified + tested?"

      self.assertEqual(self.github.upload_asset(created, path, name, label), uploaded)

    call = self.urlopen.call_args
    request = call.args[0]
    url = urlsplit(request.full_url)
    self.assertEqual((url.scheme, url.netloc, url.path), ("https", "uploads.example.invalid", "/releases/100/assets"))
    self.assertEqual(parse_qs(url.query), {"name": [name], "label": [label]})
    self.assertEqual(request.get_method(), "POST")
    self.assertEqual(request.data, payload)
    headers = {key.lower(): value for key, value in request.header_items()}
    self.assertEqual(headers["authorization"], "Bearer test-token")
    self.assertEqual(headers["content-type"], "application/octet-stream")
    self.assertEqual(headers["accept"], "application/vnd.github+json")
    self.assertEqual(headers["x-github-api-version"], "2022-11-28")
    self.assertEqual(call.kwargs["timeout"], 120)


class PublishMobileTests(unittest.TestCase):
  def setUp(self):
    temporary = tempfile.TemporaryDirectory()
    self.addCleanup(temporary.cleanup)
    self.directory = Path(temporary.name)
    self.fixture_number = 0

  def context(self, number=10, attempt=1, results=None, downloads=None, channel="nightly", artifacts=None):
    if artifacts is None:
      self.fixture_number += 1
      artifacts = self.directory / str(self.fixture_number)
      for platform, relative in PAYLOADS.items():
        path = artifacts / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(f"{platform} build {number} attempt {attempt}".encode())
    return publisher.Context(
      repository="tokn-ai/tokn-session",
      channel=channel,
      tag="nightly" if channel == "nightly" else "v0.2.0",
      sha=hashlib.sha1(f"commit {number}".encode()).hexdigest(),
      run_id=1000 + number,
      run_number=number,
      run_attempt=attempt,
      server_url="https://github.com",
      artifacts=artifacts,
      results=results or {"android": "success", "ios": "success"},
      downloads=downloads or {"android": "success", "ios": "success"},
      built_at="2026-10-31T12:00:00Z",
    )

  def assert_one_asset(self, github, platform):
    assets = github.assets(platform)
    self.assertEqual(len(assets), 1, assets)
    return assets[0]

  def test_failed_android_does_not_block_successful_ios(self):
    github = FakeGitHub()
    context = self.context(results={"android": "failure", "ios": "success"})

    outcome = publisher.publish(context, github)

    self.assertEqual(github.assets("android"), [])
    self.assert_one_asset(github, "ios")
    self.assertIn("android", outcome.summary.lower())
    self.assertIn("ios", outcome.summary.lower())

  def test_failed_ios_does_not_block_successful_android(self):
    github = FakeGitHub()

    publisher.publish(self.context(results={"android": "success", "ios": "failure"}), github)

    self.assert_one_asset(github, "android")
    self.assertEqual(github.assets("ios"), [])

  def test_all_failed_builds_do_not_create_an_empty_nightly_release(self):
    github = FakeGitHub()

    publisher.publish(self.context(results={"android": "failure", "ios": "cancelled"}), github)

    self.assertIsNone(github.release)
    self.assertEqual(github.events, [])

  def test_failed_artifact_download_does_not_publish_platform(self):
    github = FakeGitHub()
    context = self.context(downloads={"android": "failure", "ios": "success"})

    outcome = publisher.publish(context, github)

    self.assertEqual(github.assets("android"), [])
    self.assert_one_asset(github, "ios")
    self.assertTrue(outcome.errors)
    latest = publisher.read_state(github.release["body"])[0]["latest"]
    self.assertEqual(latest["results"]["android"], "artifact unavailable")

  def test_late_older_success_can_fill_other_platform_without_rolling_back_newer_one(self):
    github = FakeGitHub()
    newer = self.context(20, results={"android": "success", "ios": "failure"})
    publisher.publish(newer, github)
    android = copy.deepcopy(self.assert_one_asset(github, "android"))
    latest = copy.deepcopy(publisher.read_state(github.release["body"])[0]["latest"])
    github.events.clear()

    older = self.context(19, results={"android": "success", "ios": "success"})
    publisher.publish(older, github)

    self.assertEqual(self.assert_one_asset(github, "android"), android)
    self.assert_one_asset(github, "ios")
    self.assertEqual(publisher.read_state(github.release["body"])[0]["latest"], latest)
    self.assertEqual(github.tag_sha, newer.sha)
    self.assertFalse(any(event[0] == "upload" and platform_for(event[1]) == "android" for event in github.events))
    self.assertNotIn(("delete", android["id"]), github.events)

  def test_older_run_cannot_roll_back_assets_or_latest_attempt_status(self):
    github = FakeGitHub()
    publisher.publish(self.context(20), github)
    assets = copy.deepcopy(github.release["assets"])
    body = github.release["body"]
    tag_sha = github.tag_sha
    github.events.clear()

    publisher.publish(self.context(19, results={"android": "failure", "ios": "success"}), github)

    self.assertEqual(github.release["assets"], assets)
    self.assertEqual(github.release["body"], body)
    self.assertEqual(github.tag_sha, tag_sha)
    self.assertFalse(any(event[0] in {"upload", "delete", "move"} for event in github.events))

  def test_nightly_retry_adds_missing_platform_without_replacing_successful_one(self):
    github = FakeGitHub()
    publisher.publish(self.context(results={"android": "success", "ios": "failure"}), github)
    android = copy.deepcopy(self.assert_one_asset(github, "android"))
    github.events.clear()

    publisher.publish(self.context(attempt=2), github)

    self.assertEqual(self.assert_one_asset(github, "android"), android)
    self.assert_one_asset(github, "ios")
    self.assertEqual([event[0] for event in github.events if event[0] in {"upload", "delete"}], ["upload"])

  def test_older_attempt_can_fill_a_gap_without_replacing_newer_attempt_status(self):
    github = FakeGitHub()
    publisher.publish(self.context(attempt=3, results={"android": "success", "ios": "failure"}), github)
    latest = copy.deepcopy(publisher.read_state(github.release["body"])[0]["latest"])
    android = copy.deepcopy(self.assert_one_asset(github, "android"))

    publisher.publish(self.context(attempt=2, results={"android": "failure", "ios": "success"}), github)

    self.assertEqual(publisher.read_state(github.release["body"])[0]["latest"], latest)
    self.assertEqual(self.assert_one_asset(github, "android"), android)
    self.assert_one_asset(github, "ios")

  def test_upload_failure_keeps_previous_asset_and_other_platform_can_advance(self):
    github = FakeGitHub()
    publisher.publish(self.context(10), github)
    android = copy.deepcopy(self.assert_one_asset(github, "android"))
    ios = copy.deepcopy(self.assert_one_asset(github, "ios"))
    github.failed_uploads.add("android")
    github.events.clear()

    outcome = publisher.publish(self.context(11), github)

    self.assertTrue(outcome.errors)
    self.assertEqual(self.assert_one_asset(github, "android"), android)
    self.assertNotEqual(self.assert_one_asset(github, "ios")["id"], ios["id"])
    self.assertNotIn(("delete", android["id"]), github.events)
    latest = publisher.read_state(github.release["body"])[0]["latest"]
    self.assertEqual(latest["results"], {"android": "publication failed", "ios": "success"})
    upload_index = next(index for index, event in enumerate(github.events) if event[0] == "upload" and platform_for(event[1]) == "ios")
    self.assertLess(upload_index, github.events.index(("delete", ios["id"])))

  def test_nightly_replaces_assets_with_unique_names_before_deleting_previous_downloads(self):
    github = FakeGitHub()
    publisher.publish(self.context(10), github)
    old_assets = copy.deepcopy(github.release["assets"])
    github.events.clear()

    publisher.publish(self.context(11), github)

    for old in old_assets:
      platform = platform_for(old["name"])
      new = self.assert_one_asset(github, platform)
      self.assertNotEqual(new["name"], old["name"])
      self.assertLess(github.events.index(("upload", new["name"])), github.events.index(("delete", old["id"])))

  def test_release_retry_adds_missing_platform_and_preserves_previous_success(self):
    context = self.context(channel="release", results={"android": "success", "ios": "failure"})
    github = FakeGitHub(release(context.tag, "Release notes written by a maintainer.\n"), context.sha)
    publisher.publish(context, github)
    android = copy.deepcopy(self.assert_one_asset(github, "android"))
    github.events.clear()

    retry = self.context(attempt=2, channel="release")
    publisher.publish(retry, github)

    self.assertEqual(self.assert_one_asset(github, "android"), android)
    self.assert_one_asset(github, "ios")
    self.assertEqual([event[0] for event in github.events if event[0] in {"upload", "delete", "move"}], ["upload"])
    self.assertIn("Release notes written by a maintainer.", github.release["body"])
    github.events.clear()
    publisher.publish(self.context(attempt=3, channel="release"), github)
    self.assertFalse(any(event[0] in {"upload", "delete", "move"} for event in github.events))

  def test_failed_notes_update_retains_previous_download_and_retry_recovers_uploaded_asset(self):
    github = FakeGitHub()
    original = self.context(10)
    publisher.publish(original, github)
    android = copy.deepcopy(self.assert_one_asset(github, "android"))
    body = github.release["body"]
    replacement = self.context(11, results={"android": "success", "ios": "failure"})
    github.fail_updates = True
    github.events.clear()

    outcome = publisher.publish(replacement, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.release["body"], body)
    self.assertEqual(github.tag_sha, original.sha)
    self.assertIn(android, github.assets("android"))
    orphan = next(asset for asset in github.assets("android") if asset["id"] != android["id"])
    self.assertFalse(any(event[0] in {"delete", "move"} for event in github.events))
    github.fail_updates = False
    github.events.clear()

    recovered = publisher.publish(replacement, github)

    self.assertEqual(recovered.errors, [])
    self.assertEqual(self.assert_one_asset(github, "android"), orphan)
    self.assertEqual(github.tag_sha, replacement.sha)
    self.assertFalse(any(event[0] == "upload" for event in github.events))
    self.assertIn(("delete", android["id"]), github.events)

  def test_failed_tag_move_persists_clock_before_late_run_and_retry(self):
    github = FakeGitHub()
    original = self.context(10)
    publisher.publish(original, github)
    newer = self.context(12, results={"android": "success", "ios": "failure"})
    github.fail_moves = True
    github.events.clear()

    outcome = publisher.publish(newer, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.tag_sha, original.sha)
    self.assertLess(github.events.index(("update", github.release["id"])), github.events.index(("move", newer.sha)))
    state = publisher.read_state(github.release["body"])[0]
    self.assertEqual(state["latest"]["sha"], newer.sha)
    latest = copy.deepcopy(state["latest"])
    android = copy.deepcopy(state["platforms"]["android"])
    github.fail_moves = False
    github.events.clear()

    publisher.publish(self.context(11), github)

    state = publisher.read_state(github.release["body"])[0]
    self.assertEqual(state["latest"], latest)
    self.assertEqual(state["platforms"]["android"], android)
    self.assertEqual(state["platforms"]["ios"]["run_number"], 11)
    self.assertEqual(github.tag_sha, original.sha)
    self.assertFalse(any(event[0] == "move" for event in github.events))

    retried = publisher.publish(newer, github)

    self.assertEqual(retried.errors, [])
    self.assertEqual(github.tag_sha, newer.sha)

  def test_moved_release_tag_is_rejected_before_any_mutation(self):
    context = self.context(channel="release")
    github = FakeGitHub(release(context.tag), "f" * 40)

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.events, [])

  def test_immutable_release_is_rejected_before_any_mutation(self):
    context = self.context(channel="release")
    github = FakeGitHub(release(context.tag, immutable=True), context.sha)

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.events, [])

  def test_immutable_nightly_is_rejected_before_any_mutation(self):
    context = self.context()
    github = FakeGitHub(release(immutable=True), context.sha)

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.events, [])

  def test_missing_payload_is_rejected_without_blocking_other_platform(self):
    context = self.context()
    (context.artifacts / PAYLOADS["android"]).unlink()
    github = FakeGitHub()

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.assets("android"), [])
    self.assert_one_asset(github, "ios")

  def test_symlinked_payload_is_rejected_without_blocking_other_platform(self):
    context = self.context()
    path = context.artifacts / PAYLOADS["android"]
    path.unlink()
    outside = self.directory / "outside.apk"
    outside.write_bytes(b"unexpected external bytes")
    path.symlink_to(outside)
    github = FakeGitHub()

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.assets("android"), [])
    self.assert_one_asset(github, "ios")

  def test_empty_payload_is_rejected_without_blocking_other_platform(self):
    context = self.context()
    (context.artifacts / PAYLOADS["android"]).write_bytes(b"")
    github = FakeGitHub()

    outcome = publisher.publish(context, github)

    self.assertTrue(outcome.errors)
    self.assertEqual(github.assets("android"), [])
    self.assert_one_asset(github, "ios")

  def test_publication_preserves_human_notes_before_and_after_managed_status(self):
    github = FakeGitHub()
    publisher.publish(self.context(10), github)
    prefix = "# Maintainer introduction\n\nThis is hand-written.\n\n"
    suffix = "\n\n## Known issue\nKeep these instructions intact.\n"
    github.release["body"] = prefix + github.release["body"] + suffix

    publisher.publish(self.context(11), github)

    self.assertTrue(github.release["body"].startswith(prefix))
    self.assertTrue(github.release["body"].endswith(suffix))


if __name__ == "__main__":
  unittest.main()
