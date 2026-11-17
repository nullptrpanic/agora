#include <spawn.h>

extern int posix_spawn_file_actions_addchdir(posix_spawn_file_actions_t *, const char *)
    __attribute__((weak_import));
extern int posix_spawn_file_actions_addfchdir(posix_spawn_file_actions_t *, int)
    __attribute__((weak_import));
extern int agora_sandbox_posix_spawn_file_actions_addchdir(posix_spawn_file_actions_t *, const char *);
extern int agora_sandbox_posix_spawn_file_actions_addfchdir(posix_spawn_file_actions_t *, int);

struct agora_interpose { const void *replacement; const void *replacee; };
__attribute__((used, section("__DATA,__interpose")))
const struct agora_interpose AGORA_INTERPOSE_SPAWN_ADDCHDIR = {
    (const void *)agora_sandbox_posix_spawn_file_actions_addchdir,
    (const void *)posix_spawn_file_actions_addchdir,
};
__attribute__((used, section("__DATA,__interpose")))
const struct agora_interpose AGORA_INTERPOSE_SPAWN_ADDFCHDIR = {
    (const void *)agora_sandbox_posix_spawn_file_actions_addfchdir,
    (const void *)posix_spawn_file_actions_addfchdir,
};
