#!/usr/bin/env python3
"""End-to-end test of the rtk-mcp binary over real MCP stdio.

usage: python3 tests/functional.py [path/to/rtk-mcp]     (default: target/release/rtk-mcp[.exe])
       python tests/functional.py path/to/rtk-mcp.exe --installed [claude_desktop_config.json]

The second form checks what install.ps1 leaves behind (an rtk.exe beside the server, RTK not on
PATH, and the Claude Desktop config it edited) instead of running the whole suite.

Runs on Linux, macOS and Windows. On Windows the shared tests run under Git Bash (the shell the
server picks when Git for Windows is installed) and a section of their own covers cmd.exe.

Uses whatever `rtk` is on PATH (tests that need RTK are skipped when there is none, and the
no-RTK fallback is exercised separately). Every server runs in a throwaway HOME, so nothing
here reads or writes your real RTK data or Claude settings (on Windows RTK finds its data
through the profile folder, not through HOME, so there the runner's own is used). Exit status 1
on any failure.
"""
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

if hasattr(sys.stdout, "reconfigure"):
    # A pipe on Windows is cp1252, and what the server prints back includes Japanese
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

IS_WINDOWS = os.name == "nt"
ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
INSTALLED = "--installed" in sys.argv[1:]  # check an install.ps1 layout instead of running everything
BINARY = os.path.abspath(ARGS[0] if ARGS else "target/release/rtk-mcp" + (".exe" if IS_WINDOWS else ""))
FAILURES = []

# Things that differ between a POSIX shell on Linux/macOS and Git Bash on Windows.
PWD = "pwd -W" if IS_WINDOWS else "pwd"  # Git Bash: `pwd -W` prints the Windows form of the directory
# The pid of the last background job, as the OS knows it. Git Bash starts a program by fork and exec,
# and /proc/$!/winpid names the forked shell until the exec has happened, so it waits a moment. It
# reads the file with the shell's own `read`: RTK 0.34.3 rewrites a `cat` it finds inside $( ) to
# `rtk read`, a native program that cannot see Git Bash's /proc.
BG_PID = "$(sleep 1; read -r p < /proc/$!/winpid; echo $p)" if IS_WINDOWS else "$!"
# The timeout of the servers that time commands out: Git Bash needs a while to start, and the
# command that records a background job's pid waits a second more.
SHORT_TIMEOUT = 6 if IS_WINDOWS else 2
# A path with no system dirs other than the basics, for the "RTK is missing" tests.
BASIC_PATH = (
    os.pathsep.join(os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), d) for d in ("System32", ""))
    if IS_WINDOWS
    else "/usr/bin:/bin"
)


def check(name, ok, detail=""):
    print(("PASS" if ok else "FAIL") + ": " + name)
    if not ok:
        print("      " + str(detail)[:600])
        FAILURES.append(name)


def skip(name, why):
    print("SKIP: " + name + " (" + why + ")")


def sh_path(path):
    """A path as it is written inside a shell command line (Git Bash wants forward slashes)."""
    return path.replace("\\", "/") if IS_WINDOWS else path


def same_dir(printed, expected):
    """Is `printed` (the output of PWD) the directory `expected`?"""
    printed = printed.strip()
    if IS_WINDOWS:
        return os.path.normcase(os.path.normpath(printed)) == os.path.normcase(os.path.normpath(expected))
    return printed == expected


class Server:
    """One rtk-mcp process in an isolated HOME, speaking line-delimited JSON-RPC."""

    def __init__(self, path=None, deny=(), extra_env=None, binary=None):
        self.root = tempfile.mkdtemp(prefix="rtk-mcp-func-")
        self.home = os.path.join(self.root, "home")
        self.project = os.path.join(self.root, "project")
        os.makedirs(os.path.join(self.home, ".claude", "projects"))
        os.makedirs(os.path.join(self.project, ".claude"))
        # RTK reads deny rules from the user's and from the project's Claude settings. On Windows it
        # finds the user's through the profile folder, not through HOME, so the project's file (found
        # by walking up from the working directory) is the one a test controls there.
        settings = json.dumps({"permissions": {"deny": list(deny)}})
        for directory in (os.path.join(self.home, ".claude"), os.path.join(self.project, ".claude")):
            with open(os.path.join(directory, "settings.json"), "w") as f:
                f.write(settings)
        # Windows finds its shells through SystemRoot, ComSpec and ProgramFiles, so it keeps the
        # environment; elsewhere the server gets nothing but what is listed here.
        self.env = dict(os.environ) if IS_WINDOWS else {}
        self.env.update({
            "HOME": self.home,
            "XDG_CONFIG_HOME": os.path.join(self.home, ".config"),
            "XDG_DATA_HOME": os.path.join(self.home, ".local", "share"),
            "PATH": path or os.environ.get("PATH", "/usr/bin:/bin"),
            "LANG": "C.UTF-8",
            "RTK_FUNC_TOKEN": "pipe-ok",
        })
        if IS_WINDOWS:
            self.env.update({
                "USERPROFILE": self.home,
                "APPDATA": os.path.join(self.home, "AppData", "Roaming"),
                "LOCALAPPDATA": os.path.join(self.home, "AppData", "Local"),
            })
            # RTK asks Windows for its data folder, and Windows only names a folder that exists
            for sub in ("Roaming", "Local"):
                os.makedirs(os.path.join(self.home, "AppData", sub), exist_ok=True)
        for key, value in (extra_env or {}).items():
            if IS_WINDOWS:  # variable names are case-insensitive there; os.environ upper-cases them
                for existing in [k for k in self.env if k.upper() == key.upper()]:
                    del self.env[existing]
            self.env[key] = value
        # The child appends to this file; reading goes through a separate handle, so the two never
        # share a file offset.
        self.log_path = os.path.join(self.root, "stderr.log")
        self.stderr = open(self.log_path, "ab")
        self.proc = subprocess.Popen(
            [binary or BINARY], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            env=self.env, cwd=self.project,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.next_id = 1
        init = self.request("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "functional", "version": "0"},
        })
        self.init = init["result"]
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _pump(self):
        """Move the server's stdout lines onto a queue (select() does not work on pipes on Windows)."""
        try:
            for line in iter(self.proc.stdout.readline, b""):
                self.lines.put(line)
        finally:
            self.lines.put(None)

    def _send(self, message):
        self.proc.stdin.write((json.dumps(message) + "\n").encode("utf-8"))
        self.proc.stdin.flush()

    def request(self, method, params=None, timeout=60):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                line = self.lines.get(timeout=0.2)
            except queue.Empty:
                if self.proc.poll() is not None:
                    raise RuntimeError("server exited with %s" % self.proc.returncode)
                continue
            if line is None:
                raise RuntimeError("server closed stdout")
            message = json.loads(line.decode("utf-8"))
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

    def tools(self):
        return {t["name"]: t for t in self.request("tools/list")["result"]["tools"]}

    def alive(self):
        return self.proc.poll() is None

    def log(self):
        self.stderr.flush()
        with open(self.log_path, "r", encoding="utf-8", errors="replace") as f:
            return f.read()

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
        self.stderr.close()
        shutil.rmtree(self.root, ignore_errors=True)


def rtk_version(path=None):
    # Look the program up in the PATH we were given: on Windows a bare name is resolved against
    # this process's own PATH, not the `env` passed to the child.
    exe = shutil.which("rtk", path=path or os.environ.get("PATH", ""))
    if not exe:
        return None
    try:
        out = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    numbers = re.findall(r"\d+", out)
    return tuple(int(n) for n in numbers[:3]) if len(numbers) >= 3 else None


def process_alive(pid):
    if IS_WINDOWS:
        out = subprocess.run(
            ["tasklist", "/FI", "PID eq %d" % pid, "/FO", "CSV", "/NH"], capture_output=True, text=True
        ).stdout
        return '"%d"' % pid in out
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


def read_pid(pidfile, seconds=0):
    """The pid a command wrote into `pidfile`, waiting up to `seconds` for it to appear."""
    deadline = time.time() + seconds
    while True:
        try:
            with open(pidfile) as f:
                return int(f.read().strip())
        except (OSError, ValueError):
            if time.time() >= deadline:
                raise
            time.sleep(0.1)


def test_with_rtk(version):
    marker = None
    s = Server(deny=["Bash(touch:*)", "Bash(git push:*)"])
    try:
        # --- handshake and tool surface -------------------------------------------------------
        check("initialize reports rtk-mcp", s.init["serverInfo"]["name"] == "rtk-mcp", s.init)
        check("initialize carries instructions", "run_command" in s.init.get("instructions", ""))
        tools = s.tools()
        check("tools/list has the 4 tools", set(tools) == {
            "run_command", "get_rtk_savings", "get_rtk_discover", "get_rtk_session"}, sorted(tools))
        props = tools["run_command"]["inputSchema"]["properties"]
        check("run_command takes command + cwd", "command" in props and "cwd" in props, props)
        desc = tools["run_command"]["description"]
        check("run_command documents the configurable limits",
              "RTK_MCP_TIMEOUT_SECS (default 300s)" in desc and "RTK_MCP_MAX_OUTPUT_KB" in desc, desc[-400:])
        if IS_WINDOWS:
            check("run_command names the Windows shell instead of sh -c",
                  "on this Windows machine" in desc and "`sh -c`" not in desc, desc[-500:])
        else:
            check("run_command describes sh -c, as it always did", "runs under `sh -c`" in desc, desc[-500:])
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
        err, text = s.call("run_command", command="echo $RTK_FUNC_TOKEN | tr a-z A-Z && true")
        check("shell operators work", not err and "PIPE-OK" in text, text)
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
        err, text = s.call("run_command", command="touch " + sh_path(marker))
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
        if IS_WINDOWS and err and "projects directory not found" in text:
            # This RTK looks for Claude Code's history in the real profile folder on Windows, not
            # in the HOME the test gave it, and a CI runner has none.
            skip("discover all=true / since / limit", "no Claude Code history in this profile folder")
        else:
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
    timed_out = "timed out after %ds" % SHORT_TIMEOUT
    s = Server(extra_env={"RTK_MCP_TIMEOUT_SECS": str(SHORT_TIMEOUT), "RTK_MCP_MAX_OUTPUT_KB": "64"})
    try:
        err, text = s.call("run_command", command="echo partial-result; sleep 60")
        check("a timeout still returns the output printed so far",
              err and timed_out in text and "partial-result" in text, text)

        pidfile = os.path.join(s.project, "grandchild.pid")
        started = time.time()
        err, text = s.call("run_command", command="sleep 300 & echo %s > %s; wait" % (BG_PID, sh_path(pidfile)))
        check("timeout is reported", err and timed_out in text, text)
        check("timeout returns promptly", time.time() - started < SHORT_TIMEOUT + 13, time.time() - started)
        try:
            pid = read_pid(pidfile)
            check("timeout killed the grandchild too (whole process tree)", wait_dead(pid), "pid %d alive" % pid)
        except (OSError, ValueError) as e:
            check("grandchild pid recorded", False, e)

        err, text = s.call("run_command", command="yes 日本語日本語 | head -c 200000")
        check("multibyte output over the cap (64KB here) is truncated, not fatal",
              not err and "[output truncated at 64KB" in text and s.alive(), text[-120:])
        check("truncated output is valid UTF-8 text", text.encode("utf-8").decode("utf-8") == text)
        err, text = s.call("run_command", command="echo alive")
        check("server alive after truncation", not err and "alive" in text, text)

        err, text = s.call("run_command", command="cat")
        check("stdin of children is isolated", not err and text == "(no output)", text)
    finally:
        s.close()

    # A server of its own: this check is about memory, not time, and BSD `tr` on a busy CI runner
    # needs longer than the 2s timeout above.
    s = Server(extra_env={"RTK_MCP_MAX_OUTPUT_KB": "64"})
    try:
        err, text = s.call("run_command", command="head -c 30000000 /dev/zero | tr '\\0' x")
        check("30MB of output is handled (capped, not buffered)", not err and "[output truncated" in text, text[-120:])
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
            "arguments": {"command": "sleep 300 & echo %s > %s; wait" % (BG_PID, sh_path(pidfile))}}})
        try:
            pid = read_pid(pidfile, seconds=15)
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
        err, text = s.call("run_command", command=PWD, cwd=inside)
        check("cwd inside the jail runs there", not err and same_dir(text, inside), text)
        err, text = s.call("run_command", command=PWD, cwd=jail)
        check("the jail root itself is allowed", not err and same_dir(text, jail), text)
        err, text = s.call("run_command", command="pwd", cwd=outside)
        check("cwd outside the jail is refused", err and "outside allowed root" in text, text)
        err, text = s.call("run_command", command="pwd", cwd=os.path.join(inside, "..", ".."))
        check(".. cannot be used to climb out", err and "outside allowed root" in text, text)
        err, text = s.call("run_command", command="touch " + sh_path(marker), cwd=outside)
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
        err, text = s.call("run_command", command="sleep 300 & echo " + BG_PID)
        took = time.time() - started
        check("a job holding the output pipes does not hold the call", not err and took < 15, "%.1fs: %s" % (took, text))
        check("the reply says the job is detached", "background process" in text, text)
        pid = int(text.split()[0])
        pids.append(pid)
        time.sleep(0.5)
        check("the job is still running", process_alive(pid))

        started = time.time()
        err, text = s.call("run_command", command="nohup sleep 300 >/dev/null 2>&1 & echo " + BG_PID)
        took = time.time() - started
        check("a job with redirected output returns at once, with no note",
              not err and took < 5 and "background process" not in text, "%.1fs: %s" % (took, text))
        pid = int(text.split()[0])
        pids.append(pid)
        check("that job is running too", process_alive(pid))

        marker = os.path.join(s.project, "late-marker")
        err, text = s.call("run_command", command="( sleep 2; head -c 300000 /dev/zero; touch %s ) & echo started" % sh_path(marker))
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
    path = BASIC_PATH
    if rtk_version(path) is not None:
        skip("no-RTK fallback", "rtk is installed in the system dirs")
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


def windows_default_shell():
    """The shell the server picks on this machine, read from its own startup log.

    ("bash", path of Git Bash's bash.exe) or ("cmd", None).
    """
    s = Server()
    try:
        log = s.log()
    finally:
        s.close()
    if "/d /s /c" in log.lower():
        return "cmd", None
    found = re.search(r"commands run under (.+?) -c", log)
    return "bash", (found.group(1) if found else None)


def test_windows(version, bash_path):
    """Windows only: picking the shell, cmd.exe, the Windows allowlist, an rtk.exe beside the server."""
    # --- Git Bash is the default when Git for Windows is installed --------------------------------
    if bash_path:
        s = Server()
        try:
            check("auto: Git Bash is the shell when Git for Windows is installed",
                  "bash.exe -c" in s.log().lower(), s.log()[-300:])
            check("auto: the instructions give POSIX syntax",
                  "POSIX shell on Windows" in s.init["instructions"], s.init["instructions"][-400:])
            err, text = s.call("run_command", command="cmd //c exit 7")
            check("auto: a native program's exit code comes through Git Bash",
                  err and "[exit code: 7]" in text, text)
            utf8_script = os.path.join(s.project, "utf8.py")
            with open(utf8_script, "w", encoding="utf-8") as f:
                f.write("print('héllo 日本語 العربية')\n")
            err, text = s.call("run_command", command="'%s' utf8.py" % sh_path(sys.executable), cwd=s.project)
            check("auto: Python can print outside the ANSI code page (PYTHONIOENCODING)",
                  not err and "héllo 日本語 العربية" in text, text)
            err, text = s.call("run_command", command="echo héllo 日本語 العربية")
            check("auto: Git Bash echoes French, Japanese and Arabic as typed",
                  not err and "héllo 日本語 العربية" in text, text)
        finally:
            s.close()

    # --- cmd.exe on request -------------------------------------------------------------------------
    s = Server(extra_env={"RTK_MCP_SHELL": "cmd"})
    try:
        project = os.path.realpath(s.project)
        check("RTK_MCP_SHELL=cmd: commands run under cmd.exe", "/d /s /c" in s.log().lower(), s.log()[-300:])
        desc = s.tools()["run_command"]["description"]
        check("cmd: run_command says cmd.exe, not sh -c",
              "runs under cmd.exe on this Windows machine" in desc and "`sh -c`" not in desc, desc[-500:])
        instructions = s.init["instructions"]
        check("cmd: the instructions give Windows syntax",
              "Commands run under cmd.exe" in instructions and "`sh -c`" not in instructions, instructions[-500:])
        for name in ("alpha.txt", "beta.txt"):
            open(os.path.join(s.project, name), "w").write(name)
        spaced = os.path.join(s.project, "with space é")
        os.makedirs(spaced)

        err, text = s.call("run_command", command="echo hello from cmd")
        check("cmd: echo runs", not err and "hello from cmd" in text, text)
        err, text = s.call("run_command", command="dir /b", cwd=s.project)
        check("cmd: dir lists the files of cwd", not err and "alpha.txt" in text and "beta.txt" in text, text)
        err, text = s.call("run_command", command="cd", cwd=s.project)
        check("cmd: cwd is applied (and arrives without the \\\\?\\ prefix)",
              not err and same_dir(text, project), text)
        err, text = s.call("run_command", command="cd", cwd=spaced)
        check("cmd: a cwd with a space and a non-ASCII letter works",
              not err and same_dir(text, os.path.realpath(spaced)), text)
        err, text = s.call("run_command", command="echo %RTK_FUNC_TOKEN%")
        check("cmd: %VAR% expands", not err and text.strip() == "pipe-ok", text)
        err, text = s.call("run_command", command="RTK_DISABLED=1 echo %RTK_DISABLED%")
        check("cmd: a leading NAME=value goes into the environment", not err and text.strip() == "1", text)
        err, text = s.call("run_command", command='echo "a & b" && echo second')
        check("cmd: quotes and operators reach cmd untouched",
              not err and '"a & b"' in text and "second" in text, text)
        err, text = s.call("run_command", command="echo abc | findstr b")
        check("cmd: pipes work", not err and "abc" in text, text)
        err, text = s.call("run_command", command="echo 日本語")
        check("cmd: non-ASCII output survives (UTF-8 code page)", not err and "日本語" in text, text)
        err, text = s.call("run_command", command="echo héllo 日本語 العربية")
        check("cmd: echo prints French, Japanese and Arabic as typed",
              not err and "héllo 日本語 العربية" in text, text)
        odd_names = ("日本語.txt", "héllo.txt", "العربية.txt")
        for name in odd_names:
            with open(os.path.join(s.project, name), "w", encoding="utf-8") as f:
                f.write(name)
        err, text = s.call("run_command", command="dir /b", cwd=s.project)
        check("cmd: dir shows file names outside the OEM code page",
              not err and all(name in text for name in odd_names), text)
        utf8_script = os.path.join(s.project, "utf8.py")
        with open(utf8_script, "w", encoding="utf-8") as f:
            f.write("print('héllo 日本語 العربية')\n")
        err, text = s.call("run_command", command='"%s" utf8.py' % sys.executable, cwd=s.project)
        check("cmd: Python can print outside the ANSI code page (PYTHONIOENCODING)",
              not err and "héllo 日本語 العربية" in text, text)
        err, text = s.call("run_command", command="exit 3")
        check("cmd: a non-zero exit is an error with the code", err and "[exit code: 3]" in text, text)
        err, text = s.call("run_command", command='powershell -NoProfile -Command "Write-Output ps-ok"')
        check("cmd: PowerShell is one command away", not err and "ps-ok" in text, text)

        big = os.path.join(s.project, "big.txt")
        with open(big, "w") as f:
            f.write("y" * 400000)
        err, text = s.call("run_command", command="type big.txt", cwd=s.project)
        check("cmd: 400KB of output is cut at 256KB",
              not err and text.startswith("y" * 262144) and "[output truncated at 256KB" in text, text[-120:])

        if version:
            subprocess.run(["git", "init", "-q", "."], cwd=s.project, env=s.env)
            err, text = s.call("run_command", command="git status", cwd=s.project)
            check("cmd: git status (RTK-filtered) works", not err and text.strip() != "", text)
        check("cmd: server still alive", s.alive())
    finally:
        s.close()

    s = Server(extra_env={"RTK_MCP_SHELL": "cmd", "RTK_MCP_TIMEOUT_SECS": "4"})
    try:
        pidfile = os.path.join(s.project, "cmd-grandchild.pid")
        code = "import os,time;open(r'%s','w').write(str(os.getpid()));time.sleep(300)" % pidfile
        started = time.time()
        err, text = s.call("run_command", command='"%s" -c "%s"' % (sys.executable, code))
        check("cmd: a timeout is reported", err and "timed out after 4s" in text, text)
        check("cmd: the timeout returns promptly", time.time() - started < 25, time.time() - started)
        try:
            pid = read_pid(pidfile, seconds=5)
            check("cmd: the timeout killed the whole process tree", wait_dead(pid), "pid %d alive" % pid)
        except (OSError, ValueError) as e:
            check("cmd: the grandchild started", False, e)
    finally:
        s.close()

    # --- a shell given by path ----------------------------------------------------------------------
    usr_bash = os.path.join(os.path.dirname(os.path.dirname(bash_path or "")), "usr", "bin", "bash.exe")
    if bash_path and os.path.isfile(usr_bash):
        # PATH without Git's tools: the server has to add them for a shell it was pointed at
        s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_SHELL": usr_bash, "RTK_MCP_ALLOW_ALL": "1"})
        try:
            check("RTK_MCP_SHELL=<path>: that shell is used", usr_bash.lower() in s.log().lower(), s.log()[-300:])
            err, text = s.call("run_command", command="uname -s")
            check("RTK_MCP_SHELL=<path>: the tools next to it are on PATH", not err and "NT" in text, text)
        finally:
            s.close()
    else:
        skip("RTK_MCP_SHELL=<path>", "no Git for Windows")

    s = Server(extra_env={"RTK_MCP_SHELL": "nonsense"})
    try:
        check("an unknown RTK_MCP_SHELL is reported and ignored", "is not a shell I know" in s.log(), s.log()[-300:])
        err, text = s.call("run_command", command="echo still works")
        check("an unknown RTK_MCP_SHELL still leaves a working shell", not err and "still works" in text, text)
    finally:
        s.close()

    # --- no Git for Windows: cmd.exe is the fallback, and the Windows allowlist --------------------
    nothing = tempfile.mkdtemp(prefix="rtk-mcp-nogit-")
    no_git = {"ProgramFiles": nothing, "ProgramW6432": nothing, "ProgramFiles(x86)": nothing, "LocalAppData": nothing}
    try:
        s = Server(path=BASIC_PATH, extra_env=no_git)
        try:
            check("no Git: cmd.exe is the fallback", "/d /s /c" in s.log().lower(), s.log()[-300:])
            check("no RTK: the startup log says so", "RTK not found" in s.log(), s.log()[-300:])
            err, text = s.call("run_command", command="echo %RTK_FUNC_TOKEN%")
            check("no Git, no RTK: echo works", not err and text.strip() == "pipe-ok", text)
            err, text = s.call("run_command", command="dir /b", cwd=s.project)
            check("no RTK: dir is allowed on Windows", not err and ".claude" in text, text)
            err, text = s.call("run_command", command="where cmd")
            check("no RTK: where is allowed on Windows", not err and "cmd.exe" in text.lower(), text)
            err, text = s.call("run_command", command=r"%SystemRoot%\System32\HOSTNAME.EXE")
            check("no RTK: a program given by path, extension and capitals is matched by its name",
                  not err and text.strip() != "", text)
            err, text = s.call("run_command", command='"%SystemRoot%\\System32\\hostname.exe" /?')
            check("no RTK: a quoted program path is matched by its name too", "allowlist" not in text.lower(), text)
            keep = os.path.join(s.project, "keep.txt")
            open(keep, "w").write("keep")
            err, text = s.call("run_command", command="del keep.txt", cwd=s.project)
            check("no RTK: del is refused by the allowlist", err and "allowlist" in text.lower(), text)
            check("no RTK: and the file is still there", os.path.exists(keep))
            err, text = s.call("run_command", command="perl -e 1")
            check("no RTK: a program outside the allowlist is refused", err and "allowlist" in text.lower(), text)
            err, text = s.call("get_rtk_savings")
            check("no RTK: savings says how to install it on Windows",
                  not err and "RTK not installed" in text and "winget install rtk-ai.rtk" in text, text)
        finally:
            s.close()

        s = Server(path=BASIC_PATH, extra_env=dict(no_git, RTK_MCP_SHELL="bash"))
        try:
            check("RTK_MCP_SHELL=bash without Git: warns and uses cmd.exe",
                  "Git Bash was not found" in s.log() and "/d /s /c" in s.log().lower(), s.log()[-300:])
        finally:
            s.close()
    finally:
        shutil.rmtree(nothing, ignore_errors=True)

    # --- rtk.exe installed next to rtk-mcp.exe, not on PATH ------------------------------------------
    rtk = shutil.which("rtk") if version else None
    if not rtk:
        skip("bundled rtk.exe", "rtk is not installed")
        return
    folder = tempfile.mkdtemp(prefix="rtk-mcp-bundle-")
    try:
        server_copy = os.path.join(folder, "rtk-mcp.exe")
        shutil.copy2(BINARY, server_copy)
        shutil.copy2(rtk, os.path.join(folder, "rtk.exe"))
        s = Server(path=BASIC_PATH, binary=server_copy)
        try:
            check("bundled rtk.exe: found beside the server although it is not on PATH",
                  "detected" in s.log() and "RTK not found" not in s.log(), s.log()[-300:])
            err, text = s.call("run_command", command="rtk --version")
            check("bundled rtk.exe: the commands it runs find it too", not err and "rtk " in text, text)
            err, text = s.call("get_rtk_savings")
            check("bundled rtk.exe: the stats tools find it",
                  not err and text.strip() != "" and "RTK not installed" not in text, text)
        finally:
            s.close()
    finally:
        shutil.rmtree(folder, ignore_errors=True)


def test_installed(config_path):
    """What install.ps1 leaves behind: rtk.exe beside the server, nothing about RTK on PATH, and a
    Claude Desktop config that kept what it had (the workflow seeds `theme` and a server `other`)."""
    check("rtk.exe sits beside the server", os.path.isfile(os.path.join(os.path.dirname(BINARY), "rtk.exe")))
    s = Server(path=BASIC_PATH)
    try:
        check("the server finds the RTK installed beside it",
              "detected" in s.log() and "RTK not found" not in s.log(), s.log()[-300:])
        err, text = s.call("run_command", command="rtk --version")
        check("commands it runs find that RTK too", not err and text.startswith("rtk "), text)
        err, text = s.call("run_command", command="echo installed")
        check("a command runs", not err and "installed" in text, text)
        err, text = s.call("get_rtk_savings")
        check("the stats tools work", not err and text.strip() != "" and "RTK not installed" not in text, text)
    finally:
        s.close()
    if config_path:
        with open(config_path, encoding="utf-8-sig") as f:
            config = json.load(f)
        servers = config.get("mcpServers", {})
        command = servers.get("rtk", {}).get("command", "")
        check("config: rtk points at the installed server",
              os.path.normcase(os.path.normpath(command)) == os.path.normcase(BINARY), servers.get("rtk"))
        check("config: the other server is kept as it was",
              servers.get("other") == {"command": "x", "args": ["a"]}, servers.get("other"))
        check("config: the rest of the file is kept", config.get("theme") == "dark", config)
        backups = [n for n in os.listdir(os.path.dirname(config_path))
                   if n.startswith(os.path.basename(config_path) + ".bak-")]
        check("config: a backup of the old file was made", len(backups) >= 1, os.listdir(os.path.dirname(config_path)))


def main():
    if not os.access(BINARY, os.X_OK):
        print("binary not found: " + BINARY)
        return 2
    if INSTALLED:
        test_installed(ARGS[1] if len(ARGS) > 1 else None)
        print()
        if FAILURES:
            print("%d FAILED: %s" % (len(FAILURES), "; ".join(FAILURES)))
            return 1
        print("The installation checks passed")
        return 0
    version = rtk_version()
    print("rtk-mcp: %s\nrtk: %s\n" % (BINARY, ".".join(map(str, version)) if version else "not installed"))
    posix, bash_path = True, None
    if IS_WINDOWS:
        kind, bash_path = windows_default_shell()
        posix = kind == "bash"
        print("shell: %s\n" % (bash_path if posix else "cmd.exe (Git for Windows is not installed)"))
    if not posix:
        skip("the tests written in POSIX shell syntax", "no Git for Windows")
    else:
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
    if IS_WINDOWS:
        test_windows(version, bash_path)
    print()
    if FAILURES:
        print("%d FAILED: %s" % (len(FAILURES), "; ".join(FAILURES)))
        return 1
    print("All functional tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
