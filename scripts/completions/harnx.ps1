using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'harnx' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'harnx'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'harnx' {
            [CompletionResult]::new('-m', '-m', [CompletionResultType]::ParameterName, 'Select a LLM model')
            [CompletionResult]::new('--model', '--model', [CompletionResultType]::ParameterName, 'Select a LLM model')
            [CompletionResult]::new('--prompt', '--prompt', [CompletionResultType]::ParameterName, 'Use the system prompt')
            [CompletionResult]::new('-s', '-s', [CompletionResultType]::ParameterName, 'Start or join a session')
            [CompletionResult]::new('--session', '--session', [CompletionResultType]::ParameterName, 'Start or join a session')
            [CompletionResult]::new('--empty-session', '--empty-session', [CompletionResultType]::ParameterName, 'Ensure the session is empty')
            [CompletionResult]::new('--save-session', '--save-session', [CompletionResultType]::ParameterName, 'Ensure the new conversation is saved to the session')
            [CompletionResult]::new('-a', '-a', [CompletionResultType]::ParameterName, 'Start a agent')
            [CompletionResult]::new('--agent', '--agent', [CompletionResultType]::ParameterName, 'Start a agent')
            [CompletionResult]::new('--agent-variable', '--agent-variable', [CompletionResultType]::ParameterName, 'Set agent variables')
            [CompletionResult]::new('--rag', '--rag', [CompletionResultType]::ParameterName, 'Start a RAG')
            [CompletionResult]::new('--rebuild-rag', '--rebuild-rag', [CompletionResultType]::ParameterName, 'Rebuild the RAG to sync document changes')
            [CompletionResult]::new('--macro', '--macro', [CompletionResultType]::ParameterName, 'Execute a macro')
            [CompletionResult]::new('--serve', '--serve', [CompletionResultType]::ParameterName, 'Serve the LLM API and WebAPP')
            [CompletionResult]::new('--acp', '--acp', [CompletionResultType]::ParameterName, 'Serve as an ACP agent over stdio')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Include files, directories, or URLs')
            [CompletionResult]::new('--file', '--file', [CompletionResultType]::ParameterName, 'Include files, directories, or URLs')
            [CompletionResult]::new('-S', '-S', [CompletionResultType]::ParameterName, 'Turn off stream mode')
            [CompletionResult]::new('--no-stream', '--no-stream', [CompletionResultType]::ParameterName, 'Turn off stream mode')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Display the message without sending it')
            [CompletionResult]::new('--info', '--info', [CompletionResultType]::ParameterName, 'Display information')
            [CompletionResult]::new('--sync-models', '--sync-models', [CompletionResultType]::ParameterName, 'Sync models updates')
            [CompletionResult]::new('--list-models', '--list-models', [CompletionResultType]::ParameterName, 'List all available chat models')
            [CompletionResult]::new('--list-sessions', '--list-sessions', [CompletionResultType]::ParameterName, 'List all sessions')
            [CompletionResult]::new('--list-agents', '--list-agents', [CompletionResultType]::ParameterName, 'List all agents')
            [CompletionResult]::new('--list-rags', '--list-rags', [CompletionResultType]::ParameterName, 'List all RAGs')
            [CompletionResult]::new('--list-macros', '--list-macros', [CompletionResultType]::ParameterName, 'List all macros')
            [CompletionResult]::new('--mcp-root', '--mcp-root', [CompletionResultType]::ParameterName, 'Add MCP roots')
            [CompletionResult]::new('-t', '-t', [CompletionResultType]::ParameterName, 'Enable tools or toolsets for this session')
            [CompletionResult]::new('--tool', '--tool', [CompletionResultType]::ParameterName, 'Enable tools or toolsets for this session')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            break
        }
    })

    function Get-HarnxValues($arg) {
        $(harnx $arg) -split '\n' | ForEach-Object { [CompletionResult]::new($_) }
    }

    if ($commandElements.Count -gt 1) {
        $offset=2
        if ($wordToComplete -eq "") {
            $offset=1
        }
        $flag = $commandElements[$commandElements.Count-$offset].ToString()
        if ($flag -ceq "-m" -or $flag -eq "--model") {
            $completions = Get-HarnxValues "--list-models"
        } elseif ($flag -ceq "-s" -or $flag -eq "--session") {
            $completions = Get-HarnxValues "--list-sessions"
        } elseif ($flag -ceq "-a" -or $flag -eq "--agent") {
            $completions = Get-HarnxValues "--list-agents"
        } elseif ($flag -eq "--rag") {
            $completions = Get-HarnxValues "--list-rags"
        } elseif ($flag -eq "--macro") {
            $completions = Get-HarnxValues "--list-macros"
        } elseif ($flag -eq "--acp") {
            $completions = Get-HarnxValues "--list-agents"
        } elseif ($flag -ceq "-f" -or $flag -eq "--file") {
            $completions = @()
        } elseif ($flag -eq "--mcp-root") {
            $completions = @()
        } elseif ($flag -ceq "-t" -or $flag -eq "--tool") {
            $completions = @()
        }
    }

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}