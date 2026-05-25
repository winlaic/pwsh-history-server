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

$script:PwshHistoryPredictionTimeoutMs = 250
if ($env:PWSH_HISTORY_PREDICTION_TIMEOUT_MS) {
    $parsedPredictionTimeout = 0
    if ([int]::TryParse($env:PWSH_HISTORY_PREDICTION_TIMEOUT_MS, [ref]$parsedPredictionTimeout) -and $parsedPredictionTimeout -gt 0) {
        $script:PwshHistoryPredictionTimeoutMs = $parsedPredictionTimeout
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
    param([AllowEmptyString()][string]$Line = '')

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

function Register-PwshHistoryPredictor {
    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) {
        return
    }

    if (-not ('PwshHistoryServerPredictor' -as [type])) {
        $predictorSource = @'
using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Text.Json;
using System.Threading;
using System.Management.Automation.Subsystem;
using System.Management.Automation.Subsystem.Prediction;

public sealed class PwshHistoryServerPredictor : ICommandPredictor
{
    public static readonly Guid PredictorId = new Guid("c56c3b61-4d55-4e9c-8a47-375ac5d42f21");
    private static readonly HttpClient Client = new HttpClient();
    private readonly string baseUrl;
    private readonly string token;
    private readonly int timeoutMs;

    public PwshHistoryServerPredictor(string baseUrl, string token, int timeoutMs)
    {
        this.baseUrl = (baseUrl ?? "").TrimEnd('/');
        this.token = token ?? "";
        this.timeoutMs = timeoutMs <= 0 ? 250 : timeoutMs;
    }

    public Guid Id { get { return PredictorId; } }
    public string Name { get { return "pwsh-history-server"; } }
    public string Description { get { return "Predicts commands from pwsh-history-server."; } }
    public Dictionary<string, string> FunctionsToDefine { get { return new Dictionary<string, string>(); } }

    public SuggestionPackage GetSuggestion(PredictionClient client, PredictionContext context, CancellationToken cancellationToken)
    {
        var suggestions = new List<PredictiveSuggestion>();
        try
        {
            if (string.IsNullOrWhiteSpace(baseUrl) || string.IsNullOrWhiteSpace(token))
            {
                return new SuggestionPackage(suggestions);
            }

            string input = context == null || context.InputAst == null || context.InputAst.Extent == null
                ? ""
                : context.InputAst.Extent.Text ?? "";
            int lastNewline = input.LastIndexOf('\n');
            if (lastNewline >= 0)
            {
                input = input.Substring(lastNewline + 1);
            }

            string uri = baseUrl + "/v1/history/search?prefix=" + Uri.EscapeDataString(input) + "&limit=1";
            using (var request = new HttpRequestMessage(HttpMethod.Get, uri))
            {
                request.Headers.TryAddWithoutValidation("X-Pwsh-History-Token", token);
                using (var cts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
                {
                    cts.CancelAfter(timeoutMs);
                    using (var response = Client.SendAsync(request, cts.Token).GetAwaiter().GetResult())
                    {
                        if (!response.IsSuccessStatusCode)
                        {
                            return new SuggestionPackage(suggestions);
                        }

                        string json = response.Content.ReadAsStringAsync().GetAwaiter().GetResult();
                        using (var document = JsonDocument.Parse(json))
                        {
                            JsonElement entries;
                            if (!document.RootElement.TryGetProperty("entries", out entries) || entries.ValueKind != JsonValueKind.Array)
                            {
                                return new SuggestionPackage(suggestions);
                            }

                            foreach (JsonElement entry in entries.EnumerateArray())
                            {
                                JsonElement commandElement;
                                if (entry.TryGetProperty("command", out commandElement))
                                {
                                    string command = commandElement.GetString();
                                    if (!string.IsNullOrWhiteSpace(command) && !string.Equals(command, input, StringComparison.Ordinal))
                                    {
                                        suggestions.Add(new PredictiveSuggestion(command));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        catch
        {
        }

        return new SuggestionPackage(suggestions);
    }

    public bool CanAcceptFeedback(PredictionClient client, PredictorFeedbackKind feedback) { return false; }
    public void OnSuggestionDisplayed(PredictionClient client, uint session, int countOrIndex) { }
    public void OnSuggestionAccepted(PredictionClient client, uint session, string acceptedSuggestion) { }
    public void OnCommandLineAccepted(PredictionClient client, IReadOnlyList<string> history) { }
    public void OnCommandLineExecuted(PredictionClient client, string commandLine, bool success) { }
}
'@
        try {
            Add-Type -TypeDefinition $predictorSource -ErrorAction Stop
        } catch {
            return
        }
    }

    try {
        [System.Management.Automation.Subsystem.SubsystemManager]::UnregisterSubsystem(
            [System.Management.Automation.Subsystem.SubsystemKind]::CommandPredictor,
            [PwshHistoryServerPredictor]::PredictorId
        )
    } catch {
    }

    try {
        $predictor = [PwshHistoryServerPredictor]::new(
            $script:PwshHistoryServerUrl,
            $script:PwshHistoryToken,
            $script:PwshHistoryPredictionTimeoutMs
        )
        [System.Management.Automation.Subsystem.SubsystemManager]::RegisterSubsystem(
            [System.Management.Automation.Subsystem.SubsystemKind]::CommandPredictor,
            $predictor
        )
        Set-PSReadLineOption -PredictionSource HistoryAndPlugin -PredictionViewStyle InlineView
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
        if ($Direction -eq 'Forward') {
            Reset-PwshHistorySearchState
            Invoke-PwshHistoryFallback -Direction $Direction -Key $Key -Arg $Arg
            return
        }

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

try {
    Set-PSReadLineOption -HistorySaveStyle SaveNothing
} catch {
}

Register-PwshHistoryPredictor

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
