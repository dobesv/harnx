# Command Line Guide

## Usage

```
Usage: harnx [OPTIONS] [TEXT]...

Arguments:
  [TEXT]...  Input text

Options:
  -m, --model <MODEL>                  Select a LLM model
      --prompt <PROMPT>                Use the system prompt
  -r, --role <ROLE>                    Select a role
  -s, --session [<SESSION>]            Start or join a session
      --empty-session                  Ensure the session is empty
      --save-session                   Ensure the new conversation is saved to the session
  -a, --agent <AGENT>                  Start a agent
      --agent-variable <NAME> <VALUE>  Set agent variables
      --rag <RAG>                      Start a RAG
      --rebuild-rag                    Rebuild the RAG to sync document changes
      --macro <MACRO>                  Execute a macro
      --serve [<ADDRESS>]              Serve the LLM API and WebAPP
  -e, --execute                        Execute commands in natural language
  -c, --code                           Output code only
  -f, --file <FILE>                    Include files, directories, or URLs
  -S, --no-stream                      Turn off stream mode
      --dry-run                        Display the message without sending it
      --info                           Display information
      --sync-models                    Sync models updates
      --list-models                    List all available chat models
      --list-roles                     List all roles
      --list-sessions                  List all sessions
      --list-agents                    List all agents
      --list-rags                      List all RAGs
      --list-macros                    List all macros
  -h, --help                           Print help
  -V, --version                        Print version
```

## Examples

```
harnx                                          # Enter REPL
harnx Tell a joke                              # Generate response

harnx -e install nvim                          # Execute command
harnx -c fibonacci in js                       # Generate code

harnx --serve                                  # Run server
harnx --serve 0.0.0.0:8080                     # Run server with addr

harnx -m openai:gpt-4o                         # Select LLM

harnx -r role1                                 # Use role 'role1'
harnx -s                                       # Begin a temp session
harnx -s session1                              # Use session 'session1'
harnx -a agent1                                # Use agent 'agent1'
harnx --rag rag1                               # Use RAG 'rag1'

harnx --info                                   # View system info
harnx -r role1 --info                          # View role info
harnx -s session1 --info                       # View session info
harnx -a agent1 --info                         # View agent info
harnx --rag rag1 --info                        # View RAG info

harnx --macro macro1                           # Execute macro 'macro1'
harnx --macro macro2 arg1 arg2                 # Execute macro 'macro2' with args

cat data.toml | harnx -c to json > data.json   # Pipe Input/Output
output=$(harnx -S $input)                      # Run in the script

harnx -f a.png -f b.png diff images            # Use files
```

## Shell Assistant

Simply input what you want to do in natural language, and harnx will prompt and run the command that achieves your intent.

**Harnx is aware of the OS and shell you're using, so it provides shell commands for your specific system.**

## Shell Integration

Simply type `alt+e` to let `harnx` provide intelligent completions directly in your terminal.

Harnx offers shell integration scripts for bash, zsh, PowerShell, fish, and nushell. You can find them on GitHub at [https://github.com/dobesv/harnx/tree/main/scripts/shell-integration](https://github.com/dobesv/harnx/tree/main/scripts/shell-integration).

## Shell Autocompletion

The shell autocompletion suggests commands, options, and filenames as you type, enabling you to type less, work faster, and avoid typos.

Harnx offers shell completion scripts for bash, zsh, PowerShell, fish, and nushell. You can find them on GitHub at [https://github.com/dobesv/harnx/tree/main/scripts/completions](https://github.com/dobesv/harnx/tree/main/scripts/completions).

## Generate Code

By using the `--code` or `-c` parameter, you can specifically request pure code output.

**The `-c/--code` with pipe ensures the extraction of code from Markdown.**

## Use Files & URLs

The `-f/--file` flag can be used to send files to LLMs.

```
# Use local file
harnx -f data.txt
# Use image file
harnx -f image.png ocr
# Use multiple files
harnx -f file1 -f file2 explain
# Use local dirs
harnx -f dir/ summarize
# Use remote URLs
harnx -f https://example.com/page summarize
```

## Run Server

Harnx comes with a built-in lightweight HTTP server.

```
$ harnx --serve
Chat Completions API: http://127.0.0.1:8000/v1/chat/completions
Embeddings API:       http://127.0.0.1:8000/v1/embeddings
LLM Playground:       http://127.0.0.1:8000/playground
LLM Arena:            http://127.0.0.1:8000/arena?num=2
```

Change the listening address:

```
$ harnx --serve 0.0.0.0
$ harnx --serve 8080
$ harnx --serve 0.0.0.0:8080
```
