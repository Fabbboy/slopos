/*
 * SlopOS Shell Implementation
 * Main REPL (Read-Eval-Print Loop) for user interaction
 */

#include "shell.h"
#include "builtins.h"
#include "../drivers/tty.h"
#include "../drivers/serial.h"

#include <stddef.h>
#include <stdint.h>
#include "../lib/klog.h"

/* ========================================================================
 * HELPER UTILITIES
 * ======================================================================== */

static inline int shell_is_whitespace(char c) {
    return (c == ' ') || (c == '\t');
}

/* ========================================================================
 * COMMAND LINE PARSING
 * ======================================================================== */

int shell_parse_line(const char *line, char **tokens, int max_tokens) {
    if (!line || !tokens || max_tokens <= 0) {
        return 0;
    }
    
    if (max_tokens > SHELL_MAX_TOKENS) {
        max_tokens = SHELL_MAX_TOKENS;
    }
    
    static char token_storage[SHELL_MAX_TOKENS][SHELL_MAX_TOKEN_LENGTH];
    
    int token_count = 0;
    const char *cursor = line;
    
    while (*cursor != '\0') {
        /* Skip any leading whitespace before the next token */
        while (*cursor != '\0' && shell_is_whitespace(*cursor)) {
            cursor++;
        }
        
        if (*cursor == '\0') {
            break;  /* Reached end after whitespace */
        }
        
        /* Determine token length (up to whitespace or end-of-string) */
        size_t token_length = 0;
        while (cursor[token_length] != '\0' && !shell_is_whitespace(cursor[token_length])) {
            token_length++;
        }
        
        /* If we've reached the maximum token capacity, skip remaining tokens */
        if (token_count >= max_tokens) {
            cursor += token_length;
            continue;
        }
        
        /* Copy token into storage, truncating if necessary */
        size_t copy_length = token_length;
        if (copy_length > (SHELL_MAX_TOKEN_LENGTH - 1)) {
            copy_length = SHELL_MAX_TOKEN_LENGTH - 1;
        }
        
        for (size_t i = 0; i < copy_length; ++i) {
            token_storage[token_count][i] = cursor[i];
        }
        token_storage[token_count][copy_length] = '\0';
        
        tokens[token_count] = token_storage[token_count];
        token_count++;
        
        /* Advance cursor past this token (including any truncated tail) */
        cursor += token_length;
    }
    
    if (token_count < max_tokens) {
        tokens[token_count] = NULL;
    }
    
    return token_count;
}

/* ========================================================================
 * COMMAND EXECUTION (STUB)
 * ======================================================================== */

/*
 * Execute a command (placeholder for now)
 * Will be enhanced in Task 05 (parser) and Task 06 (commands)
 */
void shell_execute_command(const char *line) {
    if (!line) {
        return;
    }

    char *tokens[SHELL_MAX_TOKENS];
    int token_count = shell_parse_line(line, tokens, SHELL_MAX_TOKENS);

    if (token_count <= 0) {
        /* Empty or whitespace-only input */
        return;
    }

    const shell_builtin_t *cmd = shell_builtin_lookup(tokens[0]);
    if (!cmd) {
        klog_raw(KLOG_INFO, "Unknown command: ");
        klog(KLOG_INFO, tokens[0]);
        klog(KLOG_INFO, "Type 'help' to list available commands.");
        return;
    }

    int result = cmd->handler(token_count, tokens);
    if (result != 0) {
        klog_raw(KLOG_INFO, "Command '");
        klog_raw(KLOG_INFO, cmd->name);
        klog_raw(KLOG_INFO, "' returned error code ");
        if (result < 0) {
            klog_raw(KLOG_INFO, "-");
            klog_decimal(KLOG_INFO, (uint64_t)(-result));
        } else {
            klog_decimal(KLOG_INFO, (uint64_t)result);
        }
        klog(KLOG_INFO, "");
    }
}

/* ========================================================================
 * SHELL MAIN LOOP (REPL)
 * ======================================================================== */

/*
 * Main shell entry point
 * Called as task entry function
 */
void shell_main(void *arg) {
    (void)arg;  /* Unused parameter */
    
    /* Print welcome message (optional) */
    klog(KLOG_INFO, "");
    klog(KLOG_INFO, "SlopOS Shell v0.1");
    klog(KLOG_INFO, "");
    
    /* REPL loop */
    while (1) {
        /* Display prompt */
        klog_raw(KLOG_INFO, "$ ");
        
        /* Read line from keyboard */
        char line_buffer[256];
        size_t line_length = tty_read_line(line_buffer, sizeof(line_buffer));

        /* Handle empty lines */
        if (line_length == 0) {
            continue;
        }

        /* Execute command */
        shell_execute_command(line_buffer);
    }
    
    /* Should never reach here (infinite loop) */
}
