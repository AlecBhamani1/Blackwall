// Verify that spawn attributes cannot evade the sandbox's process-group cleanup.
#include <errno.h>
#include <spawn.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
int main(void) {
    short modes[] = {POSIX_SPAWN_SETSID, POSIX_SPAWN_SETPGROUP};
    for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); i++) {
        posix_spawnattr_t attr;
        if (posix_spawnattr_init(&attr) != 0) return 2;
        if (posix_spawnattr_setflags(&attr, modes[i]) != 0) return 3;
        if (posix_spawnattr_setpgroup(&attr, 0) != 0) return 4;
        char *args[] = {"sh", "-c", "printf escaped > escaped-spawn", NULL};
        pid_t child;
        int result = posix_spawn(&child, "/bin/sh", NULL, &attr, args, environ);
        posix_spawnattr_destroy(&attr);
        if (result == 0) { waitpid(child, NULL, 0); return 5; }
        if (result != EPERM) return 6;
    }
    return 0;
}
