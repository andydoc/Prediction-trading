"""
Execution Control Server — Single-leader lock for multi-machine trading.

Only one machine may run L4 execution at a time. This Flask API serves as
the authority. L4 checks /status before every trade cycle.

Port: 5557 (dashboard is 5556)
State: persisted to data/system_state/execution_lock.json

Endpoints:
  GET  /status     — Who holds the lock, TTL remaining
  POST /claim      — Claim execution {machine, ttl_seconds}
  POST /release    — Release lock (leader only, or force=true)
  POST /heartbeat  — Extend TTL (leader only)
  GET  /health     — Quick health check
"""
import json
import os
import time
import socket
import logging
from datetime import datetime, timezone
from flask import Flask, request, jsonify

app = Flask(__name__)

LOCK_FILE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "data", "system_state", "execution_lock.json"
)
DEFAULT_TTL = 300  # 5 minutes — leader must heartbeat within this
MAX_TTL = 3600     # 1 hour max

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [EXEC-CTRL] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S"
)
log = logging.getLogger("exec_ctrl")


def _load_lock():
    """Load lock state from disk. Returns empty dict if no lock."""
    try:
        if os.path.exists(LOCK_FILE):
            with open(LOCK_FILE, "r") as f:
                return json.load(f)
    except (json.JSONDecodeError, IOError):
        pass
    return {}


def _save_lock(lock):
    """Persist lock state to disk."""
    os.makedirs(os.path.dirname(LOCK_FILE), exist_ok=True)
    with open(LOCK_FILE, "w") as f:
        json.dump(lock, f, indent=2)


def _is_expired(lock):
    """Check if current lock has expired."""
    if not lock or "expires_at" not in lock:
        return True
    return time.time() > lock["expires_at"]


def _machine_id():
    """Get this machine's hostname for identification."""
    return socket.gethostname()


@app.route("/status", methods=["GET"])
def status():
    lock = _load_lock()
    if _is_expired(lock):
        return jsonify({
            "locked": False,
            "leader": None,
            "message": "No active leader — execution available"
        })

    remaining = lock["expires_at"] - time.time()
    return jsonify({
        "locked": True,
        "leader": lock.get("machine"),
        "claimed_at": lock.get("claimed_at"),
        "expires_at": lock.get("expires_at"),
        "ttl_remaining": round(remaining, 1),
        "message": f"Leader: {lock.get('machine')} ({round(remaining)}s remaining)"
    })


@app.route("/claim", methods=["POST"])
def claim():
    data = request.get_json(force=True) if request.data else {}
    machine = data.get("machine", _machine_id())
    ttl = min(data.get("ttl_seconds", DEFAULT_TTL), MAX_TTL)

    lock = _load_lock()

    # If locked by someone else and not expired, reject
    if not _is_expired(lock) and lock.get("machine") != machine:
        remaining = lock["expires_at"] - time.time()
        return jsonify({
            "success": False,
            "message": f"Locked by {lock['machine']} ({round(remaining)}s remaining). Use /release with force=true to override."
        }), 409

    # Claim or re-claim
    now = time.time()
    new_lock = {
        "machine": machine,
        "claimed_at": datetime.now(timezone.utc).isoformat(),
        "claimed_ts": now,
        "expires_at": now + ttl,
        "ttl_seconds": ttl
    }
    _save_lock(new_lock)
    log.info(f"Execution claimed by '{machine}' (TTL={ttl}s)")

    return jsonify({
        "success": True,
        "message": f"Execution claimed by '{machine}' for {ttl}s",
        "lock": new_lock
    })


@app.route("/release", methods=["POST"])
def release():
    data = request.get_json(force=True) if request.data else {}
    machine = data.get("machine", _machine_id())
    force = data.get("force", False)

    lock = _load_lock()

    if _is_expired(lock):
        _save_lock({})
        return jsonify({"success": True, "message": "Lock was already expired"})

    if lock.get("machine") != machine and not force:
        return jsonify({
            "success": False,
            "message": f"Lock held by '{lock['machine']}', not '{machine}'. Use force=true to override."
        }), 403

    old_leader = lock.get("machine")
    _save_lock({})
    log.info(f"Execution released (was '{old_leader}', released by '{machine}', force={force})")

    return jsonify({
        "success": True,
        "message": f"Lock released (was '{old_leader}')"
    })


@app.route("/heartbeat", methods=["POST"])
def heartbeat():
    data = request.get_json(force=True) if request.data else {}
    machine = data.get("machine", _machine_id())
    ttl = min(data.get("ttl_seconds", DEFAULT_TTL), MAX_TTL)

    lock = _load_lock()

    # If no lock or expired, auto-claim
    if _is_expired(lock):
        return claim()

    # Only leader can heartbeat
    if lock.get("machine") != machine:
        return jsonify({
            "success": False,
            "message": f"Not the leader. Leader is '{lock['machine']}'"
        }), 403

    # Extend TTL
    lock["expires_at"] = time.time() + ttl
    lock["ttl_seconds"] = ttl
    lock["last_heartbeat"] = datetime.now(timezone.utc).isoformat()
    _save_lock(lock)

    return jsonify({
        "success": True,
        "message": f"Heartbeat OK — TTL extended to {ttl}s",
        "expires_at": lock["expires_at"]
    })


@app.route("/health", methods=["GET"])
def health():
    lock = _load_lock()
    return jsonify({
        "service": "execution-control",
        "hostname": _machine_id(),
        "lock_file": LOCK_FILE,
        "lock_exists": bool(lock),
        "lock_expired": _is_expired(lock),
        "leader": lock.get("machine") if not _is_expired(lock) else None
    })


if __name__ == "__main__":
    log.info(f"Execution Control Server starting on port 5557")
    log.info(f"Lock file: {LOCK_FILE}")
    log.info(f"Machine ID: {_machine_id()}")
    app.run(host="0.0.0.0", port=5557, debug=False)
