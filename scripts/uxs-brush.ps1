param(
    [string]$Exe = "C:\Users\Administrator\Desktop\uxs-2.5.12-control-win-x64.exe",
    [string]$Token = "ca231f1b0ae6ea03d386f32a27eaa361",
    [string]$ChapterUrl = "https://mooc1.chaoxing.com/mycourse/studentstudy?chapterId=1174005903&courseId=263971860&clazzid=159507448&cpi=559409535&enc=76cd3b6497232b5e44ca45a430b805a2&mooc2=1&hidetype=0&fanyaVersion=0",
    [switch]$Restart,
    [switch]$StartCourse
)
# uXueScript 远程控制一键脚本：确保客户端在跑 -> 打开课程 -> 启动自动化 -> 打印状态
$base = "http://127.0.0.1:8765"
$h = @{ Authorization = "Bearer $Token" }

function Test-Api {
    try { $r = Invoke-RestMethod "$base/health" -Headers $h -TimeoutSec 5; return $true } catch { return $false }
}

if (-not (Test-Api)) {
    if ($Restart) {
        Get-Process | Where-Object { $_.ProcessName -like '*control-win-x64*' } | ForEach-Object { $_.Kill(); Start-Sleep -Seconds 2 }
        Start-Sleep -Seconds 3
    }
    if (-not (Get-Process | Where-Object { $_.ProcessName -like '*control-win-x64*' })) {
        Start-Process $Exe
        Write-Host "已启动客户端，等待控制接口..."
        for ($i = 0; $i -lt 60; $i++) { Start-Sleep -Seconds 2; if (Test-Api) { break } }
    }
}

if (-not (Test-Api)) { Write-Error "控制接口 $base 无响应，检查 remote.toml（enabled=true）或查看日志"; exit 1 }
Write-Host "控制接口正常"

# 确认弹窗默认 yes
Invoke-RestMethod "$base/autoconfirm" -Method POST -Headers $h -ContentType "application/json" -Body '{"mode":"yes"}' | Out-Null
Write-Host "确认策略=yes（5 秒弹窗自动点 Yes）"

if ($ChapterUrl) {
    Invoke-RestMethod "$base/navigate" -Method POST -Headers $h -ContentType "application/json" -Body (@{ url = $ChapterUrl } | ConvertTo-Json -Compress) | Out-Null
    Write-Host "已打开课程章节页"
    Start-Sleep -Seconds 12
}

if ($StartCourse) {
    Invoke-RestMethod "$base/start" -Method POST -Headers $h -ContentType "application/json" -Body '{}' | Out-Null
    Write-Host "已启动课程自动化"
    Start-Sleep -Seconds 10
}

$s = Invoke-RestMethod "$base/status" -Headers $h
Write-Host ("状态: active={0} url={1} confirm={2}" -f $s.courseActive, $s.url, $s.confirmMode)
Write-Host "日志: $(($base + '/logs?lines=30'))"
