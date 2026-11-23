# uXueScript 远程控制命令行客户端
#
# 用法：
#   .\scripts\uxs-remote.ps1 status
#   .\scripts\uxs-remote.ps1 start              # 重载当前课程页并自动确认启动
#   .\scripts\uxs-remote.ps1 start -Url "https://mooc1.chaoxing.com/mycourse/studentstudy?..."
#   .\scripts\uxs-remote.ps1 stop
#   .\scripts\uxs-remote.ps1 logs -Lines 200
#   .\scripts\uxs-remote.ps1 eval -Js "console.log('hi')"
#
# 令牌与地址从 %APPDATA%\top.unraous.uxs\remote.toml 读取，可用 -Base / -Token 覆盖。

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("help", "status", "start", "stop", "reload", "navigate", "eval", "logs", "solve")]
    [string]$Action,
    [string]$Url,
    [string]$Js,
    [string]$PayloadFile,
    [switch]$AllowPartial,
    [int]$Lines = 100,
    [string]$Base,
    [string]$Token
)

$ErrorActionPreference = "Stop"

$configPath = Join-Path $env:APPDATA "top.unraous.uxs\remote.toml"
if (-not $Base -or -not $Token) {
    if (-not (Test-Path -LiteralPath $configPath)) {
        throw "找不到 $configPath，请先启动一次客户端。"
    }
    $raw = Get-Content -LiteralPath $configPath -Raw
    if (-not $Token) {
        $Token = [regex]::Match($raw, 'token\s*=\s*"([^"]+)"').Groups[1].Value
    }
    if (-not $Base) {
        $addr = [regex]::Match($raw, 'addr\s*=\s*"([^"]+)"').Groups[1].Value
        if (-not $addr) { $addr = "127.0.0.1:8765" }
        if ($addr.StartsWith("0.0.0.0")) { $addr = $addr.Replace("0.0.0.0", "127.0.0.1") }
        $Base = "http://$addr"
    }
}
if (-not $Token) { throw "未能从 $configPath 读取 token，请用 -Token 指定。" }

$headers = @{ Authorization = "Bearer $Token" }

function Invoke-Control {
    param([string]$Path, [string]$Method = "GET", $Body)
    $params = @{
        Uri         = "$Base$Path"
        Method      = $Method
        Headers     = $headers
        ContentType = "application/json; charset=utf-8"
    }
    if ($null -ne $Body) {
        $params.Body = ([System.Text.Encoding]::UTF8.GetBytes(($Body | ConvertTo-Json -Depth 20 -Compress)))
    }
    Invoke-RestMethod @params
}

switch ($Action) {
    "help" { Invoke-Control "/" | ConvertTo-Json -Depth 5 }
    "status" { Invoke-Control "/status" | ConvertTo-Json -Depth 5 }
    "start" {
        $body = if ($Url) { @{ url = $Url } } else { @{} }
        Invoke-Control "/start" -Method POST -Body $body | ConvertTo-Json -Depth 5
    }
    "stop" { Invoke-Control "/stop" -Method POST -Body @{} | ConvertTo-Json -Depth 5 }
    "reload" { Invoke-Control "/reload" -Method POST -Body @{} | ConvertTo-Json -Depth 5 }
    "navigate" {
        if (-not $Url) { throw "navigate 需要 -Url" }
        Invoke-Control "/navigate" -Method POST -Body @{ url = $Url } | ConvertTo-Json -Depth 5
    }
    "eval" {
        if (-not $Js) { throw "eval 需要 -Js" }
        Invoke-Control "/eval" -Method POST -Body @{ js = $Js } | ConvertTo-Json -Depth 5
    }
    "logs" { Invoke-Control "/logs?lines=$Lines" | ConvertTo-Json -Depth 5 }
    "solve" {
        if (-not $PayloadFile) { throw "solve 需要 -PayloadFile（Folio JSON 文件）" }
        $payload = Get-Content -LiteralPath $PayloadFile -Raw | ConvertFrom-Json
        Invoke-Control "/solve" -Method POST -Body @{ payload = $payload; allowPartial = [bool]$AllowPartial } |
            ConvertTo-Json -Depth 20
    }
}
