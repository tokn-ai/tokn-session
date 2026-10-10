"""Offline publication tests: every Cargo process and registry request is mocked."""

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError, URLError


SCRIPT = Path(__file__).with_name("publish-crates.py")
SPEC = importlib.util.spec_from_file_location("publish_crates", SCRIPT)
publisher = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = publisher
SPEC.loader.exec_module(publisher)


def package(name, version="0.1.1", publish=None):
  return {
    "id": f"path+file:///workspace/{name}#{version}",
    "name": name,
    "version": version,
    "publish": publish,
  }


def metadata(*packages):
  return {
    "workspace_members": [item["id"] for item in packages],
    "packages": list(packages),
    "target_directory": "/workspace/target",
  }


def registry_response(name, version="0.1.1", yanked=False):
  return {"version": {"crate": name, "num": version, "yanked": yanked}}


def http_error(status):
  return HTTPError("https://crates.io/api/v1/crates/test/0.1.1", status, "mock response", {}, io.BytesIO())


class Response(io.BytesIO):
  def __init__(self, payload, status=200):
    super().__init__(payload if isinstance(payload, bytes) else json.dumps(payload).encode())
    self.status = status


class PublishTests(unittest.TestCase):
  def setUp(self):
    self.stack = contextlib.ExitStack()
    self.addCleanup(self.stack.close)
    self.stdout = io.StringIO()
    self.stderr = io.StringIO()
    self.stack.enter_context(contextlib.redirect_stdout(self.stdout))
    self.stack.enter_context(contextlib.redirect_stderr(self.stderr))
    self.cargo_version = "cargo 1.99.0 (mock)\n"
    self.workspace = metadata(package("alpha"), package("beta"))
    self.publish_status = 0
    self.commands = []
    self.run = self.stack.enter_context(patch.object(publisher.subprocess, "run", side_effect=self.cargo))
    self.urlopen = self.stack.enter_context(patch.object(publisher, "urlopen", side_effect=AssertionError("Unexpected network request")))
    self.sleep = self.stack.enter_context(patch.object(publisher.time, "sleep"))

  def cargo(self, command, **options):
    self.commands.append(command)
    self.assertEqual(options["cwd"], publisher.MANIFEST.parent)
    if command == ["cargo", "--version"]:
      return subprocess.CompletedProcess(command, 0, stdout=self.cargo_version)
    if command[1] == "metadata":
      self.assertIn("--no-deps", command)
      self.assertIn("--locked", command)
      self.assertEqual(command[-2:], ["--manifest-path", str(publisher.MANIFEST)])
      return subprocess.CompletedProcess(command, 0, stdout=json.dumps(self.workspace))
    if command[1] == "publish":
      return subprocess.CompletedProcess(command, self.publish_status)
    self.fail(f"Unexpected Cargo command: {command!r}")

  def publish_commands(self):
    return [command for command in self.commands if command[1] == "publish"]

  def assert_no_publish(self):
    self.assertEqual(self.publish_commands(), [])

  def test_mixed_versions_publish_only_pending_exact_versions(self):
    self.workspace = metadata(package("zeta"), package("alpha"), package("beta", "0.2.0"))
    self.urlopen.side_effect = [
      Response(registry_response("alpha")), http_error(404), http_error(404),
    ]

    self.assertEqual(publisher.main([]), 0)
    self.assertEqual(self.publish_commands(), [[
      "cargo", "publish", "--registry", "crates-io", "--locked",
      "--manifest-path", str(publisher.MANIFEST),
      "--target-dir", "/workspace/target/release-publish",
      "--package", "beta@0.2.0", "--package", "zeta@0.1.1",
    ]])
    self.assertIn("Skip alpha@0.1.1 (published)", self.stdout.getvalue())
    self.assertIn("2 pending; 1 already published", self.stdout.getvalue())

  def test_old_published_version_does_not_skip_current_version(self):
    self.workspace = metadata(package("alpha"))
    self.urlopen.side_effect = http_error(404)

    self.assertEqual(publisher.main([]), 0)
    request = self.urlopen.call_args.args[0]
    self.assertEqual(request.full_url, "https://crates.io/api/v1/crates/alpha/0.1.1")
    self.assertEqual(request.get_header("User-agent"), publisher.USER_AGENT)
    self.assertEqual(request.get_header("Cache-control"), "no-cache")
    self.assertEqual(self.urlopen.call_args.kwargs["timeout"], 20)
    self.assertEqual(self.publish_commands()[0][-2:], ["--package", "alpha@0.1.1"])

  def test_yanked_version_is_skipped_and_labelled(self):
    self.workspace = metadata(package("alpha"))
    self.urlopen.return_value = Response(registry_response("alpha", yanked=True))
    self.urlopen.side_effect = None

    self.assertEqual(publisher.main([]), 0)
    self.assert_no_publish()
    self.assertIn("Skip alpha@0.1.1 (published, yanked)", self.stdout.getvalue())

  def test_private_restricted_and_nonmember_packages_are_excluded(self):
    self.workspace = metadata(
      package("private", publish=[]),
      package("restricted", publish=["company"]),
      package("allowed", publish=["company", "crates-io"]),
      package("public"),
    )
    self.workspace["packages"].append(package("dependency"))
    self.urlopen.side_effect = [http_error(404), http_error(404)]

    self.assertEqual(publisher.main([]), 0)
    requests = [call.args[0].full_url for call in self.urlopen.call_args_list]
    self.assertEqual(requests, [
      "https://crates.io/api/v1/crates/allowed/0.1.1",
      "https://crates.io/api/v1/crates/public/0.1.1",
    ])
    self.assertEqual(self.publish_commands()[0][-4:], [
      "--package", "allowed@0.1.1", "--package", "public@0.1.1",
    ])

  def test_every_version_published_is_successful_without_upload(self):
    self.urlopen.side_effect = [Response(registry_response("alpha")), Response(registry_response("beta"))]

    self.assertEqual(publisher.main([]), 0)
    self.assert_no_publish()
    self.assertIn("0 pending; 2 already published", self.stdout.getvalue())

  def test_no_publishable_members_is_successful_without_registry_or_upload(self):
    self.workspace = metadata(package("private", publish=[]))

    self.assertEqual(publisher.main([]), 0)
    self.urlopen.assert_not_called()
    self.assert_no_publish()

  def test_list_checks_registry_without_building_or_uploading(self):
    self.urlopen.side_effect = [http_error(404), http_error(404)]

    self.assertEqual(publisher.main(["--list"]), 0)
    self.assertEqual(self.urlopen.call_count, 2)
    self.assert_no_publish()
    self.assertEqual(len(self.commands), 2)

  def test_dry_run_forwards_flag_to_cargo(self):
    self.urlopen.side_effect = [http_error(404), http_error(404)]

    self.assertEqual(publisher.main(["--dry-run"]), 0)
    self.assertIn("--dry-run", self.publish_commands()[0])
    self.assertNotIn("--workspace", self.publish_commands()[0])
    self.assertNotIn("--allow-dirty", self.publish_commands()[0])
    self.assertNotIn("--no-verify", self.publish_commands()[0])

  def test_configured_cargo_target_directory_is_honored(self):
    self.workspace["target_directory"] = "/custom cargo cache/target"
    self.urlopen.side_effect = [http_error(404), http_error(404)]

    self.assertEqual(publisher.main(["--dry-run"]), 0)
    command = self.publish_commands()[0]
    target_option = command.index("--target-dir")
    self.assertEqual(command[target_option + 1], "/custom cargo cache/target/release-publish")

  def test_malformed_cargo_metadata_is_a_friendly_error_before_registry(self):
    self.workspace = {}

    with self.assertRaises(publisher.ReleaseError):
      publisher.main([])
    self.urlopen.assert_not_called()
    self.assert_no_publish()

  def test_list_and_dry_run_are_mutually_exclusive(self):
    with self.assertRaises(SystemExit) as error:
      publisher.main(["--list", "--dry-run"])
    self.assertEqual(error.exception.code, 2)
    self.run.assert_not_called()
    self.urlopen.assert_not_called()

  def test_registry_error_after_pending_crate_stops_before_publish(self):
    self.urlopen.side_effect = [http_error(404), http_error(403)]

    with self.assertRaises(publisher.ReleaseError):
      publisher.main([])
    self.assert_no_publish()
    self.assertEqual(self.urlopen.call_count, 2)
    self.sleep.assert_not_called()

  def test_malformed_or_mismatched_responses_stop_before_publish(self):
    invalid = [
      b"not JSON",
      b"\xff",
      [],
      {},
      {"version": None},
      registry_response("other"),
      registry_response("beta", version="0.1.0"),
      registry_response("beta", yanked="false"),
      {"version": {"crate": "beta", "num": 1, "yanked": False}},
    ]
    for payload in invalid:
      with self.subTest(payload=payload):
        self.commands.clear()
        self.urlopen.reset_mock()
        self.urlopen.side_effect = [http_error(404), Response(payload)]
        with self.assertRaises(publisher.ReleaseError):
          publisher.main([])
        self.assert_no_publish()
        self.assertEqual(self.urlopen.call_count, 2)
        self.sleep.assert_not_called()

  def test_unexpected_success_status_is_rejected(self):
    self.urlopen.side_effect = [http_error(404), Response({}, status=204)]

    with self.assertRaises(publisher.ReleaseError):
      publisher.main([])
    self.assert_no_publish()
    self.sleep.assert_not_called()

  def test_transient_registry_failure_retries_then_succeeds(self):
    self.workspace = metadata(package("alpha"))
    self.urlopen.side_effect = [http_error(429), URLError("temporary network failure"), http_error(404)]

    self.assertEqual(publisher.main(["--list"]), 0)
    self.assertEqual(self.urlopen.call_count, 3)
    self.assertEqual([call.args for call in self.sleep.call_args_list], [(1,), (2,)])
    self.assert_no_publish()

  def test_registry_retries_are_bounded_and_never_publish_on_failure(self):
    failures = [
      lambda: http_error(503),
      lambda: URLError("offline"),
      lambda: TimeoutError("timed out"),
      lambda: OSError("connection reset"),
    ]
    for failure in failures:
      with self.subTest(failure=failure):
        self.commands.clear()
        self.urlopen.reset_mock()
        self.sleep.reset_mock()
        self.urlopen.side_effect = [http_error(404), failure(), failure(), failure()]
        with self.assertRaises(publisher.ReleaseError):
          publisher.main([])
        self.assert_no_publish()
        self.assertEqual(self.urlopen.call_count, 4)
        self.assertEqual([call.args for call in self.sleep.call_args_list], [(1,), (2,)])

  def test_old_or_unknown_cargo_version_stops_before_registry(self):
    for version in ["cargo 1.98.1 (mock)", "cargo 0.99.0 (mock)", "unrecognized"]:
      with self.subTest(version=version):
        self.commands.clear()
        self.cargo_version = version
        with self.assertRaises(publisher.ReleaseError):
          publisher.main([])
        self.assertEqual(self.commands, [["cargo", "--version"]])
        self.urlopen.assert_not_called()
        self.assert_no_publish()

  def test_cargo_nonzero_status_is_preserved(self):
    self.urlopen.side_effect = [http_error(404), http_error(404)]
    self.publish_status = 101

    self.assertEqual(publisher.main([]), 101)
    self.assertIn("Rerun this script", self.stderr.getvalue())

  def test_cargo_signal_status_is_converted_to_shell_exit_status(self):
    self.urlopen.side_effect = [http_error(404), http_error(404)]
    self.publish_status = -15

    self.assertEqual(publisher.main([]), 143)

  def test_rerun_rechecks_completed_uploads(self):
    self.urlopen.side_effect = [http_error(404), http_error(404)]
    self.publish_status = 101
    self.assertEqual(publisher.main([]), 101)
    self.commands.clear()
    self.publish_status = 0
    self.urlopen.side_effect = [Response(registry_response("alpha")), http_error(404)]

    self.assertEqual(publisher.main([]), 0)
    self.assertEqual(self.publish_commands()[0][-2:], ["--package", "beta@0.1.1"])
    self.assertNotIn("alpha@0.1.1", self.publish_commands()[0])

  def test_build_metadata_does_not_try_to_republish_existing_version(self):
    self.workspace = metadata(package("alpha", version="0.1.1+local"))
    self.urlopen.side_effect = [Response(registry_response("alpha", version="0.1.1+original"))]

    self.assertEqual(publisher.main([]), 0)
    self.assertEqual(self.urlopen.call_args.args[0].full_url, "https://crates.io/api/v1/crates/alpha/0.1.1")
    self.assert_no_publish()


if __name__ == "__main__":
  unittest.main()
