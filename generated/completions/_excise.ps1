
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'excise' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'excise'
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
        'excise' {
            [CompletionResult]::new('--config', '--config', [CompletionResultType]::ParameterName, 'Read configuration from FILE')
            [CompletionResult]::new('--scan-threads', '--scan-threads', [CompletionResultType]::ParameterName, 'Scanner worker count (1-32)')
            [CompletionResult]::new('--event-buffer', '--event-buffer', [CompletionResultType]::ParameterName, 'Bounded worker-event capacity (16-4096)')
            [CompletionResult]::new('--exclude', '--exclude', [CompletionResultType]::ParameterName, 'Ordered gitignore-style exclusion pattern')
            [CompletionResult]::new('--memory-mib', '--memory-mib', [CompletionResultType]::ParameterName, 'Whole-process memory envelope in MiB')
            [CompletionResult]::new('--theme', '--theme', [CompletionResultType]::ParameterName, 'Built-in semantic color theme')
            [CompletionResult]::new('--keymap', '--keymap', [CompletionResultType]::ParameterName, 'Keyboard preset. Arrows and safety keys always work')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Output mode. Table and JSON never acquire a terminal')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Write a noninteractive report to FILE instead of stdout')
            [CompletionResult]::new('-a', '-a', [CompletionResultType]::ParameterName, 'Show apparent file sizes instead of allocated bytes')
            [CompletionResult]::new('--apparent-size', '--apparent-size', [CompletionResultType]::ParameterName, 'Show apparent file sizes instead of allocated bytes')
            [CompletionResult]::new('--cross-filesystems', '--cross-filesystems', [CompletionResultType]::ParameterName, 'Permit traversal across filesystem boundaries')
            [CompletionResult]::new('--reduced-motion', '--reduced-motion', [CompletionResultType]::ParameterName, 'Disable nonessential motion')
            [CompletionResult]::new('--ascii', '--ascii', [CompletionResultType]::ParameterName, 'Use ASCII-only symbols and borders')
            [CompletionResult]::new('--mouse', '--mouse', [CompletionResultType]::ParameterName, 'Enable mouse capture and selection')
            [CompletionResult]::new('-d', '-d', [CompletionResultType]::ParameterName, 'Do not ask for confirmation before deleting')
            [CompletionResult]::new('--disable-delete-confirmation', '--disable-delete-confirmation', [CompletionResultType]::ParameterName, 'Do not ask for confirmation before deleting')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
