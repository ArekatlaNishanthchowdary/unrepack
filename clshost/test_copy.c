/* A CLS codec whose decoding is copying: lets the tests run the whole
 * pipeline, host processes included, without any third-party DLL.
 * With the method parameter "fail" (on the host's command line) it instead
 * starts a 60 s grandchild that inherits the host's pipes and fails, as a
 * cls-uni shim's helper exe does when the shim gives up. */
#include <string.h>
#include <windows.h>
typedef int(__cdecl *cls_callback)(void *, int, void *, int);
__declspec(dllexport) int __cdecl ClsMain(int op, cls_callback cb, void *inst) {
    if (op != 4) return op == 1 || op == 2 ? 0 : -2;
    const char *cl = GetCommandLineA();
    size_t n_cl = strlen(cl);
    if (n_cl >= 4 && strcmp(cl + n_cl - 4, "fail") == 0) {
        STARTUPINFOA si = {sizeof si};
        PROCESS_INFORMATION pi;
        char cmd[] = "cmd.exe /c ping -n 60 127.0.0.1";
        if (CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        return -1;
    }
    static char buf[65536];
    for (int n; (n = cb(inst, 5120, buf, sizeof buf)) != 0;) {
        if (n < 0) return n;
        if (cb(inst, 6144, buf, n) != n) return -5;
    }
    return 0;
}
