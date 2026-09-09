// 자식 프로세스를 macOS 책임 프로세스에서 분리해 실행하는 도구다. 권한이 허용된 실행 파일(예: nvm node)을
// 이 도구로 띄우면 그 실행 파일 자신이 책임 프로세스가 되어 손쉬운 사용·화면 기록 판정을 그대로 받는다.
//
// 빌드
//   clang -o /tmp/disclaim-run scripts/officestra-disclaim-run.c
// 사용
//   disclaim-run <실행 파일 절대 경로> [인자...]
//   예: /tmp/disclaim-run /Users/neo/.nvm/versions/node/v24.14.0/bin/node script.js
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
// libSystem의 비공개 함수. posix_spawn 속성에 책임 분리(disclaim)를 켠다.
extern int responsibility_spawnattrs_setdisclaim(posix_spawnattr_t *attrs, int disclaim);

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: disclaim-run <program> [args]\n");
    return 2;
  }
  posix_spawnattr_t attr;
  posix_spawnattr_init(&attr);
  if (responsibility_spawnattrs_setdisclaim(&attr, 1) != 0) {
    perror("setdisclaim");
    return 2;
  }
  pid_t pid;
  int rc = posix_spawn(&pid, argv[1], NULL, &attr, argv + 1, environ);
  if (rc != 0) {
    fprintf(stderr, "spawn failed: %d\n", rc);
    return 2;
  }
  int status = 0;
  waitpid(pid, &status, 0);
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
