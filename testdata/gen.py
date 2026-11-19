import struct, zlib, sys
x = 12345
def rnd():
    global x
    x = (x * 1103515245 + 12345) & 0x7fffffff
    return x >> 8
words = [b"repack", b"unpack", b"game", b"data", b"file", b"tornado", b"stream", b"block", b"index", b"the", b"a", b"of"]
pool = []
for _ in range(24):
    pool.append(b" ".join(words[rnd() % len(words)] for _ in range(4 + rnd() % 8)) + b".\n")
out = bytearray()
while len(out) < int(sys.argv[2]) * 1024:
    k = rnd() % 9; k = 0 if k < 6 else k - 5
    if k == 0:   # text run from a small pool of sentences
        for _ in range(60):
            out += pool[rnd() % len(pool)]
    elif k == 1: # u16 table rising slowly
        v = rnd() % 1000
        for _ in range(1500):
            v = (v + 3 + (rnd() % 16 == 0)) & 0xffff
            out += struct.pack("<H", v)
    elif k == 2: # u32 table rising
        v = rnd()
        for _ in range(1200):
            v = (v + 1000 + 64 * (rnd() % 16 == 0)) & 0xffffffff
            out += struct.pack("<I", v)
    else:        # a little noise
        out += bytes(rnd() & 255 for _ in range(150))
open(sys.argv[1], "wb").write(out)
print(len(out), "%08x" % zlib.crc32(out))
