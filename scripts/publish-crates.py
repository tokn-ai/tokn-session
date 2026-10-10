#!/usr/bin/env python3
"""Publish missing workspace versions to crates.io, safely resuming partial releases."""

import argparse
import json
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass
from http.client import HTTPException
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


MANIFEST = Path(__file__).resolve().parent.parent / "Cargo.toml"
USER_AGENT = "tokn-session-release (https://github.com/tokn-ai/tokn-session)"


class ReleaseError(Exception):
  pass


@dataclass(frozen=True)
class Crate:
  name: str
  version: str

  @property
  def spec(self):
    return f"{self.name}@{self.version}"


def cargo_output(*arguments):
  result = subprocess.run(
    ["cargo", *arguments], cwd=MANIFEST.parent, text=True,
    stdout=subprocess.PIPE, check=True,
  )
  return result.stdout


def workspace_crates(metadata):
  members = set(metadata["workspace_members"])
  crates = []
  for package in metadata["packages"]:
    if package["id"] not in members:
      continue
    registries = package["publish"]
    if registries is not None and "crates-io" not in registries:
      continue
    if not all(isinstance(package[field], str) for field in ("name", "version")):
      raise ReleaseError("Invalid Cargo package metadata; publication stopped")
    crates.append(Crate(package["name"], package["version"]))
  return sorted(crates, key=lambda crate: crate.name)


def published_version(crate):
  # Registry version uniqueness ignores SemVer build metadata.
  version = crate.version.split("+", 1)[0]
  url = f"https://crates.io/api/v1/crates/{quote(crate.name, safe='')}/{quote(version, safe='')}"
  request = Request(url, headers={
    "User-Agent": USER_AGENT,
    "Accept": "application/json",
    "Cache-Control": "no-cache",
  })
  for attempt in range(3):
    try:
      with urlopen(request, timeout=20) as response:
        if response.status != 200:
          raise ReleaseError(f"Unexpected HTTP {response.status} checking {crate.spec}")
        payload = json.load(response)
      published = payload.get("version") if isinstance(payload, dict) else None
      if (
        not isinstance(published, dict)
        or published.get("crate") != crate.name
        or not isinstance(published.get("num"), str)
        or published["num"].split("+", 1)[0] != version
        or not isinstance(published.get("yanked"), bool)
      ):
        raise ReleaseError(f"Unexpected crates.io response for {crate.spec}")
      return published
    except HTTPError as error:
      error.close()
      if error.code == 404:
        return None
      if error.code not in (429, 500, 502, 503, 504) or attempt == 2:
        raise ReleaseError(f"HTTP {error.code} checking {crate.spec}; publication stopped") from error
    except (URLError, TimeoutError, OSError, HTTPException) as error:
      if attempt == 2:
        raise ReleaseError(f"Cannot check {crate.spec}; publication stopped: {error}") from error
    except (ValueError, UnicodeError) as error:
      raise ReleaseError(f"Invalid crates.io JSON for {crate.spec}; publication stopped") from error
    print(f"Retrying registry check for {crate.spec}...", file=sys.stderr, flush=True)
    time.sleep(2 ** attempt)


def publish_command(crates, dry_run, target_directory):
  command = [
    "cargo", "publish", "--registry", "crates-io", "--locked",
    "--manifest-path", str(MANIFEST),
    "--target-dir", str(target_directory),
  ]
  if dry_run:
    command.append("--dry-run")
  for crate in crates:
    command.extend(["--package", crate.spec])
  return command


def main(argv=None):
  parser = argparse.ArgumentParser(description=__doc__)
  mode = parser.add_mutually_exclusive_group()
  mode.add_argument("--list", action="store_true", help="check and list versions without building or uploading")
  mode.add_argument("--dry-run", action="store_true", help="verify remaining crates without uploading")
  arguments = parser.parse_args(argv)

  cargo_version = cargo_output("--version")
  match = re.match(r"cargo (\d+)\.(\d+)\.", cargo_version)
  if match is None or tuple(map(int, match.groups())) < (1, 99):
    raise ReleaseError("Cargo 1.99 or newer is required for workspace publication")
  try:
    metadata = json.loads(cargo_output(
      "metadata", "--no-deps", "--format-version", "1", "--locked",
      "--manifest-path", str(MANIFEST),
    ))
    crates = workspace_crates(metadata)
    target_directory = Path(metadata["target_directory"]) / "release-publish"
  except (KeyError, TypeError, ValueError) as error:
    raise ReleaseError("Invalid Cargo metadata; publication stopped") from error
  pending = []
  for crate in crates:
    published = published_version(crate)
    if published is None:
      pending.append(crate)
      print(f"Pending {crate.spec}", flush=True)
    else:
      status = "published, yanked" if published["yanked"] else "published"
      print(f"Skip {crate.spec} ({status})", flush=True)

  print(f"{len(pending)} pending; {len(crates) - len(pending)} already published", flush=True)
  if not pending or arguments.list:
    return 0

  command = publish_command(pending, arguments.dry_run, target_directory)
  print(shlex.join(command), flush=True)
  result = subprocess.run(command, cwd=MANIFEST.parent)
  if result.returncode != 0:
    print("Cargo stopped. Rerun this script to recheck and skip any completed uploads.", file=sys.stderr)
  return result.returncode if result.returncode >= 0 else 128 - result.returncode


if __name__ == "__main__":
  try:
    sys.exit(main())
  except (ReleaseError, subprocess.CalledProcessError, OSError, ValueError) as error:
    print(f"error: {error}", file=sys.stderr)
    sys.exit(1)
  except KeyboardInterrupt:
    print("Interrupted. Rerun to recheck and skip any completed uploads.", file=sys.stderr)
    sys.exit(130)
