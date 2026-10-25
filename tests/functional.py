#!/usr/bin/env python3
"""End-to-end test of the rtk-mcp binary over real MCP stdio.

usage: python3 tests/functional.py [path/to/rtk-mcp]     (default: target/release/rtk-mcp)

Uses whatever `rtk` is on PATH (tests that need RTK are skipped when there is none, and the
no-RTK fallback is exercised separately). Every server runs in a throwaway HOME, so nothing
here reads or writes your real RTK data or Claude settings. Exit status 1 on any failure.
"""
import json
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time

BINARY = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/rtk-mcp")
FAILURES = []


def check(name, ok, detail=""):
    print(("PASS" if ok else "FAIL") + ": " + name)
    if not ok:
        print("      " + str(detail)[:600])
        FAILURES.append(name)


def skip(name, why):
    print("SKIP: " + name + " (" + why + ")")


class Server:
    """One rtk-mcp process in an isolated HOME, speaking line-delimited JSON-RPC."""

    def __init__(self, path=None, deny=(), extra_env=None):
        self.root = tempfile.mkdtemp(prefix="rtk-mcp-func-")
        self.home = os.path.join(self.root, "home")
        self.project = os.path.join(self.root, "project")
        os.makedirs(os.path.join(self.home, ".claude", "projects"))
        os.makedirs(self.project)
        with open(os.path.join(self.home, ".claude", "settings.json"), "w") as f:
            json.dump({"permissions": {"deny": list(deny)}}, f)
        self.env = {
            "HOME": self.home,
            "XDG_CONFIG_HOME": os.path.join(self.home, ".config"),
            "XDG_DATA_HOME": os.path.join(self.home, ".local", "share"),
            "PATH": path or os.environ.get("PATH", "/usr/bin:/bin"),
            "LANG": "C.UTF-8",
        }
        self.env.update(extra_env or {})
        self.stderr = open(os.path.join(self.root, "stderr.log"), "w+")
        self.proc = subprocess.Popen(
            [BINARY], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            env=self.env, cwd=self.project, text=True, bufsize=1,
        )
        self.next_id = 1
        init = self.request("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "functional", "version": "0"},
        })
        self.init = init["result"]
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params=None, timeout=60):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready, _, _ = select.select([self.proc.stdout], [], [], 0.2)
            if not ready:
                if self.proc.poll() is not None:
                    raise RuntimeError("server exited with %s" % self.proc.returncode)
                continue
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout")
            message = json.loads(line)
            if message.get("id") == rid:
                return message
        raise TimeoutError("no response to %s" % method)

    def call(self, name, **arguments):
        """tools/call -> (is_error, text); a protocol-level error comes back as (True, error)."""
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        if "error" in response:
            return True, "protocol error: " + json.dumps(response["error"])
        result = response["result"]
        text = "".join(c.get("text", "") for c in result.get("content", []))
        return bool(result.get("isError")), text

    def alive(self):
        return self.proc.poll() is None

    def log(self):
        self.stderr.flush()
        self.stderr.seek(0)
        return self.stderr.read()

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
        self.stderr.close()
        shutil.rmtree(self.root, ignore_errors=True)


def rtk_version(path=None):
    env = dict(os.environ, PATH=path or os.environ.get("PATH", ""))
    try:
        out = subprocess.run(["rtk", "--version"], env=env, capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    numbers = re.findall(r"\d+", out)
    return tuple(int(n) for n in numbers[:3]) if len(numbers) >= 3 else None


def process_alive(pid):
    try:
        with open("/proc/%d/stat" % pid) as f:
            return not f.read().rsplit(")", 1)[-1].lstrip().startswith("Z")  # zombie == dead
    except FileNotFoundError:
        pass
    except OSError:
        return True
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_dead(pid, seconds=8):
    deadline = time.time() + seconds
    while time.time() < deadline:
        if not process_alive(pid):
            return True
        time.sleep(0.1)
    return False


def test_with_rtk(version):
    marker = None
    s = Server(deny=["Bash(touch:*)", "Bash(git push:*)"])
    try:
        # --- handshake and tool surface -------------------------------------------------------
        check("initialize reports rtk-mcp", s.init["serverInfo"]["name"] == "rtk-mcp", s.init)
        check("initialize carries instructions", "run_command" in s.init.get("instructions", ""))
        tools = {t["name"]: t for t in s.request("tools/list")["result"]["tools"]}
        check("tools/list has the 4 tools", set(tools) == {
            "run_command", "get_rtk_savings", "get_rtk_discover", "get_rtk_session"}, sorted(tools))
        props = tools["run_command"]["inputSchema"]["properties"]
        check("run_command takes command + cwd", "command" in props and "cwd" in props, props)
        desc = tools["run_command"]["description"]
        check("run_command documents the configurable limits",
              "RTK_MCP_TIMEOUT_SECS (default 300s)" in desc and "RTK_MCP_MAX_OUTPUT_KB" in desc, desc[-400:])
        check("run_command is flagged destructive/open-world",
              tools["run_command"]["annotations"].get("destructiveHint") is True
              and tools["run_command"]["annotations"].get("openWorldHint") is True,
              tools["run_command"].get("annotations"))
        check("stats tools are flagged read-only", all(
            tools[n]["annotations"].get("readOnlyHint") is True
            for n in ("get_rtk_savings", "get_rtk_discover", "get_rtk_session")))
        check("startup log names the detected RTK", "detected" in s.log() and "rtk " in s.log(), s.log()[-300:])

        # --- running commands ---------------------------------------------------------------
        err, text = s.call("run_command", command="echo hello from functional")
        check("echo runs", not err and "hello from functional" in text, text)
        for name in ("alpha.txt", "beta.txt"):
            open(os.path.join(s.project, name), "w").write(name)
        err, text = s.call("run_command", command="ls", cwd=s.project)
        check("ls (through RTK) lists files, honours cwd", not err and "alpha.txt" in text and "beta.txt" in text, text)
        err, text = s.call("run_command", command="pwd", cwd=s.project)
        check("cwd is applied", not err and os.path.basename(s.project) in text, text)
        err, text = s.call("run_command", command="echo $HOME | tr a-z A-Z && true")
        check("shell operators work", not err and s.home.upper() in text, text)
        err, text = s.call("run_command", command="exit 3")
        check("non-zero exit is an error with the code", err and "[exit code: 3]" in text, text)
        err, text = s.call("run_command", command="")
        check("empty command is rejected", err and "empty" in text.lower(), text)

        subprocess.run(["git", "init", "-q", "."], cwd=s.project, env=s.env)
        err, text = s.call("run_command", command="git status", cwd=s.project)
        check("git status (RTK-filtered) works", not err and text.strip() != "", text)
        err, text = s.call("run_command", command="RTK_DISABLED=1 git status", cwd=s.project)
        check("RTK_DISABLED=1 bypass works (raw git output)",
              not err and ("On branch" in text or "No commits" in text), text)

        # --- hook-warning nag must not reach the LLM ------------------------------------------
        err, text = s.call("run_command", command="rtk git --version")
        check("no 'No hook installed' nag in output", "No hook installed" not in text, text)
        err, text = s.call("run_command", command="echo [$RTK_SUPPRESS_HOOK_WARNING]")
        check("children get RTK_SUPPRESS_HOOK_WARNING=1", "[1]" in text, text)

        # --- option-like input must not be taken for `rtk rewrite --help` ----------------------
        err, text = s.call("run_command", command="--help")
        check("option-like command is not executed as rewrite help", "rewrite" not in text.lower(), text)

        # --- deny rules in the user's Claude settings ------------------------------------------
        marker = os.path.join(s.project, "denied-marker")
        err, text = s.call("run_command", command="touch " + marker)
        check("denied command is refused", err and text.startswith("Denied"), text)
        check("denied command did not run", not os.path.exists(marker))
        err, text = s.call("run_command", command="git push origin main", cwd=s.project)
        check("denied git push is refused", err and text.startswith("Denied"), text)
        err, text = s.call("run_command", command="echo still fine")
        check("allowed command still works after a denial", not err and "still fine" in text, text)

        # --- stats tools ---------------------------------------------------------------------
        err, text = s.call("get_rtk_savings", format="json")
        try:
            summary = json.loads(text)["summary"]
        except (ValueError, KeyError):
            summary = None
        check("savings json parses and counts RTK-routed commands",
              not err and summary is not None and summary["total_commands"] >= 1, text)
        err, text = s.call("get_rtk_savings", view="all", format="json")
        check("savings view=all has daily/weekly/monthly",
              not err and all(k in json.loads(text) for k in ("daily", "weekly", "monthly")), text)
        err, text = s.call("get_rtk_savings")
        check("savings default text works", not err and text.strip() != "", text)
        err, text = s.call("get_rtk_savings", graph=True, view="history")
        check("savings graph + history accepted", not err, text)
        err, text = s.call("get_rtk_savings", view="quota", tier="pro")
        check("savings view=quota tier=pro accepted", not err, text)
        err, text = s.call("get_rtk_savings", view="failures")
        check("savings view=failures accepted", not err, text)
        if version >= (0, 49, 0):
            err, text = s.call("get_rtk_savings", view="recalls")
            check("savings view=recalls works on RTK >= 0.49", not err and "Recall" in text, text)
        else:
            err, text = s.call("get_rtk_savings", view="recalls")
            check("savings view=recalls on old RTK gives a clear upgrade hint",
                  err and "predates" in text and "rtk %d.%d.%d" % version in text, text)
        err, text = s.call("get_rtk_savings", view="reset")
        check("there is no way to reach --reset", err and "unknown variant" in text, text)
        err, text = s.call("get_rtk_discover", all=True, format="json")
        check("discover all=true format=json works", not err and text.strip() != "", text)
        err, text = s.call("get_rtk_discover", all=True, since=7, limit=3)
        check("discover since/limit accepted", not err, text)
        err, text = s.call("get_rtk_discover", format="csv")
        check("discover rejects csv", err and "unknown variant" in text, text)
        err, text = s.call("get_rtk_session")
        check("session answers", text.strip() != "", text)

        check("server still alive and quiet", s.alive())
    finally:
        s.close()


def test_runtime_robustness():
    s = Server(extra_env={"RTK_MCP_TIMEOUT_SECS": "2", "RTK_MCP_MAX_OUTPUT_KB": "64"})
    try:
        err, text = s.call("run_command", command="echo partial-result; sleep 60")
        check("a timeout still returns the output printed so far",
              err and "timed out after 2s" in text and "partial-result" in text, text)

        pidfile = os.path.join(s.project, "grandchild.pid")
        started = time.time()
        err, text = s.call("run_command", command="sleep 300 & echo $! > %s; wait" % pidfile)
        check("timeout is reported", err and "timed out after 2s" in text, text)
        check("timeout returns promptly", time.time() - started < 15, time.time() - started)
        try:
            pid = int(open(pidfile).read().strip())
            check("timeout killed the grandchild too (whole process tree)", wait_dead(pid), "pid %d alive" % pid)
        except (OSError, ValueError) as e:
            check("grandchild pid recorded", False, e)

        err, text = s.call("run_command", command="yes 日本語日本語 | head -c 200000")
        check("multibyte output over the cap (64KB here) is truncated, not fatal",
              not err and "[output truncated at 64KB" in text and s.alive(), text[-120:])
        check("truncated output is valid UTF-8 text", text.encode("utf-8").decode("utf-8") == text)
        err, text = s.call("run_command", command="echo alive")
        check("server alive after truncation", not err and "alive" in text, text)

        err, text = s.call("run_command", command="head -c 30000000 /dev/zero | tr '\\0' x")
        check("30MB of output is handled (capped, not buffered)", not err and "[output truncated" in text, text[-120:])
        err, text = s.call("run_command", command="cat")
        check("stdin of children is isolated", not err and text == "(no output)", text)
    finally:
        s.close()


def test_client_cancel():
    """notifications/cancelled from the client kills the command's whole process tree."""
    s = Server()
    try:
        pidfile = os.path.join(s.project, "cancel.pid")
        rid = s.next_id
        s.next_id += 1
        s._send({"jsonrpc": "2.0", "id": rid, "method": "tools/call", "params": {
            "name": "run_command",
            "arguments": {"command": "sleep 300 & echo $! > %s; wait" % pidfile}}})
        deadline = time.time() + 15
        while time.time() < deadline and not (os.path.exists(pidfile) and open(pidfile).read().strip()):
            time.sleep(0.1)
        try:
            pid = int(open(pidfile).read().strip())
        except (OSError, ValueError) as e:
            check("the long command started", False, e)
            return
        check("the long command is running", process_alive(pid))
        s._send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                 "params": {"requestId": rid, "reason": "functional test"}})
        check("a client cancel kills the grandchild too (whole process tree)", wait_dead(pid), "pid %d alive" % pid)
        err, text = s.call("run_command", command="echo still serving")
        check("server healthy after a cancel", not err and "still serving" in text and s.alive(), text)
    finally:
        s.close()


def test_output_limits():
    """RTK_MCP_MAX_OUTPUT_KB: 256KB by default, and settable."""
    s = Server()
    try:
        err, text = s.call("run_command", command="head -c 200000 /dev/zero | tr '\\0' y")
        check("default cap: 200KB comes back whole", not err and text == "y" * 200000, text[-120:])
        err, text = s.call("run_command", command="head -c 400000 /dev/zero | tr '\\0' y")
        check("default cap: 400KB is cut at 256KB",
              not err and text.startswith("y" * 262144) and "[output truncated at 256KB" in text
              and len(text) < 262144 + 120, text[-120:])
    finally:
        s.close()

    s = Server(extra_env={"RTK_MCP_MAX_OUTPUT_KB": "1"})
    try:
        err, text = s.call("run_command", command="head -c 5000 /dev/zero | tr '\\0' z")
        check("RTK_MCP_MAX_OUTPUT_KB=1 cuts at 1KB",
              not err and text.startswith("z" * 1024) and "[output truncated at 1KB" in text
              and len(text) < 1024 + 120, text[-120:])
    finally:
        s.close()

    s = Server(extra_env={"RTK_MCP_MAX_OUTPUT_KB": "0", "RTK_MCP_TIMEOUT_SECS": "junk"})
    try:
        err, text = s.call("run_command", command="head -c 300000 /dev/zero | tr '\\0' y")
        check("junk limits fall back to the defaults (0KB is not a cap)",
              not err and "[output truncated at 256KB" in text, text[-120:])
    finally:
        s.close()


def test_cwd_jail(has_rtk):
    """RTK_MCP_CWD_JAIL confines the cwd parameter."""
    jail = os.path.realpath(tempfile.mkdtemp(prefix="rtk-mcp-jail-"))
    inside = os.path.join(jail, "inside")
    os.makedirs(inside)
    outside = os.path.dirname(jail)
    marker = os.path.join(outside, "rtk-mcp-jail-marker-%d" % os.getpid())
    s = Server(extra_env={"RTK_MCP_CWD_JAIL": jail})
    try:
        err, text = s.call("run_command", command="pwd", cwd=inside)
        check("cwd inside the jail runs there", not err and text.strip() == inside, text)
        err, text = s.call("run_command", command="pwd", cwd=jail)
        check("the jail root itself is allowed", not err and text.strip() == jail, text)
        err, text = s.call("run_command", command="pwd", cwd=outside)
        check("cwd outside the jail is refused", err and "outside allowed root" in text, text)
        err, text = s.call("run_command", command="pwd", cwd=os.path.join(inside, "..", ".."))
        check(".. cannot be used to climb out", err and "outside allowed root" in text, text)
        err, text = s.call("run_command", command="touch " + marker, cwd=outside)
        check("a refused cwd never runs the command", err and not os.path.exists(marker), text)
        err, text = s.call("run_command", command="pwd", cwd=os.path.join(jail, "missing"))
        check("a missing cwd is reported", err and "invalid cwd" in text, text)
        err, text = s.call("run_command", command="echo no-cwd-given")
        check("without a cwd the command runs in the server's own directory", not err and "no-cwd-given" in text, text)
        if has_rtk:
            err, text = s.call("get_rtk_savings", cwd=outside, project=True)
            check("the stats tool's cwd is jailed too", err and "outside allowed root" in text, text)
        check("server alive after refusals", s.alive())
    finally:
        s.close()
        shutil.rmtree(jail, ignore_errors=True)
        if os.path.exists(marker):
            os.remove(marker)


def test_background_jobs():
    """A backgrounded job must survive the call without holding it hostage."""
    s = Server()
    pids = []
    try:
        started = time.time()
        err, text = s.call("run_command", command="sleep 300 & echo $!")
        took = time.time() - started
        check("a job holding the output pipes does not hold the call", not err and took < 15, "%.1fs: %s" % (took, text))
        check("the reply says the job is detached", "background process" in text, text)
        pid = int(text.split()[0])
        pids.append(pid)
        time.sleep(0.5)
        check("the job is still running", process_alive(pid))

        started = time.time()
        err, text = s.call("run_command", command="nohup sleep 300 >/dev/null 2>&1 & echo $!")
        took = time.time() - started
        check("a job with redirected output returns at once, with no note",
              not err and took < 5 and "background process" not in text, "%.1fs: %s" % (took, text))
        pid = int(text.split()[0])
        pids.append(pid)
        check("that job is running too", process_alive(pid))

        marker = os.path.join(s.project, "late-marker")
        err, text = s.call("run_command", command="( sleep 2; head -c 300000 /dev/zero; touch %s ) & echo started" % marker)
        check("a job that prints later returns at once", not err and text.startswith("started"), text)
        deadline = time.time() + 20
        while time.time() < deadline and not os.path.exists(marker):
            time.sleep(0.2)
        check("its late output neither blocks nor kills it (no full pipe, no SIGPIPE)", os.path.exists(marker))
        err, text = s.call("run_command", command="echo healthy")
        check("server healthy afterwards", not err and "healthy" in text and s.alive(), text)
    finally:
        for pid in pids:
            try:
                os.kill(pid, 9)
            except OSError:
                pass
        s.close()


def test_without_rtk():
    # A PATH that cannot contain rtk: only the system dirs.
    path = "/usr/bin:/bin"
    if rtk_version(path) is not None:
        skip("no-RTK fallback", "rtk is installed in /usr/bin or /bin")
        return
    s = Server(path=path)
    try:
        check("startup log warns RTK is missing", "RTK not found" in s.log(), s.log()[-300:])
        err, text = s.call("run_command", command="echo hello without rtk")
        check("echo works without RTK", not err and "hello without rtk" in text, text)
        err, text = s.call("run_command", command="perl -e 1")
        check("a command outside the allowlist is blocked", err and "allowlist" in text.lower(), text)
        err, text = s.call("run_command", command="bash -c 'echo from bash'")
        check("bash is allowlisted (carried over from the earlier local build)",
              not err and "from bash" in text, text)
        err, text = s.call("run_command", command="rm -rf /")
        check("rm is blocked by the allowlist", err and "allowlist" in text.lower(), text)
        err, text = s.call("get_rtk_savings")
        check("savings explains RTK is missing", not err and "RTK not installed" in text, text)
        err, text = s.call("get_rtk_discover")
        check("discover explains RTK is missing", not err and "RTK not installed" in text, text)
        err, text = s.call("get_rtk_session")
        check("session explains RTK is missing", not err and "RTK not installed" in text, text)
    finally:
        s.close()

    s = Server(path=path, extra_env={"RTK_MCP_ALLOW_ALL": "1"})
    try:
        err, text = s.call("run_command", command="sh -c 'echo allowed by env'")
        check("RTK_MCP_ALLOW_ALL=1 lifts the allowlist", not err and "allowed by env" in text, text)
    finally:
        s.close()


def main():
    if not os.access(BINARY, os.X_OK):
        print("binary not found: " + BINARY)
        return 2
    version = rtk_version()
    print("rtk-mcp: %s\nrtk: %s\n" % (BINARY, ".".join(map(str, version)) if version else "not installed"))
    if version:
        test_with_rtk(version)
    else:
        skip("RTK-backed tests", "rtk not on PATH")
    test_runtime_robustness()
    test_client_cancel()
    test_output_limits()
    test_cwd_jail(bool(version))
    test_background_jobs()
    test_without_rtk()
    print()
    if FAILURES:
        print("%d FAILED: %s" % (len(FAILURES), "; ".join(FAILURES)))
        return 1
    print("All functional tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
