if (-not (Get-Module PSReadLine -ErrorAction SilentlyContinue)) {
    Import-Module PSReadLine -ErrorAction SilentlyContinue
}

# ==========================================
# 1. 全局参数配置区 (所有参数都在这里改)
# ==========================================
$script:PwshHistoryServerUrl = if ($env:PWSH_HISTORY_URL) {
    $env:PWSH_HISTORY_URL.TrimEnd('/')
} elseif ($env:PWSH_HISTORY_BIND -and $env:PWSH_HISTORY_BIND -match ':(\d+)$') {
    "http://127.0.0.1:$($Matches[1])"
} else {
    'http://127.0.0.1:37373'
}

$script:PwshHistoryToken = $env:PWSH_HISTORY_TOKEN

# HTTP 请求超时时间 (秒)
$script:PwshHistoryTimeoutSec = 1
if ($env:PWSH_HISTORY_TIMEOUT_SEC) {
    $parsedTimeout = 0
    if ([int]::TryParse($env:PWSH_HISTORY_TIMEOUT_SEC, [ref]$parsedTimeout) -and $parsedTimeout -gt 0) { $script:PwshHistoryTimeoutSec = $parsedTimeout }
}

# C# 右侧预测器超时时间 (毫秒)
$script:PwshHistoryPredictionTimeoutMs = 250
if ($env:PWSH_HISTORY_PREDICTION_TIMEOUT_MS) {
    $parsedPredictionTimeout = 0
    if ([int]::TryParse($env:PWSH_HISTORY_PREDICTION_TIMEOUT_MS, [ref]$parsedPredictionTimeout) -and $parsedPredictionTimeout -gt 0) { $script:PwshHistoryPredictionTimeoutMs = $parsedPredictionTimeout }
}

# 【新增】C# 预测器向下寻找可用单行历史的最大拉取数量
$script:PwshHistoryPredictionLimit = 10

# 纯底层 HttpClient 初始化
$script:PwshHistoryClient = [System.Net.Http.HttpClient]::new()
$script:PwshHistoryClient.Timeout = [System.TimeSpan]::FromSeconds($script:PwshHistoryTimeoutSec)
if (-not [string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) {
    $script:PwshHistoryClient.DefaultRequestHeaders.TryAddWithoutValidation('X-Pwsh-History-Token', $script:PwshHistoryToken) | Out-Null
}

$script:PwshHistorySearchState = @{ Prefix = $null; Matches = @(); Index = -1 }

# ==========================================
# 2. 原生回退与本地内存替换逻辑
# ==========================================
function Invoke-PwshHistoryFallback {
    param([string]$Direction, $Key, $Arg)
    try {
        if ($Direction -eq 'Backward') { [Microsoft.PowerShell.PSConsoleReadLine]::HistorySearchBackward($Key, $Arg) } 
        else { [Microsoft.PowerShell.PSConsoleReadLine]::HistorySearchForward($Key, $Arg) }
    } catch {
        try {
            if ($Direction -eq 'Backward') { [Microsoft.PowerShell.PSConsoleReadLine]::PreviousHistory($Key, $Arg) } 
            else { [Microsoft.PowerShell.PSConsoleReadLine]::NextHistory($Key, $Arg) }
        } catch { }
    }
}

function Get-PwshHistoryBuffer {
    $line = $null; $cursor = 0
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
    [pscustomobject]@{ Line = if ($null -eq $line) { '' } else { $line }; Cursor = $cursor }
}

function Set-PwshHistoryBuffer {
    param([string]$Line = '')
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
    param([string]$Prefix = '', [int]$Limit = 100)
    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) { return @() }
    try {
        $encodedPrefix = [uri]::EscapeDataString($Prefix)
        $uri = "$script:PwshHistoryServerUrl/v1/history/search?prefix=$encodedPrefix&limit=$Limit"
        $json = $script:PwshHistoryClient.GetStringAsync($uri).GetAwaiter().GetResult()
        $doc = [System.Text.Json.JsonDocument]::Parse($json)
        try {
            $entries = $doc.RootElement.GetProperty("entries")
            $result = @()
            foreach ($entry in $entries.EnumerateArray()) { $result += $entry.GetProperty("command").GetString() }
            return $result
        } catch { }
    } catch { }
    return @()
}

function Add-PwshHistoryServer {
    param([string]$Command)
    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken) -or [string]::IsNullOrWhiteSpace($Command)) { return }
    try {
        $body = @{ command = $Command } | ConvertTo-Json -Compress
        $content = [System.Net.Http.StringContent]::new($body, [System.Text.Encoding]::UTF8, "application/json")
        $null = $script:PwshHistoryClient.PostAsync("$script:PwshHistoryServerUrl/v1/history/add", $content)
    } catch { }
}

# ==========================================
# 3. 智能按需编译 & 注册预测器
# ==========================================
function Register-PwshHistoryPredictor {
    if ([string]::IsNullOrWhiteSpace($script:PwshHistoryToken)) { return }

    $dllDir = Split-Path $PROFILE
    if (-not (Test-Path $dllDir)) { New-Item -ItemType Directory -Path $dllDir -Force | Out-Null }
    $dllPath = Join-Path $dllDir "PwshHistoryPredictor.dll"

    # 【智能编译】：如果没有找到 DLL，说明是初次运行或者用户主动删除了它
    if (-not (Test-Path $dllPath)) {
        Write-Host " [PwshHistory] Building C# Predictor DLL for the first time..." -ForegroundColor DarkGray
        
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
    private readonly int limit;

    // 【新增】：将 limit 也放入构造函数暴露出来
    public PwshHistoryServerPredictor(string baseUrl, string token, int timeoutMs, int limit)
    {
        this.baseUrl = (baseUrl ?? "").TrimEnd('/');
        this.token = token ?? "";
        this.timeoutMs = timeoutMs <= 0 ? 250 : timeoutMs;
        this.limit = limit <= 0 ? 10 : limit;
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
            if (string.IsNullOrWhiteSpace(baseUrl) || string.IsNullOrWhiteSpace(token)) return new SuggestionPackage(suggestions);

            string input = context == null || context.InputAst == null || context.InputAst.Extent == null ? "" : context.InputAst.Extent.Text ?? "";
            int lastNewline = input.LastIndexOf('\n');
            if (lastNewline >= 0) { input = input.Substring(lastNewline + 1); }

            // 动态拼接 limit
            string uri = baseUrl + "/v1/history/search?prefix=" + Uri.EscapeDataString(input) + "&limit=" + this.limit;
            using (var request = new HttpRequestMessage(HttpMethod.Get, uri))
            {
                request.Headers.TryAddWithoutValidation("X-Pwsh-History-Token", token);
                using (var cts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
                {
                    cts.CancelAfter(timeoutMs);
                    using (var response = Client.SendAsync(request, cts.Token).GetAwaiter().GetResult())
                    {
                        if (!response.IsSuccessStatusCode) { return new SuggestionPackage(suggestions); }
                        string json = response.Content.ReadAsStringAsync().GetAwaiter().GetResult();
                        using (var document = JsonDocument.Parse(json))
                        {
                            if (!document.RootElement.TryGetProperty("entries", out JsonElement entries) || entries.ValueKind != JsonValueKind.Array)
                            {
                                return new SuggestionPackage(suggestions);
                            }

                            foreach (JsonElement entry in entries.EnumerateArray())
                            {
                                if (entry.TryGetProperty("command", out JsonElement commandElement))
                                {
                                    string command = commandElement.GetString();
                                    if (!string.IsNullOrWhiteSpace(command) && !string.Equals(command, input, StringComparison.Ordinal))
                                    {
                                        if (command.IndexOf('\n') >= 0 || command.IndexOf('\r') >= 0) continue;
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
        catch { }
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
            Add-Type -TypeDefinition $predictorSource -OutputAssembly $dllPath -OutputType Library
        } catch {
            Write-Warning "Failed to compile DLL: $($_.Exception.Message)"
            return
        }
    }

    # 加载编译好的 DLL
    try {
        Add-Type -Path $dllPath -ErrorAction Stop
        
        try {
            [System.Management.Automation.Subsystem.SubsystemManager]::UnregisterSubsystem(
                [System.Management.Automation.Subsystem.SubsystemKind]::CommandPredictor,
                [PwshHistoryServerPredictor]::PredictorId
            )
        } catch { }

        # 实例化时，将我们上面定义的全局变量注入进去
        $predictor = [PwshHistoryServerPredictor]::new(
            $script:PwshHistoryServerUrl,
            $script:PwshHistoryToken,
            $script:PwshHistoryPredictionTimeoutMs,
            $script:PwshHistoryPredictionLimit
        )
        
        [System.Management.Automation.Subsystem.SubsystemManager]::RegisterSubsystem(
            [System.Management.Automation.Subsystem.SubsystemKind]::CommandPredictor,
            $predictor
        )
        Set-PSReadLineOption -PredictionSource HistoryAndPlugin -PredictionViewStyle InlineView
    } catch { }
}

# ==========================================
# 4. 按键绑定
# ==========================================
function Invoke-PwshHistoryPrefixSearch {
    param([string]$Direction, $Key, $Arg)
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
        if ($index -lt ($matches.Count - 1)) { $index += 1 }
    } else {
        if ($index -gt 0) { $index -= 1 } 
        else {
            $script:PwshHistorySearchState['Index'] = -1
            Set-PwshHistoryBuffer -Line ([string]$script:PwshHistorySearchState['Prefix'])
            return
        }
    }
    $script:PwshHistorySearchState['Index'] = $index
    Set-PwshHistoryBuffer -Line $matches[$index]
}

try { Set-PSReadLineOption -HistorySaveStyle SaveNothing } catch { }

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
