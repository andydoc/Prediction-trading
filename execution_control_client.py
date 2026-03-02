"""
Execution Control Client — imported by L4 to check/claim/heartbeat the lock.

Usage in layer4_runner.py:
    from execution_control_client import ExecutionLock
    lock = ExecutionLock("http://desktop-ip:5557", machine="laptop")

    if lock.can_execute():
        # proceed with trade
    else:
        log.info(f"Execution blocked: {lock.last_status}")
"""
import json
import socket
import logging
import requests
from typing import Optional

log = logging.getLogger("exec_lock")


class ExecutionLock:
    def __init__(self, server_url: str = "http://localhost:5557",
                 machine: str = None, ttl: int = 300, timeout: float = 3.0):
        self.server_url = server_url.rstrip("/")
        self.machine = machine or socket.gethostname()
        self.ttl = ttl
        self.timeout = timeout  # HTTP timeout in seconds
        self.last_status = None
        self._enabled = True

    def _get(self, path: str) -> Optional[dict]:
        try:
            r = requests.get(f"{self.server_url}{path}", timeout=self.timeout)
            return r.json()
        except Exception as e:
            log.warning(f"Exec control unreachable ({path}): {e}")
            return None

    def _post(self, path: str, data: dict = None) -> Optional[dict]:
        try:
            r = requests.post(f"{self.server_url}{path}",
                              json=data or {}, timeout=self.timeout)
            return r.json()
        except Exception as e:
            log.warning(f"Exec control unreachable ({path}): {e}")
            return None

    def status(self) -> Optional[dict]:
        """Get current lock status."""
        self.last_status = self._get("/status")
        return self.last_status

    def can_execute(self) -> bool:
        """Check if this machine is allowed to execute trades.

        Returns True if:
        - This machine holds the lock, OR
        - No lock exists (unclaimed), OR
        - Control server unreachable (fail-open to avoid blocking)
        """
        if not self._enabled:
            return True

        st = self.status()
        if st is None:
            # Server unreachable — fail-open so trades aren't blocked
            # if control server is down
            log.warning("Exec control unreachable — fail-open, allowing execution")
            return True

        if not st.get("locked"):
            return True  # No leader — anyone can execute

        return st.get("leader") == self.machine

    def claim(self, ttl: int = None) -> bool:
        """Claim execution control for this machine."""
        resp = self._post("/claim", {
            "machine": self.machine,
            "ttl_seconds": ttl or self.ttl
        })
        if resp and resp.get("success"):
            log.info(f"Claimed execution lock (TTL={ttl or self.ttl}s)")
            return True
        log.warning(f"Failed to claim: {resp}")
        return False

    def release(self, force: bool = False) -> bool:
        """Release execution control."""
        resp = self._post("/release", {
            "machine": self.machine,
            "force": force
        })
        if resp and resp.get("success"):
            log.info("Released execution lock")
            return True
        return False

    def heartbeat(self, ttl: int = None) -> bool:
        """Extend the lock TTL. Call this periodically from L4."""
        resp = self._post("/heartbeat", {
            "machine": self.machine,
            "ttl_seconds": ttl or self.ttl
        })
        if resp and resp.get("success"):
            return True
        log.warning(f"Heartbeat failed: {resp}")
        return False

    def disable(self):
        """Disable lock checking (single-machine mode)."""
        self._enabled = False

    def enable(self):
        """Enable lock checking."""
        self._enabled = True
