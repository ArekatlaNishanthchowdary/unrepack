/*
 * unrepack-clshost: runs one FreeArc CLS codec DLL as a filter, decoding
 * stdin to stdout. Repack decoders such as cls-lolz.dll and cls-srep.dll are
 * 32-bit DLLs, which a 64-bit unrepack can't load, so it drives them through
 * this process instead and keeps reading the archive itself.
 *
 * Usage: unrepack-clshost <cls-dll> [params]
 * Exit:  0 on success, 1 on bad usage or load failure, 2 when the codec fails,
 *        3 when it crashes (the crash address goes to stderr).
 * Env:   UNREPACK_CLS_TRACE=1 logs every callback to stderr.
 *        UNREPACK_CLS_PAD=0|4|8|12 offsets the stack alignment ClsMain sees.
 *
 * Written against the CLS 0.10 constants in FreeArc's Compression/_CLS/cls.h.
 * Build: gcc -m32 -O2 -s -static -o unrepack-clshost.exe clshost.c
 */
#include <fcntl.h>
#include <io.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

enum {
    CLS_INIT = 1,
    CLS_DONE = 2,
    CLS_DECOMPRESS = 4,
    CLS_FULL_READ = 4096,
    CLS_PARTIAL_READ = 5120,
    CLS_FULL_WRITE = 6144,
    CLS_PARTIAL_WRITE = 7168,
    CLS_MALLOC = 1,
    CLS_FREE = 2,
    CLS_GET_PARAMSTR = 3,
    CLS_ERROR_NOT_IMPLEMENTED = -2,
    CLS_ERROR_NOT_ENOUGH_MEMORY = -3,
    CLS_ERROR_READ = -4,
    CLS_ERROR_WRITE = -5,
};

typedef int(__cdecl *cls_callback)(void *instance, int op, void *ptr, int n);
typedef int(__cdecl *cls_main)(int op, cls_callback callback, void *instance);

static const char *params = "";
static int trace;
static long long bytes_in, bytes_out;
static void *dll_base;

/* cls-uni.dll reports failures with MessageBoxA, which would hang a run no
 * one is watching. Its import is pointed here instead: the text goes to
 * stderr and the box counts as answered. */
static int WINAPI quiet_box(HWND owner, LPCSTR text, LPCSTR caption, UINT type) {
    (void)owner;
    (void)type;
    fprintf(stderr, "clshost: codec message: %s: %s\n", caption ? caption : "", text ? text : "");
    return IDOK;
}

static void silence_message_boxes(HMODULE m) {
    HMODULE user32 = GetModuleHandleA("user32.dll");
    FARPROC real = user32 ? GetProcAddress(user32, "MessageBoxA") : NULL;
    if (!real) return;
    BYTE *base = (BYTE *)m;
    IMAGE_NT_HEADERS32 *nt = (IMAGE_NT_HEADERS32 *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (!dir.VirtualAddress) return;
    for (IMAGE_IMPORT_DESCRIPTOR *imp = (IMAGE_IMPORT_DESCRIPTOR *)(base + dir.VirtualAddress);
         imp->Name; imp++) {
        for (IMAGE_THUNK_DATA32 *t = (IMAGE_THUNK_DATA32 *)(base + imp->FirstThunk); t->u1.Function;
             t++) {
            if (t->u1.Function != (DWORD)(ULONG_PTR)real) continue;
            DWORD old;
            VirtualProtect(&t->u1.Function, sizeof t->u1.Function, PAGE_READWRITE, &old);
            t->u1.Function = (DWORD)(ULONG_PTR)quiet_box;
            VirtualProtect(&t->u1.Function, sizeof t->u1.Function, old, &old);
            if (trace) fprintf(stderr, "clshost: MessageBoxA redirected to stderr\n");
        }
    }
}

/* The instance handed to ClsMain. CLS says it is only passed back to the
 * callback; a real, zeroed block costs nothing and matches what FreeArc and
 * other hosts pass, in case a codec looks at it. */
static long long instance_block[512];

/* Only stream 0 exists: CLS_FULL_READ+i for i > 0 reads as EOF. */
static int read_in(void *ptr, int n, int full) {
    int got = 0;
    while (got < n) {
        int r = _read(0, (char *)ptr + got, (unsigned)(n - got));
        if (r < 0) return CLS_ERROR_READ;
        if (r == 0) break;
        got += r;
        if (!full) break;
    }
    return got;
}

static int write_out(const void *ptr, int n) {
    int put = 0;
    while (put < n) {
        int r = _write(1, (const char *)ptr + put, (unsigned)(n - put));
        if (r <= 0) return CLS_ERROR_WRITE;
        put += r;
    }
    return put;
}

static int handle(int op, void *ptr, int n) {
    if (op >= CLS_FULL_READ && op < CLS_FULL_READ + 1024)
        return op == CLS_FULL_READ ? read_in(ptr, n, 1) : 0;
    if (op >= CLS_PARTIAL_READ && op < CLS_PARTIAL_READ + 1024)
        return op == CLS_PARTIAL_READ ? read_in(ptr, n, 0) : 0;
    if ((op >= CLS_FULL_WRITE && op < CLS_FULL_WRITE + 1024) ||
        (op >= CLS_PARTIAL_WRITE && op < CLS_PARTIAL_WRITE + 1024))
        return (op == CLS_FULL_WRITE || op == CLS_PARTIAL_WRITE) ? write_out(ptr, n)
                                                                 : CLS_ERROR_WRITE;
    switch (op) {
    case CLS_MALLOC:
        *(void **)ptr = malloc((size_t)n);
        return *(void **)ptr ? 0 : CLS_ERROR_NOT_ENOUGH_MEMORY;
    case CLS_FREE:
        free(ptr);
        return 0;
    case CLS_GET_PARAMSTR:
        if (n <= 0) return CLS_ERROR_NOT_IMPLEMENTED;
        strncpy((char *)ptr, params, (size_t)n);
        ((char *)ptr)[n - 1] = '\0';
        return 0;
    default:
        return CLS_ERROR_NOT_IMPLEMENTED;
    }
}

static int __cdecl callback(void *instance, int op, void *ptr, int n) {
    (void)instance;
    int r = handle(op, ptr, n);
    if (op >= CLS_FULL_READ && op < CLS_FULL_READ + 2048 && r > 0) bytes_in += r;
    if (op >= CLS_FULL_WRITE && op < CLS_FULL_WRITE + 2048 && r > 0) bytes_out += r;
    if (trace) fprintf(stderr, "clshost: op=%d n=%d -> %d\n", op, n, r);
    return r;
}

/* Some codec DLLs keep SSE data on the stack and assume it is 16-byte
 * aligned, while 32-bit Windows code only promises 4. So ClsMain is called
 * with esp aligned to 16 at the call instruction, then lowered by `pad`.
 * Plain assembly, since the stack moves under any C operand. */
int __cdecl call_aligned(cls_main f, int op, unsigned pad, cls_callback cb, void *instance);
__asm__(".globl _call_aligned\n"
        "_call_aligned:\n\t"
        "pushl %ebp\n\t"
        "movl %esp, %ebp\n\t"
        "movl 8(%ebp), %eax\n\t"
        "movl 12(%ebp), %ecx\n\t"
        "movl 16(%ebp), %edx\n\t"
        "andl $-16, %esp\n\t"
        "subl %edx, %esp\n\t"
        "subl $4, %esp\n\t"
        "pushl 24(%ebp)\n\t"
        "pushl 20(%ebp)\n\t"
        "pushl %ecx\n\t"
        "call *%eax\n\t"
        "movl %ebp, %esp\n\t"
        "popl %ebp\n\t"
        "ret\n");

/* Says where a crashing codec crashed, as an offset into its DLL, so the
 * faulting instruction can be found in a disassembly. */
static LONG CALLBACK on_crash(EXCEPTION_POINTERS *e) {
    DWORD code = e->ExceptionRecord->ExceptionCode;
    if (code != EXCEPTION_ACCESS_VIOLATION && code != EXCEPTION_ILLEGAL_INSTRUCTION &&
        code != EXCEPTION_PRIV_INSTRUCTION && code != EXCEPTION_DATATYPE_MISALIGNMENT)
        return EXCEPTION_CONTINUE_SEARCH;
    char *at = (char *)e->ExceptionRecord->ExceptionAddress;
    fprintf(stderr,
            "clshost: exception 0x%08lx at %p (dll base %p, rva 0x%lx), esp %08lx, thread %lu\n",
            code, (void *)at, dll_base, (unsigned long)(at - (char *)dll_base),
            (unsigned long)e->ContextRecord->Esp, GetCurrentThreadId());
    if (e->ExceptionRecord->NumberParameters >= 2)
        fprintf(stderr, "clshost: %s address %08lx\n",
                e->ExceptionRecord->ExceptionInformation[0] ? "writing" : "reading",
                (unsigned long)e->ExceptionRecord->ExceptionInformation[1]);
    CONTEXT *c = e->ContextRecord;
    fprintf(stderr, "clshost: eax %08lx ebx %08lx ecx %08lx edx %08lx esi %08lx edi %08lx ebp %08lx\n",
            c->Eax, c->Ebx, c->Ecx, c->Edx, c->Esi, c->Edi, c->Ebp);
    /* Return addresses into the DLL among the top of the stack give the call
     * chain without symbols. */
    unsigned long *sp = (unsigned long *)c->Esp;
    unsigned long lo = (unsigned long)dll_base, hi = lo + 0x100000;
    for (int i = 0; i < 256; i++) {
        unsigned long v;
        if (IsBadReadPtr(sp + i, sizeof v)) break;
        v = sp[i];
        if (v >= lo && v < hi) fprintf(stderr, "clshost: stack[esp+0x%x] = rva 0x%lx\n", i * 4, v - lo);
    }
    fflush(stderr);
    ExitProcess(3);
    return EXCEPTION_CONTINUE_SEARCH;
}

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "usage: %s <cls-dll> [params]\n", argv[0]);
        return 1;
    }
    if (argc == 3) params = argv[2];
    _setmode(0, _O_BINARY);
    _setmode(1, _O_BINARY);
    setvbuf(stderr, NULL, _IONBF, 0);
    trace = getenv("UNREPACK_CLS_TRACE") != NULL;
    const char *pad_env = getenv("UNREPACK_CLS_PAD");
    unsigned pad = pad_env ? (unsigned)atoi(pad_env) & 12u : 0;
    AddVectoredExceptionHandler(1, on_crash);

    /* Newer cls-uni shims (FitGirl's CLS-srep.dll, cls-lollypop.dll) read the
     * prefix of their helper's mapping names from "Init_MapFile_"; FitGirl's
     * installer script creates it holding "Global\", which is what the
     * helper exes always use. Older shims ignore it. Kept open until exit. */
    HANDLE init_map = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 0x100,
                                         "Init_MapFile_");
    char *init_view = init_map ? (char *)MapViewOfFile(init_map, FILE_MAP_ALL_ACCESS, 0, 0, 0x100) : NULL;
    if (init_view) lstrcpyA(init_view, "Global\\");
    else fprintf(stderr, "clshost: can't create Init_MapFile_ (error %lu)\n", GetLastError());

    HMODULE dll = LoadLibraryA(argv[1]);
    if (!dll) {
        fprintf(stderr, "clshost: can't load %s (error %lu)\n", argv[1], GetLastError());
        return 1;
    }
    dll_base = (void *)dll;
    silence_message_boxes(dll);
    cls_main entry = (cls_main)(void *)GetProcAddress(dll, "ClsMain");
    if (!entry) {
        fprintf(stderr, "clshost: %s has no ClsMain\n", argv[1]);
        return 1;
    }
    if (trace) fprintf(stderr, "clshost: %s loaded at %p, pad %u\n", argv[1], dll_base, pad);
    int init = call_aligned(entry, CLS_INIT, pad, callback, instance_block);
    int result = call_aligned(entry, CLS_DECOMPRESS, pad, callback, instance_block);
    int done = call_aligned(entry, CLS_DONE, pad, callback, instance_block);
    if (trace)
        fprintf(stderr, "clshost: init=%d decompress=%d done=%d, read %lld, wrote %lld\n", init,
                result, done, bytes_in, bytes_out);
    if (result < 0) {
        fprintf(stderr, "clshost: %s failed with CLS error %d\n", argv[1], result);
        return 2;
    }
    return 0;
}
