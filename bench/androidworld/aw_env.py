"""Connecting to an AndroidWorld emulator, shared by the bench scripts.

AndroidWorld's own `run.py` hard-codes gRPC port 8554 and offers only a
console-port flag. This machine may run another emulator that already holds
8554, so the bench scripts pass both ports through to
`env_launcher.load_and_setup_env` (which takes them).
"""

from __future__ import annotations

import dataclasses
import os
import shutil
import subprocess

# As AndroidWorld's run.py does, before gRPC is imported: gRPC's INFO lines
# ("FD from fork parent still in poll list") are written by each forked adb
# child before exec and land in the adb output android_env captures, which
# breaks its checks (an empty `ls` reads as non-empty, so restoring an app
# snapshot fails at random).
os.environ['GRPC_VERBOSITY'] = 'ERROR'
os.environ['GRPC_TRACE'] = 'none'
# android_env forks an adb child for every device call while gRPC threads
# are live. gRPC's fork handlers can deadlock such a child before it execs
# (seen here: a forked copy of the runner stuck for good, its parent waiting
# on it). The children exec adb at once and never use gRPC, so the handlers
# are not needed.
os.environ['GRPC_ENABLE_FORK_SUPPORT'] = '0'


def default_adb() -> str:
  """The adb AndroidWorld would use (the SDK's), else the one on PATH."""
  for p in (
      os.path.expanduser('~/Library/Android/sdk/platform-tools/adb'),
      os.path.expanduser('~/Android/Sdk/platform-tools/adb'),
  ):
    if os.path.isfile(p):
      return p
  return shutil.which('adb') or 'adb'


@dataclasses.dataclass
class Device:
  console_port: int = 5556
  grpc_port: int = 8556
  adb_path: str = dataclasses.field(default_factory=default_adb)

  @property
  def serial(self) -> str:
    return f'emulator-{self.console_port}'

  def adb(self, *args: str, timeout: float = 30) -> str:
    out = subprocess.run(
        [self.adb_path, '-s', self.serial, *args],
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    return out.stdout.strip()


def keep_adb_server() -> None:
  """Stop android_env from restarting the shared adb server.

  When an adb call fails, android_env's AdbController runs `adb kill-server`
  and `start-server` before retrying. On a machine where other emulators or
  tools share the adb server (port 5037), that drops their connections. The
  retries themselves still happen; only the server restart is skipped. Set
  AW_ALLOW_ADB_RESTART=1 to keep upstream behaviour.
  """
  if os.environ.get('AW_ALLOW_ADB_RESTART') == '1':
    return
  from android_env.components import adb_controller  # pylint: disable=g-import-not-at-top

  def _no_restart(self, timeout=None):  # pylint: disable=unused-argument
    return None

  adb_controller.AdbController._restart_server = _no_restart  # pylint: disable=protected-access


def load_env(device: Device, emulator_setup: bool = False):
  """AndroidWorld's env on `device` (imports android_world lazily)."""
  # pylint: disable=g-import-not-at-top
  from android_world.env import env_launcher

  keep_adb_server()

  return env_launcher.load_and_setup_env(
      console_port=device.console_port,
      emulator_setup=emulator_setup,
      adb_path=device.adb_path,
      grpc_port=device.grpc_port,
  )
