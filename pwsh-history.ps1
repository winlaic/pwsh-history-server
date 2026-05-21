if (-not (Get-Module PSReadLine -ErrorAction SilentlyContinue)) {
    Import-Module PSReadLine -ErrorAction SilentlyContinue
}

$script:PwshHistoryServerUrl = if ($env:PWSH_HISTORY_URL) {
    $env:PWSH_HISTORY_URL.TrimEnd('/')
} elseif ($env:PWSH_HISTORY_BIND -and $env:PWSH_HISTORY_BIND -match ':(\d+)$') {
    "http://127.0.0.1:$($Matches[1])"
} else {
    'http://127.0.0.1:37373'
}

$script:PwshHistoryToken = $env:PWSH_HISTORY_TOKEN
$script:PwshHistoryTimeoutSec = 1
if ($env:PWSH_HISTORY_TIMEOUT_SEC) {
    $parsedTimeout = 0
    if ([int]::TryParse($env:PWSH_HISTORY_TIMEOUT_SEC, [ref]$parsedTimeout) -and $parsedTimeout -gt 0) {
        $script:PwshHistoryTimeoutSec = $parsedTimeout
    }
}

$script:PwshHistorySearchState = @{
    Prefix  = $null
    Matches = @()
    Index   = -1
}

function Invoke-PwshHistoryFallback {
    param(
        [Parameter(Mandatory)]
        [ValidateSet('Backward', 'Forward')]
        [string]$Direction,

        [Parameter(Mandatory)]
        $Key,

        $Arg
    )

    try {
        if ($Direction -eq 'Backward') {
            [Microsoft.PowerShell.PSConsoleReadLine]::HistorySearchBackward($Key, $Arg)
        } else {
            [Microsoft.PowerShell.PSConsoleReadLine]::HistorySearchForward($Key, $Arg)
        }
    } catch {
        try {
            if ($Direction -eq 'Backward') {
                [Microsoft.PowerShell.PSConsoleReadLine]::PreviousHistory($Key, $Arg)
            } else {
                [Microsoft.PowerShell.PSConsoleReadLine]::NextHistory($Key, $Arg)
            }
        } catch {
        }
    }
}

function Get-PwshHistoryBuffer {
    $line = $null
    $cursor = 0
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)

    [pscustomobject]@{
        Line   = if ($null -eq $line) { '' } else { $line }
        Cursor = $cursor
    }
}

function Set-PwshHistoryBuffer {
    param([Parameter(Mandatory)][string]$Line)

    $buffer = Get-PwshHistoryBuffer
    [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $buffer.Line.Length, $Line)
    [Microsoft.PowerShell.PSConsoleReadLine]::SetCursorPosition($Line.Length)
}

function Reset-PwshHistorySearchState {
    $script:PwshHistorySearchState['Prefix'] = $null
    $script:PwshHistorySearchState['Matches'] = @()
    $script:PwshHistorySearchState['Index'] = -1
}

function Search-PwshHistoryServer {
    param(
        [AllowEmptyString()][string]$Prefix = '',
        [int]$Limit = 100
    )

    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) {
        return @()
    }

    $encodedPrefix = [uri]::EscapeDataString($Prefix)
    $uri = "$script:PwshHistoryServerUrl/v1/history/search?prefix=$encodedPrefix&limit=$Limit"
    $headers = @{ 'X-Pwsh-History-Token' = $script:PwshHistoryToken }

    try {
        $response = Invoke-RestMethod -Method Get -Uri $uri -Headers $headers -TimeoutSec $script:PwshHistoryTimeoutSec
        if ($null -eq $response.entries) {
            return @()
        }

        return @($response.entries | ForEach-Object { $_.command })
    } catch {
        return @()
    }
}

function Add-PwshHistoryServer {
    param([Parameter(Mandatory)][string]$Command)

    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) {
        return
    }

    if ([string]::IsNullOrWhiteSpace($Command)) {
        return
    }

    $headers = @{ 'X-Pwsh-History-Token' = $script:PwshHistoryToken }
    $body = @{ command = $Command } | ConvertTo-Json -Compress

    try {
        Invoke-RestMethod `
            -Method Post `
            -Uri "$script:PwshHistoryServerUrl/v1/history/add" `
            -Headers $headers `
            -Body $body `
            -ContentType 'application/json' `
            -TimeoutSec $script:PwshHistoryTimeoutSec | Out-Null
    } catch {
    }
}

function Invoke-PwshHistoryPrefixSearch {
    param(
        [Parameter(Mandatory)]
        [ValidateSet('Backward', 'Forward')]
        [string]$Direction,

        [Parameter(Mandatory)]
        $Key,

        $Arg
    )

    $buffer = Get-PwshHistoryBuffer
    $matches = @($script:PwshHistorySearchState['Matches'])
    $index = [int]$script:PwshHistorySearchState['Index']
    $isCurrentServerMatch = $index -ge 0 -and $index -lt $matches.Count -and $buffer.Line -eq $matches[$index]

    if (-not $isCurrentServerMatch) {
        $prefix = $buffer.Line.Substring(0, $buffer.Cursor)
        $matches = @(Search-PwshHistoryServer -Prefix $prefix -Limit 100)

        if ($matches.Count -eq 0) {
            Reset-PwshHistorySearchState
            Invoke-PwshHistoryFallback -Direction $Direction -Key $Key -Arg $Arg
            return
        }

        $script:PwshHistorySearchState['Prefix'] = $prefix
        $script:PwshHistorySearchState['Matches'] = $matches
        $script:PwshHistorySearchState['Index'] = 0
        Set-PwshHistoryBuffer -Line $matches[0]
        return
    }

    if ($Direction -eq 'Backward') {
        if ($index -lt ($matches.Count - 1)) {
            $index += 1
        }
    } else {
        if ($index -gt 0) {
            $index -= 1
        } else {
            $script:PwshHistorySearchState['Index'] = -1
            Set-PwshHistoryBuffer -Line ([string]$script:PwshHistorySearchState['Prefix'])
            return
        }
    }

    $script:PwshHistorySearchState['Index'] = $index
    Set-PwshHistoryBuffer -Line $matches[$index]
}

Set-PSReadLineOption -EditMode Emacs
Set-PSReadLineOption -HistorySearchCursorMovesToEnd
Set-PSReadLineOption -MaximumHistoryCount 100000
Set-PSReadLineOption -HistoryNoDuplicates
Set-PSReadLineKeyHandler -Chord Tab -Function MenuComplete
Set-PSReadLineOption -BellStyle None

try {
    Set-PSReadLineOption -HistorySaveStyle SaveNothing
} catch {
}

Set-PSReadLineOption -AddToHistoryHandler {
    param([string]$line)

    Reset-PwshHistorySearchState
    Add-PwshHistoryServer -Command $line
    return $true
}

Set-PSReadLineKeyHandler -Chord UpArrow -ScriptBlock {
    param($key, $arg)
    Invoke-PwshHistoryPrefixSearch -Direction Backward -Key $key -Arg $arg
}

Set-PSReadLineKeyHandler -Chord DownArrow -ScriptBlock {
    param($key, $arg)
    Invoke-PwshHistoryPrefixSearch -Direction Forward -Key $key -Arg $arg
}
