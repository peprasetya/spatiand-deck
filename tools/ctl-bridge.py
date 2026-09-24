"""Relay lines between stdin/stdout and a Unix control socket. Run on the machine with the socket:
python3 ctl-bridge.py $XDG_RUNTIME_DIR/spatiand/control   (used by tools/clipboard-matrix.py over ssh)"""
import socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
def pump():
    f = s.makefile("r", encoding="utf-8", errors="replace")
    for line in f:
        sys.stdout.write(line)
        sys.stdout.flush()
    sys.stdout.write("closed\n"); sys.stdout.flush()
threading.Thread(target=pump, daemon=True).start()
for line in sys.stdin:
    s.sendall(line.encode())
