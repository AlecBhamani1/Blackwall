// Start children in a new session and a new group with spawn attributes. Each tries to write
// outside the workspace, then waits; cancellation must kill them.
#include <spawn.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
int main(void) {
    short modes[] = {POSIX_SPAWN_SETSID, POSIX_SPAWN_SETPGROUP};
    char *scripts[] = {"printf escaped > ../outside-setsid; printf tried > tried-setsid; "
                       "exec /bin/sleep 30",
                       "printf escaped > ../outside-setpgroup; printf tried > tried-setpgroup; "
                       "exec /bin/sleep 30"};
    pid_t children[2];
    for (unsigned i = 0; i < 2; i++) {
        posix_spawnattr_t attr;
        if (posix_spawnattr_init(&attr) != 0) return 2;
        if (posix_spawnattr_setflags(&attr, modes[i]) != 0) return 3;
        if (posix_spawnattr_setpgroup(&attr, 0) != 0) return 4;
        char *args[] = {"sh", "-c", scripts[i], NULL};
        int result = posix_spawn(&children[i], "/bin/sh", NULL, &attr, args, environ);
        posix_spawnattr_destroy(&attr);
        if (result != 0) return 5;
    }
    FILE *ready = fopen("spawned.tmp", "w");
    if (ready == NULL) return 6;
    fprintf(ready, "%d %d %d", (int)getpid(), (int)children[0], (int)children[1]);
    fclose(ready);
    if (rename("spawned.tmp", "spawned") != 0) return 7;
    for (unsigned i = 0; i < 2; i++) waitpid(children[i], NULL, 0);
    return 0;
}
