#ifndef SHELL_SHELL_H
#define SHELL_SHELL_H

/* ========================================================================
 * SHELL API
 * ======================================================================== */

/*
 * Maximum tokenisation parameters for shell command parsing.
 */
#define SHELL_MAX_TOKENS        16
#define SHELL_MAX_TOKEN_LENGTH  64

/*
 * Main shell entry point
 * Called as task entry function
 */
void shell_main(void *arg);

/*
 * Execute a command (placeholder for now)
 * Will be enhanced in Task 05 (parser) and Task 06 (commands)
 */
void shell_execute_command(const char *line);

/*
 * Parse a raw command line into tokens.
 *
 * Returns the number of tokens written to the provided array, or 0 when the
 * line is empty or only contains whitespace.
 */
int shell_parse_line(const char *line, char **tokens, int max_tokens);

/* Kernel-side helpers */
int shell_launch_once(void);
void shell_register_roulette_hook(void);

#endif /* SHELL_SHELL_H */
