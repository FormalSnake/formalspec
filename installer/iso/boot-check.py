#!/usr/bin/env python3
"""Boots an installer ISO under QEMU and waits for fspec-install's first frame on the serial console.

usage: boot-check.py <iso> <serial-log>

Secure Boot is used when OVMF_CODE and OVMF_VARS point at a Secure Boot firmware build with the
Microsoft keys enrolled; otherwise plain OVMF is used and the script says so. Environment:
  OVMF_CODE, OVMF_VARS   firmware files (default: Ubuntu's ovmf package, Secure Boot variant)
  QEMU                   qemu-system-x86_64 binary
  BOOT_TIMEOUT           seconds to wait for the marker (default 300)
  BOOT_MARKER            serial line that means success (default: fspec-install: first screen)
"""
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

MARKER = os.environ.get("BOOT_MARKER", "fspec-install: first screen").encode()
iso, serial_log = sys.argv[1], sys.argv[2]
timeout = int(os.environ.get("BOOT_TIMEOUT", "300"))
qemu = os.environ.get("QEMU", "qemu-system-x86_64")

candidates = [
    (os.environ.get("OVMF_CODE"), os.environ.get("OVMF_VARS"), "secure boot"),
    ("/usr/share/OVMF/OVMF_CODE_4M.secboot.fd", "/usr/share/OVMF/OVMF_VARS_4M.ms.fd", "secure boot"),
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd", "plain OVMF, no secure boot"),
    ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd", "plain OVMF, no secure boot"),
]
code, varsrc, mode = next(
    (c, v, m) for c, v, m in candidates if c and v and os.path.exists(c) and os.path.exists(v)
)
print(f"boot-check: firmware {code} ({mode})", flush=True)

tmp = tempfile.mkdtemp(prefix="fspec-boot-")
varsf = os.path.join(tmp, "vars.fd")
shutil.copy(varsrc, varsf)
os.chmod(varsf, 0o644)
disk = os.path.join(tmp, "disk.qcow2")
subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", disk, "20G"], check=True)
sock_path = os.path.join(tmp, "serial.sock")
if os.path.exists(serial_log):
    os.remove(serial_log)

accel = ["-enable-kvm", "-cpu", "host"] if os.path.exists("/dev/kvm") else ["-cpu", "max"]
print(f"boot-check: {'kvm' if accel[0] == '-enable-kvm' else 'tcg, no /dev/kvm'}", flush=True)
args = [
    qemu, "-machine", "q35,smm=on", *accel, "-m", "3072", "-smp", "2",
    "-global", "driver=cfi.pflash01,property=secure,value=on",
    "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={code}",
    "-drive", f"if=pflash,format=raw,unit=1,file={varsf}",
    "-drive", f"file={disk},if=virtio,format=qcow2",
    "-cdrom", iso, "-boot", "d",
    "-device", "virtio-vga", "-display", "none",
    "-chardev", f"socket,id=ser,path={sock_path},server=on,wait=off,logfile={serial_log}",
    "-serial", "chardev:ser", "-monitor", "none",
]
proc = subprocess.Popen(args)

ok = False
try:
    for _ in range(100):
        if os.path.exists(sock_path):
            break
        time.sleep(0.1)
    s = socket.socket(socket.AF_UNIX)
    s.connect(sock_path)
    s.setblocking(False)

    def log():
        try:
            return open(serial_log, "rb").read()
        except FileNotFoundError:
            return b""

    deadline = time.time() + timeout
    sent = False
    while time.time() < deadline and proc.poll() is None:
        try:
            s.recv(65536)
        except BlockingIOError:
            pass
        data = log()
        # Entry 2 of the GRUB menu is the ttyS0 console entry; the menu times out after 5 seconds.
        if not sent and (b"highlighted entry will be executed" in data or b"serial console, ttyS0" in data):
            time.sleep(1)
            s.sendall(b"\x1b[B")
            time.sleep(0.5)
            s.sendall(b"\r")
            sent = True
            print("boot-check: selected the serial console entry", flush=True)
        if MARKER in data:
            ok = True
            break
        time.sleep(1)
finally:
    proc.terminate()
    try:
        proc.wait(10)
    except subprocess.TimeoutExpired:
        proc.kill()
    shutil.rmtree(tmp, ignore_errors=True)

data = open(serial_log, "rb").read() if os.path.exists(serial_log) else b""
if b"Secure boot enabled" in data or b"secureboot: Secure boot enabled" in data:
    print("boot-check: kernel reports secure boot enabled", flush=True)
print(f"boot-check: {'PASS' if ok else 'FAIL'} (marker {'seen' if ok else f'not seen within {timeout}s'})", flush=True)
sys.exit(0 if ok else 1)
