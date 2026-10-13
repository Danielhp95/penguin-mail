#!/usr/bin/env python3
"""The measuring half of scripts/measure.sh. Run that script, not this one:
it builds the app, makes the sandbox, starts the hidden display and the
session bus, and takes everything down afterwards. Its header says what
each number means.

This half launches the app, watches the X server for its first frame,
reads memory and CPU from /proc, seeds a Dovecot container with generated
mail, adds an IMAP account for it to a throwaway store, and drives the
window through AT-SPI and XTest. It prints a JSON report on stdout and its
progress on stderr.
"""

import argparse
import ctypes
import email.utils
import imaplib
import json
import os
import random
import select
import signal
import sqlite3
import ssl
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

DOVECOT_IMAGE = (
    "dovecot/dovecot:2.4.5@sha256:"
    "c807be4fb5a97d9c3a90770569d3a6c4cbdcb36742ad41f90409cbd929166553"
)
LABEL = "io.github.c9dev.penguin-mail.measure"
ADDRESS = "me@example.test"
TICKS = os.sysconf("SC_CLK_TCK")


def log(text):
    print(f"[measure {time.strftime('%H:%M:%S')}] {text}", file=sys.stderr, flush=True)


# The X server: when the window maps, and when it first shows more than the
# blank background the server paints on map.

class Display:
    SUBSTRUCTURE_NOTIFY = 1 << 19
    CREATE_NOTIFY = 16
    MAP_NOTIFY = 19
    REPORT_RAW = 0

    def __init__(self):
        self.x11 = ctypes.CDLL("libX11.so.6")
        self.damage = ctypes.CDLL("libXdamage.so.1")
        self.xtst = ctypes.CDLL("libXtst.so.6")
        x, d = self.x11, self.damage
        vp, ul = ctypes.c_void_p, ctypes.c_ulong
        x.XOpenDisplay.restype = vp
        x.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x.XDefaultRootWindow.restype = ul
        x.XDefaultRootWindow.argtypes = [vp]
        x.XSelectInput.argtypes = [vp, ul, ctypes.c_long]
        x.XPending.argtypes = [vp]
        x.XNextEvent.argtypes = [vp, vp]
        x.XConnectionNumber.argtypes = [vp]
        x.XFlush.argtypes = [vp]
        x.XSync.argtypes = [vp, ctypes.c_int]
        x.XGetImage.restype = vp
        x.XGetImage.argtypes = [vp, ul, ctypes.c_int, ctypes.c_int, ctypes.c_uint,
                                ctypes.c_uint, ul, ctypes.c_int]
        x.XDestroyImage.argtypes = [vp]
        x.XGetGeometry.argtypes = [vp, ul] + [vp] * 7
        x.XKeysymToKeycode.argtypes = [vp, ul]
        d.XDamageQueryExtension.argtypes = [vp, vp, vp]
        d.XDamageCreate.restype = ul
        d.XDamageCreate.argtypes = [vp, ul, ctypes.c_int]
        self.xtst.XTestFakeMotionEvent.argtypes = [vp, ctypes.c_int, ctypes.c_int, ctypes.c_int, ul]
        self.xtst.XTestFakeButtonEvent.argtypes = [vp, ctypes.c_uint, ctypes.c_int, ul]
        self.xtst.XTestFakeKeyEvent.argtypes = [vp, ctypes.c_uint, ctypes.c_int, ul]
        # A window can go away between its CreateNotify and the calls made
        # on it; Xlib's default handler would end this process for that.
        handler = ctypes.CFUNCTYPE(ctypes.c_int, vp, vp)
        self._ignore = handler(lambda _d, _e: 0)
        x.XSetErrorHandler(self._ignore)
        self.dpy = x.XOpenDisplay(None)
        if not self.dpy:
            sys.exit("measure.py: cannot open the hidden display")
        self.root = x.XDefaultRootWindow(self.dpy)
        base, error = ctypes.c_int(), ctypes.c_int()
        if not d.XDamageQueryExtension(self.dpy, ctypes.byref(base), ctypes.byref(error)):
            sys.exit("measure.py: the X server lacks the DAMAGE extension")
        self.damage_event = base.value
        x.XSelectInput(self.dpy, self.root, self.SUBSTRUCTURE_NOTIFY)
        x.XFlush(self.dpy)

    def drain(self):
        """Throws away events left from an earlier launch."""
        event = (ctypes.c_long * 24)()
        while self.x11.XPending(self.dpy):
            self.x11.XNextEvent(self.dpy, event)

    def size(self, window):
        root, x, y = ctypes.c_ulong(), ctypes.c_int(), ctypes.c_int()
        w, h, border, depth = ctypes.c_uint(), ctypes.c_uint(), ctypes.c_uint(), ctypes.c_uint()
        ok = self.x11.XGetGeometry(self.dpy, window, ctypes.byref(root), ctypes.byref(x),
                                   ctypes.byref(y), ctypes.byref(w), ctypes.byref(h),
                                   ctypes.byref(border), ctypes.byref(depth))
        return (w.value, h.value) if ok else (0, 0)

    def painted(self, window):
        """Whether the window shows anything but one flat colour."""
        width, height = self.size(window)
        if width < 2 or height < 2:
            return False
        image = self.x11.XGetImage(self.dpy, window, 0, 0, width, height, 0xFFFFFFFF, 2)
        if not image:
            return False
        try:
            data = ctypes.c_void_p.from_address(image + 16).value
            stride = ctypes.c_int.from_address(image + 44).value
            pixels = ctypes.string_at(data, stride * height)
        finally:
            self.x11.XDestroyImage(image)
        first = pixels[:4]
        return pixels != first * (len(pixels) // 4)

    def first_frame(self, proc, started, timeout=60.0):
        """Waits for the app's first painted frame and returns
        (mapped, painted) in milliseconds since `started`, or None."""
        event = (ctypes.c_long * 24)()
        fd = self.x11.XConnectionNumber(self.dpy)
        mapped_at, watched = None, set()
        last_check = 0.0
        while time.monotonic() - started < timeout:
            if proc.poll() is not None:
                return None
            while self.x11.XPending(self.dpy):
                self.x11.XNextEvent(self.dpy, event)
                raw = bytes(event)
                kind = int.from_bytes(raw[0:4], "little")
                now = time.monotonic()
                if kind == self.CREATE_NOTIFY:
                    window = int.from_bytes(raw[40:48], "little")
                    self.damage.XDamageCreate(self.dpy, window, self.REPORT_RAW)
                elif kind == self.MAP_NOTIFY:
                    window = int.from_bytes(raw[40:48], "little")
                    override = int.from_bytes(raw[48:52], "little")
                    if not override and min(self.size(window)) >= 200:
                        watched.add(window)
                        mapped_at = mapped_at or now
                elif kind == self.damage_event:
                    window = int.from_bytes(raw[32:40], "little")
                    # Damage comes in bursts of rectangles; one look at the
                    # pixels every 5 ms is enough.
                    if window in watched and now - last_check > 0.005:
                        last_check = now
                        if self.painted(window):
                            return ((mapped_at - started) * 1000, (now - started) * 1000)
            select.select([fd], [], [], 0.02)
        return None

    def click(self, x, y):
        self.xtst.XTestFakeMotionEvent(self.dpy, -1, int(x), int(y), 0)
        self.x11.XFlush(self.dpy)
        time.sleep(0.1)
        for pressed in (1, 0):
            self.xtst.XTestFakeButtonEvent(self.dpy, 1, pressed, 0)
            self.x11.XFlush(self.dpy)
            time.sleep(0.05)

    def pointer(self, x, y):
        self.xtst.XTestFakeMotionEvent(self.dpy, -1, int(x), int(y), 0)
        self.x11.XFlush(self.dpy)
        time.sleep(0.1)

    def wheel(self, button, notches):
        """Button 5 turns the wheel down a notch, 4 up."""
        for _ in range(notches):
            for pressed in (1, 0):
                self.xtst.XTestFakeButtonEvent(self.dpy, button, pressed, 0)
        self.x11.XFlush(self.dpy)

    def key(self, keysym):
        code = self.x11.XKeysymToKeycode(self.dpy, keysym)
        for pressed in (1, 0):
            self.xtst.XTestFakeKeyEvent(self.dpy, code, pressed, 0)
            self.x11.XFlush(self.dpy)
            time.sleep(0.03)


KEY_DOWN = 0xFF54


# /proc: memory and CPU of the app and every process under it (WebKit's
# web and network processes run as its descendants).

def parent_of(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            return int(f.read().rsplit(")", 1)[1].split()[1])
    except (OSError, IndexError, ValueError):
        return None


def tree(pid):
    children = {}
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            parent = parent_of(int(entry))
            if parent is not None:
                children.setdefault(parent, []).append(int(entry))
    found, todo = [], [pid]
    while todo:
        current = todo.pop()
        found.append(current)
        todo.extend(children.get(current, []))
    return found


def rollup(pid):
    """RSS and PSS in KiB from smaps_rollup, or None once the process is gone."""
    values = {}
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                parts = line.split()
                if len(parts) >= 3 and parts[0] in ("Rss:", "Pss:"):
                    values[parts[0][:-1]] = int(parts[1])
    except OSError:
        return None
    return values


def memory(pid):
    main = rollup(pid) or {}
    total_rss = total_pss = 0
    processes = 0
    for member in tree(pid):
        values = rollup(member)
        if values:
            processes += 1
            total_rss += values.get("Rss", 0)
            total_pss += values.get("Pss", 0)
    return {
        "rss_mib": round(main.get("Rss", 0) / 1024, 1),
        "pss_mib": round(main.get("Pss", 0) / 1024, 1),
        "tree_pss_mib": round(total_pss / 1024, 1),
        "tree_rss_mib": round(total_rss / 1024, 1),
        "processes": processes,
    }


def cpu_seconds(pid):
    total = 0
    for member in tree(pid):
        try:
            with open(f"/proc/{member}/stat") as f:
                fields = f.read().rsplit(")", 1)[1].split()
            total += int(fields[11]) + int(fields[12])
        except (OSError, IndexError, ValueError):
            pass
    return total / TICKS


def wait_quiet(pid, share=0.03, span=3.0, timeout=600.0):
    """Waits until the app and its children use under `share` of one core
    over `span` seconds. Returns the monotonic time that quiet stretch
    began, or None on timeout."""
    started = time.monotonic()
    window = []
    while time.monotonic() - started < timeout:
        window.append((time.monotonic(), cpu_seconds(pid)))
        while window and window[-1][0] - window[0][0] > span:
            window.pop(0)
        if len(window) >= 2 and window[-1][0] - window[0][0] >= span * 0.9:
            used = (window[-1][1] - window[0][1]) / (window[-1][0] - window[0][0])
            if used < share:
                return window[0][0]
        time.sleep(0.25)
    return None


# Profiles: a home of their own for each, so no run reads another's
# settings, and none reads the person's.

class Profile:
    def __init__(self, root):
        self.root = Path(root)
        self.home = self.root / "home"
        for sub in ("home", "data", "config", "cache", "tmp"):
            (self.root / sub).mkdir(parents=True, exist_ok=True)

    def env(self, extra=None):
        env = dict(os.environ)
        env.update({
            "HOME": str(self.home),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "TMPDIR": str(self.root / "tmp"),
        })
        env.update(extra or {})
        return env

    @property
    def store(self):
        return self.root / "data" / "penguin-mail" / "mailrs.db"

    def disk(self):
        def du(path):
            return sum(p.stat().st_size for p in Path(path).rglob("*") if p.is_file())
        store = sum(Path(f"{self.store}{suffix}").stat().st_size
                    for suffix in ("", "-wal", "-shm") if Path(f"{self.store}{suffix}").exists())
        return {
            "store_mib": round(store / 2**20, 1),
            "data_dir_mib": round(du(self.root / "data") / 2**20, 1),
            "cache_dir_mib": round(du(self.root / "cache") / 2**20, 1),
        }


class App:
    def __init__(self, binary, display, logs):
        self.binary = binary
        self.display = display
        self.logs = Path(logs)
        self.count = 0

    def launch(self, profile, args=(), extra=None):
        """Starts the app and waits for its first frame. Returns the
        process and the frame's times, or raises when it never paints."""
        self.count += 1
        out = open(self.logs / f"app-{self.count}.log", "w")
        self.display.drain()
        started = self.started = time.monotonic()
        proc = subprocess.Popen([self.binary, *args], env=profile.env(extra),
                                stdout=out, stderr=subprocess.STDOUT)
        frame = self.display.first_frame(proc, started)
        if frame is None:
            self.stop(proc)
            raise RuntimeError(f"the app showed no window; see {out.name}")
        return proc, {"mapped_ms": round(frame[0]), "painted_ms": round(frame[1])}

    @staticmethod
    def stop(proc):
        members = tree(proc.pid)
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        # WebKit's helpers end with the app; anything still here is ours.
        for member in members[1:]:
            try:
                os.kill(member, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.5)


def median(values):
    values = [v for v in values if v is not None]
    return round(statistics.median(values), 1) if values else None


def summary(runs, keys):
    return {key: {"median": median([r[key] for r in runs]),
                  "min": min(r[key] for r in runs),
                  "max": max(r[key] for r in runs)} for key in keys}


# Startup and memory at rest, for the demo and an empty profile.

def startup(app, sandbox, mode, runs):
    """One warm-up launch, then `runs` timed ones. The empty profile keeps
    the store the warm-up made, as a second launch on a computer would."""
    profile = Profile(sandbox / f"startup-{mode}")
    args = ["--demo"] if mode == "demo" else []
    results = []
    for index in range(runs + 1):
        proc, frame = app.launch(profile, args)
        time.sleep(2)
        app.stop(proc)
        if index:
            results.append(frame)
            log(f"startup {mode} run {index}: {frame}")
    return {"runs": results, **summary(results, ["mapped_ms", "painted_ms"])}


def at_rest(app, sandbox, mode, runs, idle, profile=None, extra=None):
    profile = profile or Profile(sandbox / f"rest-{mode}")
    args = ["--demo"] if mode == "demo" else []
    results = []
    for index in range(runs):
        proc, _ = app.launch(profile, args, extra)
        time.sleep(idle)
        result = memory(proc.pid)
        app.stop(proc)
        results.append(result)
        log(f"rest {mode} run {index + 1}: {result}")
    return {"runs": results, **summary(results, ["rss_mib", "pss_mib", "tree_pss_mib"])}


# The large mailbox: Dovecot in Docker, seeded over IMAP.

def run(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kwargs).stdout


def make_certs(where):
    where.mkdir(parents=True, exist_ok=True)
    (where / "server.ext").write_text(
        "subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n"
        "keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")
    steps = [
        ["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", "root.key", "-out", "root.crt", "-days", "2", "-subj", "/CN=measure root",
         "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign"],
        ["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", "tls.key", "-out", "tls.csr", "-subj", "/CN=localhost"],
        ["x509", "-req", "-in", "tls.csr", "-CA", "root.crt", "-CAkey", "root.key",
         "-CAcreateserial", "-out", "tls.crt", "-days", "2", "-extfile", "server.ext"],
    ]
    for step in steps:
        run(["openssl", *step], cwd=where)
    os.chmod(where / "tls.key", 0o644)
    return where / "root.crt"


DOVECOT_CONF = """
# No fsync per message: the seeding appends 100k of them.
mail_fsync = never
mail_max_userip_connections = 20
namespace inbox {
  mailbox Archive {
    special_use = \\Archive
    auto = create
  }
}
"""


class Dovecot:
    def __init__(self, certs, password):
        self.password = password
        self.id = run(["docker", "create", "--pull", "never", "--label", LABEL,
                       "-p", "127.0.0.1::31993", "-e", f"USER_PASSWORD={password}",
                       DOVECOT_IMAGE]).strip()
        conf = certs / "zz-measure.conf"
        conf.write_text(DOVECOT_CONF)
        for source, target in ((certs / "tls.crt", "/etc/dovecot/ssl/tls.crt"),
                               (certs / "tls.key", "/etc/dovecot/ssl/tls.key"),
                               (conf, "/etc/dovecot/conf.d/zz-measure.conf")):
            run(["docker", "cp", str(source), f"{self.id}:{target}"])
        run(["docker", "start", self.id])
        self.port = int(run(["docker", "port", self.id, "31993/tcp"]).split(":")[-1])
        self.context = ssl.create_default_context(cafile=str(certs / "root.crt"))
        last = None
        for _ in range(120):
            try:
                imaplib.IMAP4_SSL("localhost", self.port, ssl_context=self.context).logout()
                return
            except ssl.SSLCertVerificationError:
                self.remove()
                raise
            except OSError as err:
                # Docker's proxy takes the connection before Dovecot
                # listens and closes it, which reads as an EOF mid-handshake.
                last = err
                time.sleep(0.5)
        self.remove()
        raise RuntimeError(f"Dovecot did not answer: {last}")

    def remove(self):
        subprocess.run(["docker", "rm", "-f", "-v", self.id], capture_output=True)

    def mail_mib(self):
        """The size of every message on the server, as IMAP counts it."""
        out = run(["docker", "exec", self.id, "doveadm", "-f", "flow", "mailbox", "status",
                   "-u", ADDRESS, "vsize", "*"])
        total = sum(int(word.split("=")[1]) for word in out.split() if word.startswith("vsize="))
        return round(total / 2**20, 1)


WORDS = ("meeting budget invoice draft review project garden weekend report schedule "
         "photos family dinner contract update release server backup travel tickets "
         "lunch summary notes plan question answer thanks please call later today "
         "tomorrow office design kitchen bicycle library museum concert recipe").split()
PEOPLE = [f"{first} {last}" for first in ("Ana", "Ben", "Chen", "Dara", "Eli", "Fatima",
                                          "Gus", "Hana", "Ivo", "Jo", "Kai", "Lena")
          for last in ("Silva", "Okafor", "Novak", "Reyes", "Tanaka", "Moreau",
                       "Ferreira", "Larsen", "Haddad", "Kowalski")]


def sentence(rng, words):
    return " ".join(rng.choice(WORDS) for _ in range(words)).capitalize() + "."


def message(rng, number, when, thread, folder):
    """One generated message: plain text, a third with an HTML part, one
    in twenty with a small attachment. Threads of up to six messages
    point back at their first message."""
    person = rng.choice(PEOPLE)
    sender = f"{person} <{person.lower().replace(' ', '.')}@example.org>"
    me = f"Me <{ADDRESS}>"
    root_id, position, subject = thread
    from_, to = (me, sender) if folder == "Sent" else (sender, me)
    headers = [
        f"From: {from_}", f"To: {to}",
        f"Subject: {'Re: ' if position else ''}{subject}",
        f"Date: {email.utils.formatdate(when, localtime=False)}",
        f"Message-ID: <m{number}@measure.example.org>",
        "MIME-Version: 1.0",
    ]
    if position:
        headers += [f"In-Reply-To: <{root_id}>", f"References: <{root_id}>"]
    text = "\n\n".join(" ".join(sentence(rng, rng.randint(6, 18)) for _ in range(rng.randint(2, 6)))
                       for _ in range(rng.randint(1, 8)))
    kind = rng.random()
    if kind < 0.05:
        blob = os.urandom(rng.randint(2_000, 20_000)).hex()
        boundary = f"b{number}"
        headers.append(f'Content-Type: multipart/mixed; boundary="{boundary}"')
        body = (f"--{boundary}\nContent-Type: text/plain; charset=utf-8\n\n{text}\n"
                f"--{boundary}\nContent-Type: application/octet-stream\n"
                f'Content-Disposition: attachment; filename="notes-{number}.txt"\n\n{blob}\n'
                f"--{boundary}--\n")
    elif kind < 0.35:
        boundary = f"a{number}"
        html = "".join(f"<p>{part}</p>" for part in text.split("\n\n"))
        headers.append(f'Content-Type: multipart/alternative; boundary="{boundary}"')
        body = (f"--{boundary}\nContent-Type: text/plain; charset=utf-8\n\n{text}\n"
                f"--{boundary}\nContent-Type: text/html; charset=utf-8\n\n"
                f"<html><body>{html}</body></html>\n--{boundary}--\n")
    else:
        headers.append("Content-Type: text/plain; charset=utf-8")
        body = text + "\n"
    return ("\r\n".join(headers) + "\r\n\r\n" + body.replace("\n", "\r\n")).encode()


class Appender:
    """A bare IMAP connection that appends many messages per command:
    MULTIAPPEND (RFC 3502) with non-synchronizing literals (LITERAL+), so
    one round trip carries a whole batch. imaplib waits for a
    continuation per message, which made 100k messages take most of an
    hour."""

    def __init__(self, dovecot):
        import socket
        raw = socket.create_connection(("localhost", dovecot.port))
        self.sock = dovecot.context.wrap_socket(raw, server_hostname="localhost")
        self.file = self.sock.makefile("rb")
        self.tag = 0
        self.file.readline()
        self.command(f'LOGIN "{ADDRESS}" "{dovecot.password}"'.encode())

    def command(self, line, allow_no=False):
        self.tag += 1
        tag = f"a{self.tag}".encode()
        self.sock.sendall(tag + b" " + line + b"\r\n")
        while True:
            answer = self.file.readline()
            if not answer:
                raise RuntimeError("Dovecot closed the connection")
            if answer.startswith(tag + b" "):
                if not answer.startswith(tag + b" OK") and not allow_no:
                    raise RuntimeError(answer.decode(errors="replace").strip())
                return answer

    def append(self, mailbox, batch):
        parts = [f'APPEND "{mailbox}"'.encode()]
        for flags, when, raw in batch:
            date = imaplib.Time2Internaldate(when)
            parts.append(f" ({flags}) {date} {{{len(raw)}+}}\r\n".encode() + raw)
        self.command(b"".join(parts))


def seed(dovecot, total, days, seed_value=1):
    """Appends `total` messages across Inbox, Archive and Sent, dated over
    the last `days` days, one connection per mailbox. The app syncs those
    three into the store; a folder with no role it lists only when
    someone opens it, so mail there would not count."""
    shares = {"INBOX": 0.55, "Archive": 0.3, "Sent": 0.15}
    counts = {name: int(total * share) for name, share in shares.items()}
    counts["INBOX"] += total - sum(counts.values())
    now = time.time()
    first_number = 0
    jobs = []
    for name, count in counts.items():
        jobs.append((name, count, first_number))
        first_number += count
    errors = []

    def fill(name, count, first):
        try:
            rng = random.Random(f"{seed_value}-{name}")
            client = Appender(dovecot)
            if name != "INBOX":
                client.command(f'CREATE "{name}"'.encode(), allow_no=True)
            thread, batch = None, []
            for index in range(count):
                if thread is None or thread[1] >= rng.randint(1, 6):
                    thread = [f"m{first + index}@measure.example.org", 0,
                              sentence(rng, rng.randint(3, 7))[:-1]]
                else:
                    thread[1] += 1
                when = now - days * 86400 * (count - index) / count - rng.random() * 600
                flags = "\\Seen" if rng.random() < 0.7 else ""
                if rng.random() < 0.03:
                    flags = (flags + " \\Flagged").strip()
                batch.append((flags, when, message(rng, first + index, when, tuple(thread), name)))
                if len(batch) == 200 or index == count - 1:
                    client.append(name, batch)
                    batch = []
            client.command(b"LOGOUT")
        except Exception as err:  # noqa: BLE001 - reported below
            errors.append(f"{name}: {err}")

    threads = [threading.Thread(target=fill, args=job) for job in jobs]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if errors:
        raise RuntimeError("; ".join(errors))
    return counts


def add_account(profile, port, password):
    """Writes an IMAP account for Dovecot into a store the app has made,
    the rows the Add Account dialog writes, and its password into the
    sandbox's keyring under the names the app reads."""
    with sqlite3.connect(profile.store) as db:
        db.execute("PRAGMA foreign_keys = ON")
        cursor = db.execute(
            "INSERT INTO accounts (email, added_at, provider, provider_name) "
            "VALUES (?, ?, 'imap', 'Dovecot')", (ADDRESS, int(time.time() * 1000)))
        account = cursor.lastrowid
        for role, server_port in (("imap", port), ("smtp", 1)):
            db.execute(
                "INSERT INTO account_servers (account_id, role, host, port, security, user_name) "
                "VALUES (?, ?, 'localhost', ?, 'tls', ?)", (account, role, server_port, ADDRESS))
    subprocess.run(["secret-tool", "store", "--label=Penguin Mail measure",
                    "service", "penguin-mail-imap", "username", str(account)],
                   input=password, text=True, check=True, env=profile.env())
    return account


def store_state(profile):
    try:
        db = sqlite3.connect(f"file:{profile.store}?mode=ro", uri=True, timeout=5)
        try:
            done = db.execute("SELECT backfill_done FROM accounts WHERE email = ?",
                              (ADDRESS,)).fetchone()
            messages = db.execute("SELECT count(*) FROM messages").fetchone()[0]
            threads = db.execute("SELECT count(*) FROM threads").fetchone()[0]
        finally:
            db.close()
    except sqlite3.Error:
        return None
    return {"backfill_done": bool(done and done[0]), "messages": messages, "threads": threads}


def wait_synced(proc, profile, started, timeout, settle=10.0):
    """Waits until the account's backfill is done and the store's counts
    have not moved for `settle` seconds. Returns the seconds from
    `started` to the last change, and the store's counts."""
    last, changed = None, time.monotonic()
    while time.monotonic() - started < timeout:
        if proc.poll() is not None:
            raise RuntimeError("the app ended during the sync")
        state = store_state(profile)
        now = time.monotonic()
        if state and state != last:
            last, changed = state, now
        if last and last["backfill_done"] and now - changed >= settle:
            return changed - started, last
        time.sleep(0.25)
    raise RuntimeError(f"no full sync within {timeout} s; last state {last}")


# The window, through AT-SPI.

def app_node():
    desktop = Atspi.get_desktop(0)
    for index in range(desktop.get_child_count()):
        child = desktop.get_child_at_index(index)
        if child and "PenguinMail" in (child.get_name() or ""):
            return child
    return None


def find_all(node, test, found=None, depth=0):
    found = [] if found is None else found
    if node is None or depth > 60:
        return found
    if test(node):
        found.append(node)
    for index in range(node.get_child_count()):
        find_all(node.get_child_at_index(index), test, found, depth + 1)
    return found


def thread_list():
    """The thread list's scroll pane: the one whose list holds rows that
    start with a sender and subject, right of the sidebar."""
    for _ in range(120):
        root = app_node()
        if root:
            panes = find_all(root, lambda n: n.get_role_name() == "scroll pane")
            for pane in panes:
                lists = [pane.get_child_at_index(i) for i in range(pane.get_child_count())]
                for candidate in lists:
                    if candidate and candidate.get_role_name() == "list" and candidate.get_child_count():
                        first = candidate.get_child_at_index(0)
                        name = first.get_name() if first else ""
                        if name and name.count(",") >= 2 and "unread message" not in name:
                            bar = next((c for c in lists if c and c.get_role_name() == "scroll bar"), None)
                            return candidate, bar
        time.sleep(0.5)
    raise RuntimeError("found no thread list on the accessibility bus")


def click_row(starts):
    """Clicks the first list row whose accessible name starts with `starts`."""
    rows = find_all(app_node(), lambda n: n.get_role_name() == "list item"
                    and (n.get_name() or "").startswith(starts))
    if not rows:
        raise RuntimeError(f"found no row named {starts}...")
    return rows[0].get_extents(Atspi.CoordType.WINDOW)


def reopen_all_inboxes(display):
    """Opens Archive and then All Inboxes from the sidebar, which lists
    All Inboxes afresh. Returns the thread list's scroll range before
    and after. On 7ca4d394 a list that was open during the first sync
    kept its first page and loaded no more as it scrolled; reopening it
    lists the mailbox the way a later launch does."""
    _, bar = thread_list()
    before = round(bar.get_value_iface().get_maximum_value())
    for name in ("Archive", "All Inboxes"):
        box = click_row(name)
        display.click(box.x + box.width / 2, box.y + box.height / 2)
        time.sleep(2)
    _, bar = thread_list()
    return {"range_before_px": before,
            "range_after_px": round(bar.get_value_iface().get_maximum_value())}


def scroll_through(display, burst=10, settle=5.0, timeout=1800):
    """Turns the mouse wheel over the thread list, `burst` notches at a
    time, until the view sits at the bottom and the list has stopped
    growing for `settle` seconds. The list loads the next page of threads
    as the view nears its end, so the bottom can move while this runs.
    Setting the scroll bar's value over AT-SPI does not move a GTK 4
    list, so this goes through XTest as a person's wheel would. Returns
    the seconds it took and the final scroll range in pixels."""
    listing, bar = thread_list()
    value = bar.get_value_iface() if bar else None
    if value is None:
        raise RuntimeError("the thread list's scroll bar has no Value interface")
    box = listing.get_extents(Atspi.CoordType.WINDOW)
    display.pointer(box.x + box.width / 2, box.y + min(box.height / 2, 300))
    started = time.monotonic()
    at_bottom_since, last_log = None, started
    while time.monotonic() - started < timeout:
        position, bottom = value.get_current_value(), value.get_maximum_value()
        if position >= bottom - 1:
            at_bottom_since = at_bottom_since or time.monotonic()
            if time.monotonic() - at_bottom_since >= settle:
                break
        else:
            at_bottom_since = None
        display.wheel(5, burst)
        if time.monotonic() - last_log > 30:
            last_log = time.monotonic()
            log(f"scrolling: at {position:.0f} of {bottom:.0f} px")
        time.sleep(0.02)
    seconds = time.monotonic() - started - settle
    result = {"seconds": round(seconds, 1), "scroll_range_px": round(value.get_maximum_value())}
    # Back to the top, for the conversations opened next.
    back = time.monotonic()
    while value.get_current_value() > value.get_minimum_value() + 1 and time.monotonic() - back < 120:
        display.wheel(4, 50)
        time.sleep(0.01)
    time.sleep(1)
    return result


def open_conversations(display, count, pause):
    """Clicks the first row, then steps down the list with the Down key,
    which opens each conversation as it selects it."""
    listing, _ = thread_list()
    row = listing.get_child_at_index(0)
    box = row.get_extents(Atspi.CoordType.WINDOW)
    display.click(box.x + box.width / 2, box.y + box.height / 2)
    time.sleep(pause)
    names = {selected_name(listing)}
    for _ in range(count - 1):
        display.key(KEY_DOWN)
        time.sleep(pause)
        names.add(selected_name(listing))
    names.discard(None)
    return {"opened": len(names)}


def selected_name(listing):
    for index in range(listing.get_child_count()):
        child = listing.get_child_at_index(index)
        if child and child.get_state_set().contains(Atspi.StateType.SELECTED):
            return child.get_name()
    return None


def large(app, sandbox, args):
    certs = sandbox / "certs"
    root = make_certs(certs)
    password = "measure-" + os.urandom(6).hex()
    log("starting Dovecot")
    dovecot = Dovecot(certs, password)
    report = {}
    try:
        log(f"seeding {args.messages} messages")
        started = time.monotonic()
        counts = seed(dovecot, args.messages, args.days)
        report["seed"] = {"counts": counts, "seconds": round(time.monotonic() - started),
                          "mail_mib": dovecot.mail_mib()}
        log(f"seeded {counts} in {report['seed']['seconds']} s")
        extra = {"SSL_CERT_FILE": str(root), "SSL_CERT_DIR": ""}
        syncs = []
        for index in range(args.sync_runs):
            profile = Profile(sandbox / f"large-{index}")
            proc, _ = app.launch(profile)
            app.stop(proc)
            add_account(profile, dovecot.port, password)
            proc, frame = app.launch(profile, extra=extra)
            seconds, state = wait_synced(proc, profile, app.started, args.sync_timeout)
            run_report = {"sync_s": round(seconds, 1), "frame": frame, **state}
            log(f"first sync {index + 1}: {seconds:.1f} s, {state}")
            if index == args.sync_runs - 1:
                # The full walk happens once, on the last profile.
                wait_quiet(proc, timeout=120)
                run_report["after_sync"] = memory(proc.pid)
                log(f"after sync: {run_report['after_sync']}")
                run_report["reopen"] = reopen_all_inboxes(app.display)
                run_report["scroll"] = scroll_through(app.display)
                wait_quiet(proc, timeout=120)
                run_report["after_scroll"] = memory(proc.pid)
                log(f"after scroll: {run_report['after_scroll']}")
                run_report["open"] = open_conversations(app.display, 20, args.open_pause)
                wait_quiet(proc, timeout=120)
                run_report["after_open"] = memory(proc.pid)
                log(f"after opening 20: {run_report['after_open']}")
                time.sleep(args.idle)
                run_report["after_idle"] = memory(proc.pid)
                log(f"after {args.idle} s idle: {run_report['after_idle']}")
            else:
                run_report["after_sync"] = memory(proc.pid)
            app.stop(proc)
            syncs.append(run_report)
        report["first_sync"] = syncs
        report["first_sync_s"] = summary(syncs, ["sync_s"])["sync_s"]
        profile = Profile(sandbox / f"large-{args.sync_runs - 1}")
        report["disk"] = profile.disk()
        resyncs = []
        for index in range(args.runs):
            before = store_state(profile)
            started = time.monotonic()
            proc, frame = app.launch(profile, extra=extra)
            settled = wait_quiet(proc, timeout=300)
            settled_s = None if settled is None else settled - app.started
            cpu = cpu_seconds(proc.pid)
            time.sleep(args.idle)
            rest = memory(proc.pid)
            app.stop(proc)
            after = store_state(profile)
            entry = {**frame, "settled_s": round(settled_s, 1) if settled_s else None,
                     "cpu_s": round(cpu, 1), **rest,
                     "store_unchanged": before == after}
            resyncs.append(entry)
            log(f"idle resync {index + 1}: {entry}")
        report["idle_resync"] = {"runs": resyncs, **summary(
            resyncs, ["painted_ms", "settled_s", "cpu_s", "rss_mib", "pss_mib", "tree_pss_mib"])}
    finally:
        log("removing Dovecot")
        dovecot.remove()
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", required=True)
    parser.add_argument("--sandbox", required=True)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--idle", type=int, default=30)
    parser.add_argument("--messages", type=int, default=100_000)
    parser.add_argument("--days", type=int, default=28)
    parser.add_argument("--sync-runs", type=int, default=3)
    parser.add_argument("--sync-timeout", type=int, default=3600)
    parser.add_argument("--open-pause", type=float, default=1.5)
    parser.add_argument("--only", default="startup,rest,large")
    args = parser.parse_args()
    sandbox = Path(args.sandbox)
    logs = sandbox / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    app = App(args.app, Display(), logs)
    parts = args.only.split(",")
    report = {"binary_mib": round(Path(args.app).stat().st_size / 2**20, 1)}
    if "startup" in parts:
        report["startup"] = {mode: startup(app, sandbox, mode, args.runs) for mode in ("demo", "empty")}
    if "rest" in parts:
        report["rest"] = {mode: at_rest(app, sandbox, mode, args.runs, args.idle)
                          for mode in ("demo", "empty")}
    if "large" in parts:
        report["large"] = large(app, sandbox, args)
    json.dump(report, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
