complete -c harnx -s m -l model -x -a "(harnx --list-models)" -d 'Select a LLM model' -r
complete -c harnx -l prompt -d 'Use the system prompt'
complete -c harnx -s s -l session -x  -a "(harnx --list-sessions)" -d 'Start or join a session' -r
complete -c harnx -l empty-session -d 'Ensure the session is empty'
complete -c harnx -l save-session -d 'Ensure the new conversation is saved to the session'
complete -c harnx -s a -l agent -x  -a "(harnx --list-agents)" -d 'Start a agent' -r
complete -c harnx -l agent-variable -d 'Set agent variables'
complete -c harnx -l rag -x  -a"(harnx --list-rags)" -d 'Start a RAG' -r
complete -c harnx -l rebuild-rag -d 'Rebuild the RAG to sync document changes'
complete -c harnx -l macro -x  -a"(harnx --list-macros)" -d 'Execute a macro' -r
complete -c harnx -l serve -d 'Serve the LLM API and WebAPP' -r
complete -c harnx -l acp -x -a "(harnx --list-agents)" -d 'Serve as an ACP agent over stdio' -r
complete -c harnx -s f -l file -d 'Include files, directories, or URLs' -r -F
complete -c harnx -s S -l no-stream -d 'Turn off stream mode'
complete -c harnx -l dry-run -d 'Display the message without sending it'
complete -c harnx -l info -d 'Display information'
complete -c harnx -l sync-models -d 'Sync models updates'
complete -c harnx -l list-models -d 'List all available chat models'
complete -c harnx -l list-sessions -d 'List all sessions'
complete -c harnx -l list-agents -d 'List all agents'
complete -c harnx -l list-rags -d 'List all RAGs'
complete -c harnx -l list-macros -d 'List all macros'
complete -c harnx -l mcp-root -d 'Add MCP roots' -r -F
complete -c harnx -s t -l tool -d 'Enable tools or toolsets for this session'
complete -c harnx -s h -l help -d 'Print help'
complete -c harnx -s V -l version -d 'Print version'