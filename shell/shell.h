#ifndef SHELL_SHELL_H
#define SHELL_SHELL_H

/* Kernel-side glue to launch the userland shell after roulette win. */
int shell_launch_once(void);
void shell_register_roulette_hook(void);

#endif /* SHELL_SHELL_H */
