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
import hashlib
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
# The tests that download from GitHub (install_rtk, RTK_MCP_AUTO_INSTALL, the pinned checksums):
# see network_enabled().
NETWORK_SETTING = os.environ.get("RTK_MCP_TEST_NETWORK", "").lower()
RTK_EXE = "rtk.exe" if IS_WINDOWS else "rtk"
ALL_TOOLS = {
    "run_command", "get_rtk_savings", "get_rtk_discover", "get_rtk_session",
    "install_rtk", "get_rtk_status", "get_rtk_recall", "get_rtk_config", "get_rtk_rewrite",
}
READ_ONLY_TOOLS = (
    "get_rtk_savings", "get_rtk_discover", "get_rtk_session",
    "get_rtk_status", "get_rtk_recall", "get_rtk_config", "get_rtk_rewrite",
)
ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
INSTALLED = "--installed" in sys.argv[1:]  # check an install.ps1 layout instead of running everything
BINARY = os.path.abspath(ARGS[0] if ARGS else "target/release/rtk-mcp" + (".exe" if IS_WINDOWS else ""))
FAILURES = []
# A proxy (and its CA bundle) that the machine needs to reach GitHub is not part of the isolation:
# curl, inside the server, has to see it for the tests that download.
PROXY_VARIABLES = (
    "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy",
    "NO_PROXY", "no_proxy", "CURL_CA_BUNDLE", "SSL_CERT_FILE", "SSL_CERT_DIR",
)
# Nothing listens on port 9, so a server given this proxy cannot reach GitHub, at once and without
# waiting for a timeout. Both spellings: curl prefers the lower-case one.
DEAD_PROXY = {name: "http://127.0.0.1:9" for name in ("HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy")}
DEAD_PROXY.update({"NO_PROXY": "", "no_proxy": ""})

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

    def __init__(self, path=None, deny=(), extra_env=None, binary=None, home_files=()):
        self.root = tempfile.mkdtemp(prefix="rtk-mcp-func-")
        self.home = os.path.join(self.root, "home")
        self.project = os.path.join(self.root, "project")
        os.makedirs(os.path.join(self.home, ".claude", "projects"))
        os.makedirs(os.path.join(self.project, ".claude"))
        # files to put into HOME before the server starts: (source, path relative to HOME)
        for source, relative in home_files:
            target = os.path.join(self.home, relative)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copy2(source, target)
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
        for name in PROXY_VARIABLES:
            if name in os.environ:
                self.env.setdefault(name, os.environ[name])
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

    def call(self, name, _timeout=60, **arguments):
        """tools/call -> (is_error, text); a protocol-level error comes back as (True, error)."""
        response = self.request("tools/call", {"name": name, "arguments": arguments}, timeout=_timeout)
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
    # pinned: with an RTK older than the pinned release, install_rtk would go and fetch the latest
    s = Server(deny=["Bash(touch:*)", "Bash(git push:*)"], extra_env={"RTK_MCP_RTK_VERSION": "pinned"})
    try:
        # --- handshake and tool surface -------------------------------------------------------
        check("initialize reports rtk-mcp", s.init["serverInfo"]["name"] == "rtk-mcp", s.init)
        check("initialize carries instructions", "run_command" in s.init.get("instructions", ""))
        check("the instructions say to use run_command instead of other shell tools",
              "instead of any built-in bash tool or other shell or terminal tool" in s.init.get("instructions", ""),
              s.init.get("instructions", "")[:300])
        check("the instructions point at install_rtk", "call install_rtk once" in s.init.get("instructions", ""),
              s.init.get("instructions", "")[-300:])
        tools = s.tools()
        check("tools/list has the 9 tools", set(tools) == ALL_TOOLS, sorted(tools))
        props = tools["run_command"]["inputSchema"]["properties"]
        check("run_command takes command + cwd", "command" in props and "cwd" in props, props)
        desc = tools["run_command"]["description"]
        check("run_command's description leads with 'use this for ALL shell commands'",
              desc.startswith("Run a shell command through RTK")
              and "Use this for ALL shell commands" in desc[:250]
              and "instead of any other shell, terminal or command tool" in desc[:350], desc[:350])
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
        check("run_command asks a deferring client to load it upfront (anthropic/alwaysLoad)",
              tools["run_command"].get("_meta", {}).get("anthropic/alwaysLoad") is True,
              tools["run_command"].get("_meta"))
        check("no other tool does",
              [name for name, tool in tools.items() if name != "run_command" and "_meta" in tool] == [],
              {name: tool.get("_meta") for name, tool in tools.items() if "_meta" in tool})
        check("stats, status, recall, config and rewrite tools are flagged read-only", all(
            tools[n]["annotations"].get("readOnlyHint") is True for n in READ_ONLY_TOOLS))
        install_annotations = tools["install_rtk"]["annotations"]
        check("install_rtk is flagged destructive (it can replace RTK), idempotent and open-world",
              install_annotations.get("destructiveHint") is True
              and install_annotations.get("idempotentHint") is True
              and install_annotations.get("openWorldHint") is True, install_annotations)
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

        # --- the tools added in 0.7.0 --------------------------------------------------------
        err, text = s.call("get_rtk_status")
        check("status names the RTK in use, where it is and how it was found",
              not err and ("RTK: rtk %d.%d.%d at " % version) in text and "found on PATH" in text, text)
        check("status describes the mode, the shell and the limits",
              "every command goes through `rtk rewrite`" in text and "Shell: " in text
              and "Limits: command 4096 bytes, output 256 KB, timeout 300 s" in text, text)
        err, text = s.call("get_rtk_rewrite", command="git status")
        check("rewrite preview shows the rewritten form", not err and text.startswith("Rewritten: ") and "rtk" in text, text)
        err, text = s.call("get_rtk_rewrite", command="echo plain")
        check("rewrite preview says when RTK has no filter", not err and text.startswith("No RTK filter"), text)
        err, text = s.call("get_rtk_rewrite", command="git push origin main", cwd=s.project)
        check("rewrite preview reports a deny rule", not err and text.startswith("Denied"), text)
        marker = os.path.join(s.project, "preview-marker")
        err, text = s.call("get_rtk_rewrite", command="touch " + sh_path(marker))
        check("a rewrite preview never runs the command", text.startswith("Denied") and not os.path.exists(marker), text)
        err, text = s.call("get_rtk_rewrite", command="  ")
        check("rewrite preview rejects an empty command", err and "empty" in text.lower(), text)
        err, text = s.call("get_rtk_rewrite", command="ls", cwd=os.path.join(s.project, "missing"))
        check("rewrite preview checks the cwd like run_command does", err and "invalid cwd" in text, text)
        err, text = s.call("get_rtk_config")
        check("config shows RTK's configuration", not err and text.strip() != "", text)
        if version >= (0, 49, 0):
            err, text = s.call("get_rtk_recall", list=True)
            check("recall list works on RTK >= 0.49", not err, text)
            err, text = s.call("get_rtk_recall")
            check("recall without a hash or list says what to pass", err and "hash" in text and "list=true" in text, text)
            err, text = s.call("get_rtk_recall", hash="--full", grep="x")
            check("a hash that looks like an option is not taken for one", s.alive() and "unexpected argument" not in text, text)
        else:
            err, text = s.call("get_rtk_recall", list=True)
            check("recall on an RTK that predates it says so instead of running it",
                  err and "predates `rtk recall`" in text and "0.49.0" in text, text)
        err, text = s.call("get_rtk_status")
        if version >= pinned_version():
            check("status does not call an up-to-date RTK out of date", "Update:" not in text, text)
            err, text = s.call("install_rtk")
            check("install_rtk with an up-to-date RTK installs nothing and says where RTK is",
                  not err and "already installed and up to date" in text and ("rtk %d.%d.%d" % version) in text
                  and "Nothing to install." in text, text)
        else:
            check("status calls an RTK older than the pinned release out of date and names the tool that fixes it",
                  ("Update: RTK %d.%d.%d is older than " % version) in text
                  and "call install_rtk to upgrade it" in text, text)
            skip("install_rtk with an up-to-date RTK", "this RTK is older than the pinned release, so it would be upgraded")

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


def usual_rtk_places():
    """RTKs the server would find although the test gives it a PATH without one: outside the
    throwaway HOME it also looks in Homebrew's and /usr/local's directories."""
    if IS_WINDOWS:
        return []
    places = ("/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin")
    return [d for d in places if os.path.isfile(os.path.join(d, "rtk"))]


def tool_version():
    """The RTK release tag install_rtk is pinned to, read from the source."""
    source = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "install.rs")
    with open(source, encoding="utf-8") as f:
        return re.search(r'tag \{\s*\(\) => \{\s*"(v\d+\.\d+\.\d+)"', f.read().replace("\n", " ")).group(1)


def pinned_version():
    """The RTK release install_rtk is pinned to, as a tuple."""
    return tuple(int(n) for n in tool_version().lstrip("v").split("."))


def latest_release_online(tries=3):
    """The release GitHub calls the latest RTK, as a tuple, or None when this machine cannot ask.

    Asked the way the server asks (the redirect of releases/latest), but by this script, so the tests
    know what to expect from the server. A sandbox whose proxy refuses that page gets None; the
    server then installs the pinned release and says GitHub could not be asked."""
    curl = shutil.which("curl")
    if not curl:
        return None
    for attempt in range(tries):
        if attempt:
            time.sleep(3)
        try:
            got = subprocess.run(
                [curl, "-sS", "--max-time", "30", "--output", os.devnull, "--write-out", "%{http_code} %{redirect_url}",
                 "https://github.com/rtk-ai/rtk/releases/latest"],
                capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.SubprocessError):
            continue
        found = re.fullmatch(r"30\d https://github\.com/rtk-ai/rtk/releases/tag/v(\d+)\.(\d+)\.(\d+)",
                             got.stdout.strip())
        if found:
            return tuple(int(n) for n in found.groups())
    return None


def test_without_rtk():
    # A PATH that cannot contain rtk: only the system dirs.
    path = BASIC_PATH
    if rtk_version(path) is not None or usual_rtk_places():
        skip("no-RTK fallback", "rtk is installed in the system dirs")
        return
    s = Server(path=path)
    try:
        check("startup log warns RTK is missing", "RTK not found" in s.log(), s.log()[-300:])
        err, text = s.call("run_command", command="echo hello without rtk")
        check("echo works without RTK", not err and "hello without rtk" in text, text)
        check("the first result says RTK is missing and names install_rtk",
              "RTK is not installed" in text and "install_rtk" in text, text)
        err, text = s.call("run_command", command="echo second")
        check("the note is not repeated on the next command",
              not err and "second" in text and "RTK is not installed" not in text, text)
        check("install_rtk is offered", "install_rtk" in s.tools())
        check("the instructions point at install_rtk", "call install_rtk once" in s.init["instructions"])
        err, text = s.call("get_rtk_status")
        check("status says RTK is missing and what the installer would fetch",
              not err and "RTK: not installed" in text and "Mode: without RTK" in text
              and "Installer: install_rtk installs the latest RTK release (" in text
              and tool_version() in text and "upgrades an older RTK in place" in text, text)
        for tool, args in (("get_rtk_recall", {"list": True}), ("get_rtk_config", {}),
                           ("get_rtk_rewrite", {"command": "git status"})):
            err, text = s.call(tool, **args)
            check(tool + " explains RTK is missing", not err and "RTK not installed" in text and "install_rtk" in text, text)
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

    s = Server(path=path, extra_env={"RTK_MCP_NO_INSTALL": "1"})
    try:
        check("RTK_MCP_NO_INSTALL: there is no install_rtk tool", "install_rtk" not in s.tools(), sorted(s.tools()))
        check("RTK_MCP_NO_INSTALL: the instructions do not mention it", "install_rtk" not in s.init["instructions"],
              s.init["instructions"][-300:])
        err, text = s.call("run_command", command="echo no installer")
        check("RTK_MCP_NO_INSTALL: the note gives the manual command instead",
              "RTK is not installed" in text and "install_rtk" not in text
              and ("winget install rtk-ai.rtk" if IS_WINDOWS else "cargo install --git") in text, text)
        err, text = s.call("get_rtk_savings")
        check("RTK_MCP_NO_INSTALL: the stats tools do not mention it either",
              not err and "RTK not installed" in text and "install_rtk" not in text, text)
        err, text = s.call("get_rtk_status")
        check("RTK_MCP_NO_INSTALL: status says the installer is off", "Installer: turned off" in text, text)
    finally:
        s.close()


def test_rtk_locations():
    """Where the server finds RTK, and that the commands it runs find it too.

    A server started by a GUI application has a short PATH, so an RTK in ~/.local/bin, or named by
    RTK_MCP_RTK_PATH, is not on it — and a rewritten command is the string `rtk ls`, which the shell
    has to resolve by name.
    """
    real = shutil.which("rtk")
    if not real:
        skip("RTK locations", "rtk is not installed")
        return
    if usual_rtk_places():
        skip("RTK locations", "rtk is installed in a directory the server always looks in")
        return

    def ls_works_through_rtk(s, label):
        with open(os.path.join(s.project, "alpha.txt"), "w") as f:
            f.write("alpha")
        err, text = s.call("get_rtk_rewrite", command="ls")
        check(label + ": `ls` is rewritten to an rtk command", not err and text.startswith("Rewritten: rtk"), text)
        err, text = s.call("run_command", command="ls", cwd=s.project)
        check(label + ": the rewritten command runs, the shell finds rtk",
              not err and "alpha.txt" in text and "not found" not in text.lower(), text)
        err, text = s.call("run_command", command="rtk --version")
        check(label + ": `rtk` can be called by name from a command", not err and text.startswith("rtk "), text)
        err, text = s.call("get_rtk_savings", format="json")
        check(label + ": the stats tools use it too", not err and '"summary"' in text, text)

    # --- in ~/.local/bin, not on PATH ---------------------------------------------------------------
    s = Server(path=BASIC_PATH, home_files=[(real, os.path.join(".local", "bin", RTK_EXE))])
    try:
        err, text = s.call("get_rtk_status")
        check("an RTK in ~/.local/bin is found although PATH lacks it",
              not err and "found in a usual install directory" in text and ".local" in text, text)
        check("the startup log says where it was found",
              "detected at" in s.log() and "usual install directory" in s.log(), s.log()[-300:])
        ls_works_through_rtk(s, "~/.local/bin")
    finally:
        s.close()

    # --- named by RTK_MCP_RTK_PATH ------------------------------------------------------------------
    custom = tempfile.mkdtemp(prefix="rtk-mcp-custom-")
    try:
        copy = os.path.join(custom, RTK_EXE)
        shutil.copy2(real, copy)
        s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_RTK_PATH": copy})
        try:
            err, text = s.call("get_rtk_status")
            check("RTK_MCP_RTK_PATH names the RTK to use",
                  not err and "set by RTK_MCP_RTK_PATH" in text and custom in text, text)
            ls_works_through_rtk(s, "RTK_MCP_RTK_PATH")
        finally:
            s.close()

        # a setting that names nothing is reported, and the search goes on to PATH
        missing = os.path.join(custom, "missing", RTK_EXE)
        s = Server(extra_env={"RTK_MCP_RTK_PATH": missing})
        try:
            err, text = s.call("get_rtk_status")
            check("a wrong RTK_MCP_RTK_PATH is reported and the search goes on",
                  not err and "RTK: rtk " in text and "found on PATH" in text
                  and "Skipped: RTK_MCP_RTK_PATH=" in text, text)
            check("the startup log warns about it", "RTK_MCP_RTK_PATH=" in s.log(), s.log()[-300:])
        finally:
            s.close()
    finally:
        shutil.rmtree(custom, ignore_errors=True)

    # --- RTK appears after the server has started ------------------------------------------------
    s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_RTK_VERSION": "pinned"})
    try:
        err, text = s.call("get_rtk_status")
        check("before RTK exists, the status says so", not err and "RTK: not installed" in text, text)
        target = os.path.join(s.home, ".local", "bin", RTK_EXE)
        os.makedirs(os.path.dirname(target))
        shutil.copy2(real, target)
        if rtk_version() >= pinned_version():
            err, text = s.call("install_rtk")
            check("install_rtk finds an RTK that was put there by hand, and installs nothing",
                  not err and "already installed" in text and "usual install directory" in text, text)
        else:
            # an RTK older than the pinned release would be upgraded by install_rtk; looking is enough here
            err, text = s.call("get_rtk_status")
            check("get_rtk_status finds an RTK that was put there by hand",
                  not err and "found in a usual install directory" in text, text)
        ls_works_through_rtk(s, "added while running")
    finally:
        s.close()

    # --- a program called rtk that is not RTK comes first on PATH -------------------------------------
    if not IS_WINDOWS:
        shadow = tempfile.mkdtemp(prefix="rtk-mcp-shadow-")
        try:
            fake = os.path.join(shadow, "rtk")
            with open(fake, "w") as f:
                f.write("#!/bin/sh\necho 'another tool 9.9'\n")
            os.chmod(fake, 0o755)
            s = Server(path=shadow + os.pathsep + os.environ.get("PATH", "/usr/bin:/bin"))
            try:
                err, text = s.call("get_rtk_status")
                check("a different program named rtk is skipped, the real RTK is used",
                      not err and "RTK: rtk " in text and "is not a working RTK" in text, text)
                ls_works_through_rtk(s, "shadowed")
            finally:
                s.close()
        finally:
            shutil.rmtree(shadow, ignore_errors=True)


def network_enabled(version):
    """Do the tests that download from GitHub run?

    RTK_MCP_TEST_NETWORK=1 asks for them and =0 rules them out. Left unset they run on GitHub
    Actions in the jobs whose RTK is the release install_rtk pins, which gives every system one real
    install (and the 0.34 and 0.49 jobs have nothing to add).
    """
    if NETWORK_SETTING in ("1", "true"):
        return True
    if NETWORK_SETTING in ("0", "false"):
        return False
    return os.environ.get("GITHUB_ACTIONS") == "true" and version == pinned_version()


def test_pinned_checksums():
    """The hashes install_rtk trusts are compiled into the server, so they have to be the ones RTK
    published for the release it installs, as published now (a re-uploaded asset changes them)."""
    tag = tool_version()
    curl = shutil.which("curl")
    if not curl:
        skip("pinned checksums against GitHub", "curl is not installed")
        return
    here = os.path.dirname(os.path.abspath(__file__))
    with open(os.path.join(here, "..", "data", "rtk-checksums-%s.txt" % tag), "rb") as f:
        pinned = f.read().replace(b"\r\n", b"\n")
    url = "https://github.com/rtk-ai/rtk/releases/download/%s/checksums.txt" % tag
    got = subprocess.run([curl, "-fsSL", "--max-time", "60", url], capture_output=True, timeout=90)
    published = got.stdout.replace(b"\r\n", b"\n")
    check("data/rtk-checksums-%s.txt is the checksums.txt RTK published" % tag,
          got.returncode == 0 and published == pinned,
          "curl exit %s; %d bytes published, %d pinned" % (got.returncode, len(published), len(pinned)))


def test_install_network(version):
    """The real thing: download RTK from GitHub, check it against the hash pinned in the server,
    put it in a throwaway directory, and run commands through it."""
    if not network_enabled(version):
        skip("install_rtk against GitHub", "set RTK_MCP_TEST_NETWORK=1 to download RTK")
        return
    test_pinned_checksums()
    if rtk_version(BASIC_PATH) is not None or usual_rtk_places():
        skip("install_rtk against GitHub", "rtk is installed in a directory the server always looks in")
        return
    tag = tool_version()
    number = tag.lstrip("v")
    root = tempfile.mkdtemp(prefix="rtk-mcp-install-")
    bindir = os.path.join(root, "bin")
    exe = os.path.join(bindir, RTK_EXE)
    s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_INSTALL_DIR": bindir, "RTK_MCP_RTK_VERSION": "pinned"})
    try:
        started = time.time()
        err, text = s.call("install_rtk", _timeout=240)
        print("      (install took %.1fs)" % (time.time() - started))
        check("install_rtk installs the pinned release",
              not err and text.startswith("Installed rtk %s at " % number) and "pinned in rtk-mcp" in text, text)
        check("the executable is in the install directory", os.path.isfile(exe), os.listdir(root))
        leftovers = [n for n in os.listdir(bindir) if n != RTK_EXE]
        check("nothing else was written there (no licence files, no temporary file)", leftovers == [], leftovers)
        if not IS_WINDOWS:
            check("it is executable", os.access(exe, os.X_OK))
        out = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=30).stdout
        check("the file is a working RTK of the pinned version", out.strip() == "rtk " + number, out)

        err, text = s.call("get_rtk_status")
        check("status shows the installed RTK", not err and ("RTK: rtk %s at %s" % (number, exe)) in text, text)
        with open(os.path.join(s.project, "alpha.txt"), "w") as f:
            f.write("alpha")
        err, text = s.call("run_command", command="ls", cwd=s.project)
        check("commands run through the installed RTK without a restart",
              not err and "alpha.txt" in text and "not found" not in text.lower(), text)
        err, text = s.call("run_command", command="rtk --version")
        check("`rtk` resolves by name for the commands it runs (its directory is not on PATH)",
              not err and text.strip() == "rtk " + number, text)
        err, text = s.call("get_rtk_savings", format="json")
        check("the stats tools use the installed RTK", not err and '"summary"' in text, text)
        err, text = s.call("install_rtk")
        check("a second install_rtk finds it already installed and up to date",
              not err and "already installed and up to date" in text, text)
        err, text = s.call("run_command", command="echo after install")
        check("no 'RTK is not installed' note once RTK is there",
              not err and "after install" in text and "RTK is not installed" not in text, text)
    finally:
        s.close()

    # a server started later finds it again, in the install directory
    s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_INSTALL_DIR": bindir})
    try:
        err, text = s.call("get_rtk_status")
        check("a later server finds the installed RTK in the install directory",
              not err and "found in a usual install directory" in text and exe in text, text)
    finally:
        s.close()
        shutil.rmtree(root, ignore_errors=True)

    # RTK_MCP_AUTO_INSTALL: installed in the background at startup, the Apache-2.0 build on request
    root = tempfile.mkdtemp(prefix="rtk-mcp-auto-")
    bindir = os.path.join(root, "bin")
    exe = os.path.join(bindir, RTK_EXE)
    s = Server(path=BASIC_PATH, extra_env={
        "RTK_MCP_INSTALL_DIR": bindir, "RTK_MCP_AUTO_INSTALL": "1", "RTK_MCP_RTK_EDITION": "oss",
        "RTK_MCP_RTK_VERSION": "pinned"})
    try:
        text = ""
        deadline = time.time() + 180
        while time.time() < deadline:
            err, text = s.call("get_rtk_status")
            if "RTK: rtk " in text:
                break
            time.sleep(2)
        check("RTK_MCP_AUTO_INSTALL installs RTK in the background", "RTK: rtk %s at " % number in text and os.path.isfile(exe), text)
        check("the log says so", "installed rtk %s" % number in s.log(), s.log()[-400:])
        err, text = s.call("run_command", command="rtk --version")
        check("the oss build (RTK_MCP_RTK_EDITION=oss) runs commands too", not err and text.startswith("rtk "), text)
    finally:
        s.close()
        shutil.rmtree(root, ignore_errors=True)


def sha256_of(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def version_of(path):
    """The (major, minor, patch) that the RTK at `path` reports, or None."""
    try:
        out = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=30).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    numbers = re.findall(r"\d+", out)
    return tuple(int(n) for n in numbers[:3]) if len(numbers) >= 3 else None


def write_fake_rtk(path, version_line="rtk 0.9.0"):
    """A program that passes the server's probe as an RTK reporting `version_line`: `rewrite` answers
    the way RTK does for a command it has a filter for (`git status`) and has no answer for any other.
    A shell script, so not for Windows."""
    script = (
        "#!/bin/sh\n"
        'case "$1" in\n'
        "  --version) echo '" + version_line + "' ;;\n"
        '  rewrite) if [ "$3" = "git status" ]; then echo \'rtk git status\'; exit 3; fi; exit 1 ;;\n'
        "  *) exit 1 ;;\n"
        "esac\n"
    )
    with open(path, "w") as f:
        f.write(script)
    os.chmod(path, 0o755)


def test_install_offline():
    """install_rtk when GitHub cannot be reached or there is nothing to do: nothing is downloaded, so
    these run everywhere. A fake RTK (a shell script that passes the server's probe) stands in for an
    old one, so most of them need a POSIX shell."""
    # --- a setting that makes no sense --------------------------------------------------------------
    s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_RTK_VERSION": "nonsense"})
    try:
        err, text = s.call("install_rtk")
        check("RTK_MCP_RTK_VERSION=nonsense is refused, naming the two values that work",
              err and "RTK_MCP_RTK_VERSION=nonsense" in text and "`latest`" in text and "`pinned`" in text, text)
        err, text = s.call("get_rtk_status")
        check("the status reports the bad setting",
              not err and "Installer: unavailable" in text and "RTK_MCP_RTK_VERSION=nonsense" in text, text)
    finally:
        s.close()

    # --- GitHub cannot be reached and there is no RTK -----------------------------------------------
    if rtk_version(BASIC_PATH) is None and not usual_rtk_places():
        root = tempfile.mkdtemp(prefix="rtk-mcp-offline-")
        bindir = os.path.join(root, "bin")
        s = Server(path=BASIC_PATH, extra_env=dict(DEAD_PROXY, RTK_MCP_INSTALL_DIR=bindir))
        try:
            err, text = s.call("install_rtk", _timeout=120)
            check("install_rtk without GitHub says what failed (it fell back on the pinned release) and installs nothing",
                  err and ("downloading https://github.com/rtk-ai/rtk/releases/download/%s/" % tool_version()) in text
                  and not os.path.exists(os.path.join(bindir, RTK_EXE)), text)
            err, text = s.call("run_command", command="echo carries on")
            check("the server carries on", not err and "carries on" in text, text)
        finally:
            s.close()
            shutil.rmtree(root, ignore_errors=True)
    else:
        skip("install_rtk without GitHub and without RTK", "rtk is installed in a directory the server always looks in")

    if IS_WINDOWS:
        skip("install_rtk with a fake old RTK", "the fake is a shell script")
        return

    # --- fake RTKs of different ages, GitHub unreachable ------------------------------------------------
    root = tempfile.mkdtemp(prefix="rtk-mcp-fake-")

    def fake(name, version_line):
        directory = os.path.join(root, name)
        os.makedirs(directory)
        path = os.path.join(directory, "rtk")
        write_fake_rtk(path, version_line)
        return path

    def server(path):
        return Server(path=BASIC_PATH, extra_env=dict(DEAD_PROXY, RTK_MCP_RTK_PATH=path))

    try:
        newer = fake("newer", "rtk 99.0.0")
        before = sha256_of(newer)
        s = server(newer)
        try:
            err, text = s.call("install_rtk", _timeout=120)
            check("an RTK newer than the release is left alone, and an unreachable GitHub is not an error",
                  not err and "already installed and up to date" in text and "rtk 99.0.0" in text
                  and "GitHub could not be asked for the latest" in text, text)
            check("... its file is untouched", sha256_of(newer) == before)
            err, text = s.call("get_rtk_status")
            check("the status does not call it out of date", not err and "Update:" not in text, text)
        finally:
            s.close()

        old = fake("old", "rtk 0.9.0")
        before = sha256_of(old)
        s = server(old)
        try:
            err, text = s.call("get_rtk_status")
            check("the status calls an old RTK out of date and names the tool that fixes it",
                  not err and ("Update: RTK 0.9.0 is older than %s" % tool_version().lstrip("v")) in text
                  and "call install_rtk to upgrade it" in text, text)
            err, text = s.call("install_rtk", _timeout=120)
            check("a download that fails leaves the old RTK as it was",
                  err and "downloading" in text and sha256_of(old) == before, text)
            check("... and no temporary file beside it", os.listdir(os.path.dirname(old)) == ["rtk"],
                  os.listdir(os.path.dirname(old)))
            err, text = s.call("run_command", command="echo still here")
            check("... and the server carries on", not err and "still here" in text, text)
        finally:
            s.close()

        behind = fake("behind-the-link", "rtk 0.9.0")
        linkdir = os.path.join(root, "bin")
        os.makedirs(linkdir)
        link = os.path.join(linkdir, "rtk")
        os.symlink(behind, link)
        before = sha256_of(behind)
        s = server(link)
        try:
            err, text = s.call("install_rtk", _timeout=120)
            check("an old RTK behind a symbolic link is reported, not replaced",
                  err and text.startswith("RTK was not upgraded.") and "is a symbolic link" in text
                  and "package manager" in text, text)
            check("... the link and the file behind it are as they were",
                  os.path.islink(link) and sha256_of(behind) == before and os.listdir(linkdir) == ["rtk"])
        finally:
            s.close()

        strange = fake("strange", "rtk dev-build")
        s = server(strange)
        try:
            err, text = s.call("install_rtk", _timeout=120)
            check("an RTK whose version cannot be read is left alone",
                  err and text.startswith("RTK was not upgraded.") and "cannot be compared" in text, text)
        finally:
            s.close()

        if hasattr(os, "geteuid") and os.geteuid() != 0:
            locked = fake("locked", "rtk 0.9.0")
            os.chmod(os.path.dirname(locked), 0o555)
            s = server(locked)
            try:
                err, text = s.call("install_rtk", _timeout=120)
                check("an old RTK in a directory that cannot be written to is reported, not replaced",
                      err and text.startswith("RTK was not upgraded.") and "cannot write in" in text, text)
            finally:
                s.close()
                os.chmod(os.path.dirname(locked), 0o755)
        else:
            skip("an old RTK in a read-only directory", "running as root, which can write anywhere")
    finally:
        for directory, _, _ in os.walk(root):
            os.chmod(directory, 0o755)
        shutil.rmtree(root, ignore_errors=True)


def upgrade_network_enabled():
    """Do the tests that upgrade an RTK from GitHub run? RTK_MCP_TEST_NETWORK=1 asks for them, =0
    rules them out, and on GitHub Actions they run in every job."""
    if NETWORK_SETTING in ("1", "true"):
        return True
    if NETWORK_SETTING in ("0", "false"):
        return False
    return os.environ.get("GITHUB_ACTIONS") == "true"


def test_upgrade_network(version):
    """The real thing, upgrading: an old RTK is replaced in place by the latest release from GitHub.

    The old RTK is a copy of this runner's own where that one is older than the pinned release (every
    system gets this from the 0.34 and 0.49 jobs), else a fake one."""
    if not upgrade_network_enabled():
        skip("upgrading RTK from GitHub", "set RTK_MCP_TEST_NETWORK=1 to download RTK")
        return
    pinned = pinned_version()
    latest = latest_release_online()
    if latest is None and os.environ.get("GITHUB_ACTIONS") == "true":
        # a runner can always ask, so this is worth a failure rather than a quiet fallback
        check("GitHub names the latest RTK release (releases/latest redirects to a tag)", False,
              "the runner could not read it")
        return
    expected = latest or pinned
    asked = "the latest release" if latest else "GitHub could not be asked for the latest"
    if latest is None:
        skip("upgrading to the latest release",
             "GitHub's releases/latest page cannot be read from here, so the pinned release is expected")
    runner_rtk = shutil.which("rtk")
    root = tempfile.mkdtemp(prefix="rtk-mcp-upgrade-")
    try:
        directory = os.path.join(root, "old")
        os.makedirs(directory)
        old = os.path.join(directory, RTK_EXE)
        fake = False
        if version is not None and version < pinned and runner_rtk:
            shutil.copy2(runner_rtk, old)
            old_number = ".".join(map(str, version))
            label = "a copy of RTK " + old_number
            untouched = (runner_rtk, sha256_of(runner_rtk))
        elif not IS_WINDOWS:
            write_fake_rtk(old, "rtk 0.9.0")
            old_number, label, fake, untouched = "0.9.0", "a fake RTK 0.9.0", True, None
        else:
            skip("upgrading RTK from GitHub", "this runner's RTK is the pinned release, and a fake needs a shell script")
            return

        s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_RTK_PATH": old})
        try:
            err, text = s.call("get_rtk_status")
            check(label + ": the status calls it out of date",
                  not err and ("Update: RTK %s is older than " % old_number) in text, text)
            started = time.time()
            err, text = s.call("install_rtk", _timeout=240)
            print("      (upgrade took %.1fs)" % (time.time() - started))
            check(label + ": install_rtk upgrades it in place",
                  not err and text.startswith("Upgraded RTK from %s to " % old_number) and old in text, text)
            new = version_of(old)
            check("the file now holds the latest RTK release" if latest else "the file now holds the pinned RTK release",
                  new == expected, (new, expected))
            check("nothing else was written beside it (no staged file, no licence files)",
                  os.listdir(directory) == [RTK_EXE], os.listdir(directory))
            if not IS_WINDOWS:
                check("it is executable", os.access(old, os.X_OK))
            if new is None:
                return
            err, text = s.call("get_rtk_status")
            check("the status shows the new version at the same place, no longer out of date",
                  not err and ("RTK: rtk %d.%d.%d at %s" % (new + (old,))) in text and "Update:" not in text, text)
            err, text = s.call("run_command", command="rtk --version")
            check("commands run through the new RTK without a restart",
                  not err and text.strip() == "rtk %d.%d.%d" % new, text)
            err, text = s.call("install_rtk", _timeout=120)
            check("a second install_rtk finds it up to date",
                  not err and "already installed and up to date" in text and asked in text, text)
        finally:
            s.close()
        if untouched:
            check("the RTK this test runs against was not touched", sha256_of(untouched[0]) == untouched[1])

        if fake:
            # RTK_MCP_AUTO_INSTALL does the same at startup, with nobody asking
            auto = os.path.join(root, "auto")
            os.makedirs(auto)
            old = os.path.join(auto, "rtk")
            write_fake_rtk(old, "rtk 0.9.0")
            s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_RTK_PATH": old, "RTK_MCP_AUTO_INSTALL": "1"})
            try:
                text = ""
                deadline = time.time() + 180
                while time.time() < deadline:
                    err, text = s.call("get_rtk_status")
                    if "RTK: rtk 0.9.0" not in text:
                        break
                    time.sleep(2)
                new = version_of(old)
                check("RTK_MCP_AUTO_INSTALL upgrades an old RTK at startup",
                      new == expected and ("RTK: rtk %d.%d.%d at %s" % (new + (old,))) in text, text)
                check("the log says so", "upgraded rtk 0.9.0 to rtk " in s.log(), s.log()[-400:])
            finally:
                s.close()
    finally:
        shutil.rmtree(root, ignore_errors=True)


def test_install_latest_network(version):
    """The default channel against GitHub: with no RTK on the machine, the latest release is installed,
    with the hash from its own checksums.txt (or the pinned one, when the latest is the pinned release)."""
    if not network_enabled(version):
        skip("install_rtk of the latest release", "set RTK_MCP_TEST_NETWORK=1 to download RTK")
        return
    if rtk_version(BASIC_PATH) is not None or usual_rtk_places():
        skip("install_rtk of the latest release", "rtk is installed in a directory the server always looks in")
        return
    pinned = pinned_version()
    latest = latest_release_online()
    if latest is None and os.environ.get("GITHUB_ACTIONS") == "true":
        check("GitHub names the latest RTK release (releases/latest redirects to a tag)", False,
              "the runner could not read it")
        return
    expected = latest or pinned
    asked = "the latest release" if latest else "GitHub could not be asked for the latest"
    if latest is None:
        skip("install_rtk of the latest release",
             "GitHub's releases/latest page cannot be read from here, so the pinned release is expected")
    root = tempfile.mkdtemp(prefix="rtk-mcp-latest-")
    bindir = os.path.join(root, "bin")
    exe = os.path.join(bindir, RTK_EXE)
    s = Server(path=BASIC_PATH, extra_env={"RTK_MCP_INSTALL_DIR": bindir})
    try:
        err, text = s.call("get_rtk_status")
        check("the status says the installer goes for the latest release",
              not err and "Installer: install_rtk installs the latest RTK release (" in text, text)
        err, text = s.call("install_rtk", _timeout=240)
        found = re.match(r"Installed rtk (\d+)\.(\d+)\.(\d+) at ", text)
        number = tuple(int(n) for n in found.groups()) if found else None
        check("install_rtk installs the latest release" if latest else "install_rtk falls back on the pinned release",
              not err and number == expected and asked in text, text)
        check("... checked against a hash from rtk-mcp or from the release's own checksums.txt",
              ("pinned in rtk-mcp" in text) if expected == pinned else ("checksums.txt published with v" in text), text)
        check("the file is a working RTK of that version", number is not None and version_of(exe) == number,
              (number, version_of(exe) if os.path.isfile(exe) else None))
        check("nothing else was written beside it", os.path.isdir(bindir) and os.listdir(bindir) == [RTK_EXE],
              os.listdir(bindir) if os.path.isdir(bindir) else "no directory")
        err, text = s.call("run_command", command="rtk --version")
        check("commands run through it without a restart",
              not err and number is not None and text.strip() == "rtk %d.%d.%d" % number, text)
        err, text = s.call("install_rtk", _timeout=120)
        check("a second install_rtk finds it up to date",
              not err and "already installed and up to date" in text and asked in text, text)
    finally:
        s.close()
        shutil.rmtree(root, ignore_errors=True)



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
            # without RTK the first result ends with the note about install_rtk
            check("no Git, no RTK: echo works", not err and text.strip().startswith("pipe-ok"), text)
            check("no Git, no RTK: the first result says RTK is missing and names install_rtk",
                  "RTK is not installed" in text and "install_rtk" in text, text)
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
        test_rtk_locations()
        test_install_network(version)
        test_install_latest_network(version)
        test_install_offline()
        test_upgrade_network(version)
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
