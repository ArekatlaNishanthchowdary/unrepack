# Test data

Streams for the `tor` and `lzma` tests, made from synthetic data by real encoders. The tests hold only the length and CRC-32 of the original data.

## tor-*.bin

Made by `tor.exe` built from the FreeArc 0.67 source (`Compression/Tornado/main.cpp`, g++ 11, `-DFULL_COMPILE -DFREEARC_WIN -DFREEARC_INTEL_BYTE_ORDER -O2 -fno-strict-aliasing`). `tor.exe -d` was used to check that every stream decodes back to its input.

| File | Input | tor options | What it covers |
|---|---|---|---|
| `tor-c1.bin` | `gen.py small.bin 120` | `-1 -b64k` | byte coder |
| `tor-c2.bin` | same | `-2 -b64k` | bit coder |
| `tor-c3t0.bin` | same | `-3 -t0 -b64k` | Huffman, no tables |
| `tor-c4t0.bin` | same | `-5 -t0 -b64k` | arithmetic, no tables |
| `tor-c4opt.bin` | same | `-12 -b64k` | arithmetic, optimal parser |
| `tor-c3t1.bin` | `gen.py big.bin 640` | `-3 -t1 -b64k` | Huffman with tables, past the 256 KB ring |
| `tor-c4t1.bin` | same | `-5 -t1 -b64k` | arithmetic with tables, past the ring |
| `tor-edge.bin` | `gen-edge.py edge.bin 256143` | `-5 -b64k` | a table row cut by the ring's end, 3 bytes off |

`small.bin` is 123,616 bytes (CRC-32 65d817f5), `big.bin` 656,986 bytes (5932f5a6), and `edge.bin` 538,143 bytes (7c8c4568).

## lzma-*.bin

Raw LZMA1 with an end marker, as FreeArc's `lzma` writes, made by Python's `lzma` module (liblzma) from `small.bin` with a 1 MB dictionary: `lzma-default.bin` with lc3 lp0 pb2, and `lzma-lc1lp2pb0.bin` with lc1 lp2 pb0. lc8, which liblzma can't write, is tested with lzma-rust2's own encoder.
