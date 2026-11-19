# A u32 table across the 256 KB ring end at a chosen offset, for tornado's
# smallest ring (-b64k): text up to the table, the table, then more text.
import struct, sys, zlib
x = 777
def rnd():
    global x
    x = (x * 1103515245 + 12345) & 0x7fffffff
    return x >> 8
words = [b"repack", b"unpack", b"game", b"data", b"file", b"stream", b"the", b"of"]
pool = [b" ".join(words[rnd() % len(words)] for _ in range(3 + rnd() % 6)) + b".\n" for _ in range(16)]
def text(n):
    t = bytearray()
    while len(t) < n:
        t += pool[rnd() % len(pool)]
    return bytes(t[:n])
start = int(sys.argv[2])
out = bytearray(text(start))
v = 0x470000
for _ in range(3000):
    v += 1000
    out += struct.pack("<I", v)
out += text(270000)
open(sys.argv[1], "wb").write(out)
print(len(out), "%08x" % zlib.crc32(out))
