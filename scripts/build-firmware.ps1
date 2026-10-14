[CmdletBinding()]
param([switch]$PrepareOnly)

$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path $PSScriptRoot -Parent
$taskEnvFile = Join-Path $taskRoot '.env.local'
if (-not (Test-Path -LiteralPath $taskEnvFile)) {
    throw '需要本地 .env.local；参照 .env.example 配置 AUTO_SHUTDOWN_RADIO_KEY'
}
$taskValues = @(foreach ($taskLine in Get-Content -LiteralPath $taskEnvFile) {
    if ($taskLine -cmatch '^\s*AUTO_SHUTDOWN_RADIO_KEY\s*=(.*)$') { $matches[1].Trim() }
})
if ($taskValues.Count -ne 1 -or $taskValues[0] -notmatch '\A[0-9a-fA-F]{64}\z' -or $taskValues[0] -match '\A0{64}\z') {
    throw 'AUTO_SHUTDOWN_RADIO_KEY 必须只配置一次，值为 64 位十六进制随机密钥（非全零、不带引号）'
}
$taskBytes = for ($i = 0; $i -lt 64; $i += 2) { '0x' + $taskValues[0].Substring($i, 2) }
# 在原始工程旁生成已被 Git 忽略的密钥头，让 Arduino IDE 与命令行使用同一工程。
$taskSketch = Join-Path $taskRoot 'ESP8266'
$taskHeader = "#pragma once`n#include <stdint.h>`nstatic const uint8_t RADIO_KEY[32] = {" + ($taskBytes -join ',') + "};`n"
[IO.File]::WriteAllText((Join-Path $taskSketch 'radio_key.local.h'), $taskHeader)
if ($PrepareOnly) {
    Write-Output "已生成本地密钥头，可在 Arduino IDE 中打开: $(Join-Path $taskSketch 'ESP8266.ino')"
    return
}
$taskCli = Get-Command arduino-cli -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1
if (-not $taskCli) {
    $taskBundledCli = Join-Path $env:ProgramFiles 'Arduino IDE/resources/app/lib/backend/resources/arduino-cli.exe'
    if (Test-Path -LiteralPath $taskBundledCli) { $taskCli = $taskBundledCli }
}
if (-not $taskCli) { throw '请安装 arduino-cli 或 Arduino IDE，或用 -PrepareOnly 生成工程后在 IDE 中编译' }
& $taskCli compile --fqbn esp8266:esp8266:nodemcuv2 --build-path (Join-Path $taskRoot 'target/esp8266-radio-only-3') $taskSketch
if ($LASTEXITCODE -ne 0) { throw 'ESP8266 固件构建失败' }
