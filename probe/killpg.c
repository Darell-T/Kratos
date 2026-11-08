#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void report(const char *label, int result) {
    int saved = errno;
    printf("%-58s -> %d (%s)\n", label, result, result == 0 ? "ok" : strerror(saved));
}

static void wait_unreaped(pid_t leader) {
    siginfo_t info;
    memset(&info, 0, sizeof info);
    int result = waitid(P_PID, (id_t)leader, &info, WEXITED | WNOWAIT);
    printf("%-58s -> %d si_pid=%d si_code=%d\n", "waitid(WEXITED|WNOWAIT)", result, info.si_pid, info.si_code);
}

static pid_t spawn_leader(int with_grandchild, pid_t *grandchild_out) {
    int pipefd[2];
    pipe(pipefd);
    pid_t leader = fork();
    if (leader == 0) {
        setpgid(0, 0);
        if (with_grandchild) {
            pid_t grandchild = fork();
            if (grandchild == 0) {
                close(pipefd[0]);
                close(pipefd[1]);
                sleep(60);
                _exit(0);
            }
            write(pipefd[1], &grandchild, sizeof grandchild);
        } else {
            pid_t none = 0;
            write(pipefd[1], &none, sizeof none);
        }
        _exit(0);
    }
    setpgid(leader, leader);
    close(pipefd[1]);
    pid_t grandchild = 0;
    read(pipefd[0], &grandchild, sizeof grandchild);
    close(pipefd[0]);
    *grandchild_out = grandchild;
    return leader;
}

int main(void) {
    pid_t grandchild;

    puts("== case A: the group holds only the exited, unreaped leader");
    pid_t leader = spawn_leader(0, &grandchild);
    wait_unreaped(leader);
    report("kill(leader, 0) on the zombie leader", kill(leader, 0));
    report("killpg(group, 0) with only the zombie leader", killpg(leader, 0));
    report("killpg(group, SIGKILL) with only the zombie leader", killpg(leader, SIGKILL));
    waitpid(leader, NULL, 0);
    report("killpg(group, SIGKILL) after reaping the leader", killpg(leader, SIGKILL));

    puts("== case B: the exited leader left a live grandchild in the group");
    leader = spawn_leader(1, &grandchild);
    wait_unreaped(leader);
    report("kill(grandchild, 0) before killpg", kill(grandchild, 0));
    report("killpg(group, SIGKILL) with a live grandchild", killpg(leader, SIGKILL));
    usleep(200000);
    report("kill(grandchild, 0) after killpg (ESRCH means it died)", kill(grandchild, 0));
    waitpid(leader, NULL, 0);

    puts("== case C: the leader is still running");
    leader = fork();
    if (leader == 0) {
        setpgid(0, 0);
        sleep(60);
        _exit(0);
    }
    setpgid(leader, leader);
    usleep(100000);
    report("killpg(group, SIGKILL) with a live leader", killpg(leader, SIGKILL));
    waitpid(leader, NULL, 0);
    return 0;
}
