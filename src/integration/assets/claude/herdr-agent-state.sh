#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=11

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|subagent) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import re
import socket
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

# One pane token per running subagent: `subagent-<agent id>`, valued with what
# the subagent is doing. Claude runs subagents inside the lead's process, so
# these hooks are the only sign of them.
SUBAGENT_TOKEN_PREFIX = "subagent-"
SUBAGENT_ID = re.compile(r"[A-Za-z0-9_-]{1,23}")
# The token only outlives its subagent when no hook reports its end: the lead's
# next Stop or a new session in the pane clears it sooner.
SUBAGENT_TOKEN_TTL_MS = 86_400_000
MAX_TOKENS_PER_REPORT = 16

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}


def call(method, params, read_reply=False):
    request = {
        "id": f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}",
        "method": method,
        "params": params,
    }
    reply = b""
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(0.5)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            while not reply.endswith(b"\n"):
                chunk = client.recv(65536)
                if not chunk:
                    break
                reply += chunk
                if not read_reply:
                    break
        except Exception:
            pass
        client.close()
    except Exception:
        return None
    if not read_reply:
        return None
    try:
        return json.loads(reply)
    except Exception:
        return None


def pane_tokens():
    reply = call("pane.get", {"pane_id": pane_id}, read_reply=True)
    if not isinstance(reply, dict):
        return {}
    pane = (reply.get("result") or {}).get("pane") or {}
    tokens = pane.get("tokens")
    return tokens if isinstance(tokens, dict) else {}


def report_tokens(tokens, ttl_ms=None):
    items = list(tokens.items())
    for start in range(0, len(items), MAX_TOKENS_PER_REPORT):
        params = {
            "pane_id": pane_id,
            "source": source,
            "tokens": dict(items[start:start + MAX_TOKENS_PER_REPORT]),
        }
        if ttl_ms:
            params["ttl_ms"] = ttl_ms
        call("pane.report_metadata", params)


def subagent_description(meta_path):
    """Wait briefly for Claude to record what the subagent was asked to do.

    Claude writes the subagent's meta file just after SubagentStart fires, so
    the hook cannot read it in line.
    """
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            with open(meta_path, encoding="utf-8") as handle:
                description = json.load(handle).get("description")
            return description if isinstance(description, str) else None
        except FileNotFoundError:
            time.sleep(0.1)
        except Exception:
            return None
    return None


def subagent_label(description, agent_type):
    for value in (description, agent_type):
        if isinstance(value, str) and value.strip():
            return value
    return "subagent"


def reconcile_subagents(background_tasks):
    """Make the pane's subagent tokens match the lead's running subagents.

    A subagent that is killed rather than finishing gets no SubagentStop, so
    its token would stay. The lead's Stop payload lists every background task
    still running, and no foreground subagent can be running once the lead's
    turn has ended, so at Stop that list is the whole truth.
    """
    if not isinstance(background_tasks, list):
        return
    running = {}
    for task in background_tasks:
        if not isinstance(task, dict) or task.get("type") != "subagent":
            continue
        if task.get("status") != "running":
            continue
        task_id = task.get("id")
        if not isinstance(task_id, str) or not SUBAGENT_ID.fullmatch(task_id):
            continue
        running[SUBAGENT_TOKEN_PREFIX + task_id] = subagent_label(
            task.get("description"), task.get("agent_type")
        )
    current = {
        key: value
        for key, value in pane_tokens().items()
        if key.startswith(SUBAGENT_TOKEN_PREFIX)
    }
    finished = {key: None for key in current if key not in running}
    if finished:
        report_tokens(finished)
    changed = {key: label for key, label in running.items() if current.get(key) != label}
    if changed:
        report_tokens(changed, SUBAGENT_TOKEN_TTL_MS)


if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")

if action == "subagent":
    if hook_event_name == "Stop":
        reconcile_subagents(hook_input.get("background_tasks"))
        raise SystemExit(0)
    if hook_event_name not in ("SubagentStart", "SubagentStop"):
        raise SystemExit(0)
    agent_id = hook_input.get("agent_id")
    if not isinstance(agent_id, str) or not SUBAGENT_ID.fullmatch(agent_id):
        raise SystemExit(0)
    token = SUBAGENT_TOKEN_PREFIX + agent_id
    if hook_event_name == "SubagentStop":
        report_tokens({token: None})
        raise SystemExit(0)
    label = subagent_label(None, hook_input.get("agent_type"))
    report_tokens({token: label}, SUBAGENT_TOKEN_TTL_MS)
    transcript_path = hook_input.get("transcript_path")
    if not isinstance(transcript_path, str) or not transcript_path.endswith(".jsonl"):
        raise SystemExit(0)
    meta_path = os.path.join(
        transcript_path[: -len(".jsonl")], "subagents", f"agent-{agent_id}.meta.json"
    )
    # Claude waits on this hook, so the wait for the description happens in a
    # detached child and the subagent starts now.
    try:
        if os.fork() != 0:
            raise SystemExit(0)
    except OSError:
        raise SystemExit(0)
    try:
        os.setsid()
        devnull = os.open(os.devnull, os.O_RDWR)
        for fd in (0, 1, 2):
            os.dup2(devnull, fd)
        description = subagent_description(meta_path)
        # A subagent that already finished has had its token cleared; do not
        # bring it back.
        if description and token in pane_tokens():
            report_tokens({token: description}, SUBAGENT_TOKEN_TTL_MS)
    finally:
        os._exit(0)

if hook_event_name != "SessionStart":
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None
session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if not agent_session_id:
    raise SystemExit(0)
params = {
    "pane_id": pane_id,
    "source": source,
    "agent": "claude",
    "seq": report_seq,
    "agent_session_id": agent_session_id,
}
if agent_session_path:
    params["agent_session_path"] = agent_session_path
if session_start_source:
    params["session_start_source"] = session_start_source
call("pane.report_agent_session", params)

# A new Claude process starts with no subagents. Rows left by one that exited
# without its stop hooks go now; /clear and compaction keep the same process,
# and its subagents with it.
if session_start_source in ("startup", "resume"):
    stale = {
        key: None for key in pane_tokens() if key.startswith(SUBAGENT_TOKEN_PREFIX)
    }
    if stale:
        report_tokens(stale)
PY
