"""The vacuum probe: tries, from inside an agent's container, every way out of it we know of.

`eidra eval vacuum-check` copies this in and runs it with python3. Each check prints one line and
the script exits 0 only when every one of them held: the model socket worked, and nothing else
did. Standard library only, so that it runs in whatever image a task uses.

Checks that need a tool the image does not have are skipped, and a skip is not a failure: the tool
being absent is one reason the way out is closed. The hole is the tool working.
"""

import json
import os
import shutil
import socket
import stat
import subprocess
import sys

TARGET = os.environ.get("EIDRA_PROBE_TARGET", "")  # the model server, host:port
MODEL_SOCKET = os.environ.get("EIDRA_MODEL_SOCKET", "")
API_PATH = os.environ.get("EIDRA_PROBE_API_PATH", "/v1")
# Addresses that answer from anywhere with a network, so reaching one means the vacuum leaks.
OUTSIDE = [("1.1.1.1", 443), ("8.8.8.8", 53), ("93.184.216.34", 80)]
CONNECT_TIMEOUT = 2.0

results = []


def check(name, ok, detail, skipped=False):
    results.append({"name": name, "ok": bool(ok), "skipped": skipped, "detail": detail})
    mark = "skip" if skipped else ("ok" if ok else "FAIL")
    print(f"{mark:>4}  {name}: {detail}", flush=True)


def run(command, timeout=20):
    """(exit code, output) of `command`, or (None, reason) if it could not run at all."""
    try:
        done = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except FileNotFoundError:
        return None, f"{command[0]} is not installed"
    except subprocess.TimeoutExpired:
        return None, f"{command[0]} did not finish in {timeout}s"
    return done.returncode, (done.stdout + done.stderr).strip()


def over_socket(request, timeout=20.0):
    """Sends a raw HTTP request over the model socket and returns the reply as text."""
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(timeout)
    try:
        conn.connect(MODEL_SOCKET)
        conn.sendall(request.encode())
        chunks = []
        while True:
            chunk = conn.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
            if len(b"".join(chunks)) > 1 << 20:
                break
    finally:
        conn.close()
    return b"".join(chunks).decode("utf-8", "replace")


# 1. The one channel that is meant to work. A vacuum that cut this off would make every check
#    below pass for the wrong reason, so this is the probe's positive control.
models_reply = ""
if not MODEL_SOCKET:
    check("model-socket", False, "EIDRA_MODEL_SOCKET is not set: the agent cannot reach the model")
else:
    try:
        models_reply = over_socket(
            f"GET {API_PATH}/models HTTP/1.1\r\nHost: model\r\nConnection: close\r\n\r\n"
        )
        status = models_reply.split("\r\n", 1)[0]
        check("model-socket", status.startswith("HTTP/"), f"the model answers through it: {status}")
    except OSError as error:
        check("model-socket", False, f"{MODEL_SOCKET}: {error}")

# 2. The socket is a pipe to one server, not a proxy. A request naming another host must still be
#    answered by the model server, not forwarded to that host.
if models_reply:
    try:
        foreign = over_socket(
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        )
        leaked = "Example Domain" in foreign or "<title>Example" in foreign
        status = foreign.split("\r\n", 1)[0] if foreign else "nothing"
        check(
            "relay-one-target",
            not leaked,
            f"a request for example.com gets {status} from the model server",
        )
    except OSError as error:
        check("relay-one-target", True, f"a request for another host got nowhere: {error}")

# 3. No DNS: nothing can be looked up.
resolved = []
for name in ("example.com", "pypi.org", "github.com", "radiance"):
    try:
        socket.getaddrinfo(name, 80, proto=socket.IPPROTO_TCP)
        resolved.append(name)
    except OSError:
        pass
check("dns", not resolved, "nothing resolves" if not resolved else f"resolved {resolved}")


def connects(host, port):
    try:
        with socket.create_connection((host, port), CONNECT_TIMEOUT):
            return True
    except OSError:
        return False


# 4. The model server's own port, over the network. Mounted socket aside, it has to be out of
#    reach: this is the check that fails first if an agent ever gets the host's network.
if TARGET:
    host, _, port = TARGET.rpartition(":")
    host = host.strip("[]")
    reached = connects(host, int(port))
    check("model-port", not reached, f"tcp {TARGET} is {'OPEN' if reached else 'unreachable'}")

# 5. The internet.
reached = [f"{host}:{port}" for host, port in OUTSIDE if connects(host, port)]
check("internet", not reached, "no address answers" if not reached else f"reached {reached}")

# 6. UDP gets no further than TCP, DNS included.
answered = []
for host, port in (("8.8.8.8", 53), ("1.1.1.1", 53)):
    query = b"\xaa\xbb\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01"
    conn = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    conn.settimeout(CONNECT_TIMEOUT)
    try:
        conn.sendto(query, (host, port))
        conn.recvfrom(4096)
        answered.append(f"{host}:{port}")
    except OSError:
        pass
    finally:
        conn.close()
check("udp", not answered, "no reply" if not answered else f"{answered} replied")

# 7. The namespace itself: loopback only, and no route off the machine.
interfaces = []
with open("/proc/net/dev", encoding="utf-8") as handle:
    for line in handle.read().splitlines()[2:]:
        name = line.split(":", 1)[0].strip()
        if name:
            interfaces.append(name)
check(
    "interfaces",
    interfaces == ["lo"],
    f"{interfaces}" + ("" if interfaces == ["lo"] else " besides loopback"),
)

routes = []
with open("/proc/net/route", encoding="utf-8") as handle:
    for line in handle.read().splitlines()[1:]:
        fields = line.split()
        if len(fields) > 2 and fields[1] == "00000000":  # a default route
            routes.append(fields[0])
check("routes", not routes, "no default route" if not routes else f"a default route via {routes}")

# 8. Nothing may change that. Making an interface or a route needs CAP_NET_ADMIN, and entering
#    another network namespace needs CAP_SYS_ADMIN; neither is in a container's default set.
caps = 0
with open("/proc/self/status", encoding="utf-8") as handle:
    for line in handle.read().splitlines():
        if line.startswith("CapEff:"):
            caps = int(line.split()[1], 16)
held = [name for name, bit in (("CAP_NET_ADMIN", 12), ("CAP_SYS_ADMIN", 21)) if caps >> bit & 1]
check("capabilities", not held, f"CapEff={caps:016x}" + (f", holds {held}" if held else ""))

if shutil.which("ip"):
    code, text = run(["ip", "addr", "add", "10.254.254.254/24", "dev", "lo"])
    check("ip-addr-add", code != 0, f"ip addr add exits {code}: {text.splitlines()[:1]}")
else:
    check("ip-addr-add", True, "ip is not installed", skipped=True)

# 9. A new network namespace of its own would be empty, but unsharing one is also how a process
#    reaches for CAP_NET_ADMIN, so it must fail.
if hasattr(os, "unshare"):
    try:
        pid = os.fork()
        if pid == 0:  # the child: it must not come back from this
            try:
                os.unshare(os.CLONE_NEWNET)
                os._exit(0)
            except OSError:
                os._exit(1)
        _, status = os.waitpid(pid, 0)
        check("unshare", os.waitstatus_to_exitcode(status) != 0, "CLONE_NEWNET is refused")
    except OSError as error:
        check("unshare", True, f"fork failed: {error}")
else:
    check("unshare", True, "python has no os.unshare", skipped=True)

for tool, command in (
    ("unshare", ["unshare", "-n", "true"]),
    ("nsenter", ["nsenter", "--net=/proc/1/ns/net", "true"]),
):
    if shutil.which(tool):
        code, text = run(command)
        check(f"{tool}-cli", code != 0, f"{tool} exits {code}: {text.splitlines()[:1]}")
    else:
        check(f"{tool}-cli", True, f"{tool} is not installed", skipped=True)

# 10. Docker would hand over the whole host, so neither its socket nor its CLI may be in here.
sockets = [
    path
    for path in ("/var/run/docker.sock", "/run/docker.sock", "/run/podman/podman.sock")
    if os.path.exists(path)
]
docker_host = os.environ.get("DOCKER_HOST", "")
if sockets or docker_host:
    check("docker-socket", False, f"sockets {sockets}, DOCKER_HOST={docker_host!r}")
else:
    check("docker-socket", True, "no container socket is mounted, and no DOCKER_HOST")

if shutil.which("docker"):
    code, text = run(["docker", "info"])
    check("docker-cli", code != 0, f"docker info exits {code}: {text.splitlines()[:1]}")
else:
    check("docker-cli", True, "docker is not installed", skipped=True)

# 11. The agent's own configuration is no way out either: eidra takes a --base-url, but a URL is
#     useless without a route to it. Run without the socket, so that it has to use the network.
if os.path.exists("/usr/local/bin/eidra") and TARGET:
    env = {key: value for key, value in os.environ.items() if key != "EIDRA_MODEL_SOCKET"}
    try:
        done = subprocess.run(
            [
                "/usr/local/bin/eidra",
                "exec",
                "--base-url",
                f"http://{TARGET}{API_PATH}",
                "--max-requests",
                "1",
                "--session-dir",
                "/tmp/vacuum-probe-sessions",
                "say hello",
            ],
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        reply = (done.stdout + done.stderr).strip().splitlines()[-1:]
        check("eidra-base-url", done.returncode != 0, f"exits {done.returncode}: {reply}")
    except subprocess.TimeoutExpired:
        check("eidra-base-url", True, "it never got an answer")
else:
    check("eidra-base-url", True, "no eidra binary or target to point it at", skipped=True)

# 12. An agent could also just ask the relay to talk to the model for it; that is allowed. What is
#     not is the socket being writable as a path other processes on the host can be aimed at: a
#     socket is fine, a regular file or a directory in its place would mean something else.
if MODEL_SOCKET and os.path.exists(MODEL_SOCKET):
    mode = os.stat(MODEL_SOCKET).st_mode
    check("socket-is-a-socket", stat.S_ISSOCK(mode), f"mode {stat.filemode(mode)}")

failed = [r["name"] for r in results if not r["ok"]]
skipped = [r["name"] for r in results if r["skipped"]]
print()
print(
    f"summary: {len(results) - len(failed)} ok, {len(failed)} failed, {len(skipped)} skipped"
    + (f" · failed: {', '.join(failed)}" if failed else "")
)
if os.environ.get("EIDRA_PROBE_JSON"):
    with open(os.environ["EIDRA_PROBE_JSON"], "w", encoding="utf-8") as handle:
        json.dump(results, handle, indent=2)
sys.exit(1 if failed else 0)
