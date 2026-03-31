module completions {

  def "nu-complete harnx completions" [] {
    [ "bash" "zsh" "fish" "powershell" "nushell" ]
  }

  def "nu-complete harnx model" [] {
    ^harnx --list-models |
    | lines 
    | parse "{value}" 
  }

  def "nu-complete harnx role" [] {
    ^harnx --list-roles |
    | lines 
    | parse "{value}" 
  }

  def "nu-complete harnx session" [] {
    ^harnx --list-sessions |
    | lines 
    | parse "{value}" 
  }

  def "nu-complete harnx agent" [] {
    ^harnx --list-agents |
    | lines 
    | parse "{value}" 
  }

  def "nu-complete harnx rag" [] {
    ^harnx --list-rags |
    | lines 
    | parse "{value}" 
  }

  def "nu-complete harnx macro" [] {
    ^harnx --list-macros |
    | lines 
    | parse "{value}" 
  }

  export extern harnx [
    --model(-m): string@"nu-complete harnx model"      # Select a LLM model
    --prompt                                            # Use the system prompt
    --role(-r): string@"nu-complete harnx role"        # Select a role
    --session(-s): string@"nu-complete harnx session"  # Start or join a session
    --empty-session                                     # Ensure the session is empty
    --save-session                                      # Ensure the new conversation is saved to the session
    --agent(-a): string@"nu-complete harnx agent"      # Start a agent
    --agent-variable                                    # Set agent variables
    --rag: string@"nu-complete harnx rag"              # Start a RAG
    --rebuild-rag                                       # Rebuild the RAG to sync document changes
    --macro: string@"nu-complete harnx macro"          # Execute a macro
    --serve                                             # Serve the LLM API and WebAPP
    --execute(-e)                                       # Execute commands in natural language
    --code(-c)                                          # Output code only
    --file(-f): string                                  # Include files, directories, or URLs
    --no-stream(-S)                                     # Turn off stream mode
    --dry-run                                           # Display the message without sending it
    --info                                              # Display information
    --sync-models                                       # Sync models updates
    --list-models                                       # List all available chat models
    --list-roles                                        # List all roles
    --list-sessions                                     # List all sessions
    --list-agents                                       # List all agents
    --list-rags                                         # List all RAGs
    --list-macros                                       # List all macros
    ...text: string                                     # Input text
    --help(-h)                                          # Print help
    --version(-V)                                       # Print version
  ]

}

export use completions *
