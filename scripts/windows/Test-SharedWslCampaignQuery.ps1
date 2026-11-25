#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-SharedWslCampaignQuery.ps1
param()
$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot 'Invoke-SharedWslPressureCampaign.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'campaign_query_source_parser_error' }
foreach ($name in @('Stop-CampaignProcessInstanceSafely', 'Invoke-BoundedCampaignCimQuery')) {
    $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true)
    if ($null -eq $definition) { throw "campaign_query_function_missing:$name" }
    Invoke-Expression $definition.Extent.Text
}
$result = Invoke-BoundedCampaignCimQuery -Query '[pscustomobject]@{ marker = "native_query_result" } | ConvertTo-Json -Compress'
if ($result.marker -cne 'native_query_result') { throw 'campaign_query_native_output_not_delivered' }
$failed = $false
try { Invoke-BoundedCampaignCimQuery -Query 'throw "fixture_query_failure"' | Out-Null }
catch { $failed = $_.Exception.Message -eq 'campaign_cim_query_failed' }
if (-not $failed) { throw 'campaign_query_nonzero_not_refused' }
$clock = [Diagnostics.Stopwatch]::StartNew()
$timedOut = $false
try { Invoke-BoundedCampaignCimQuery -Query 'Start-Sleep -Seconds 30' -TimeoutSeconds 1 | Out-Null }
catch { $timedOut = $_.Exception.Message -eq 'campaign_cim_query_deadline_exceeded' }
if (-not $timedOut -or $clock.Elapsed.TotalSeconds -gt 8) { throw 'campaign_query_timeout_or_containment_unproven' }
$queryDefinition = ${function:Invoke-BoundedCampaignCimQuery}.ToString()
$stopDefinition = ${function:Stop-CampaignProcessInstanceSafely}.ToString()
$job = Start-Job -ArgumentList $queryDefinition, $stopDefinition -ScriptBlock {
    param($QueryDefinition, $StopDefinition)
    $ErrorActionPreference = 'Stop'
    Set-Item Function:\script:Invoke-BoundedCampaignCimQuery ([scriptblock]::Create($QueryDefinition))
    Set-Item Function:\script:Stop-CampaignProcessInstanceSafely ([scriptblock]::Create($StopDefinition))
    $result = Invoke-BoundedCampaignCimQuery -Query '[pscustomobject]@{ marker = "background_result" } | ConvertTo-Json -Compress'
    if ($result.marker -cne 'background_result') { throw 'background_query_output_invalid' }
    try { Invoke-BoundedCampaignCimQuery -Query 'Start-Sleep -Seconds 30' -TimeoutSeconds 1; throw 'background_timeout_not_enforced' }
    catch { if ($_.Exception.Message -ne 'campaign_cim_query_deadline_exceeded') { throw } }
    return 'background_query_and_containment_passed'
}
try {
    if ($null -eq (Wait-Job $job -Timeout 15)) { throw 'background_query_fixture_deadline' }
    $answer = @(Receive-Job $job -ErrorAction Stop)
    if ($answer.Count -ne 1 -or $answer[0] -cne 'background_query_and_containment_passed') { throw 'background_query_or_containment_failed' }
} finally { Remove-Job $job -Force }
$start = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq 'Start-HostDiskTelemetry' }, $true).Extent.Text
if (-not $start.Contains('$containmentFunction') -or -not $start.Contains('Function:\script:Stop-CampaignProcessInstanceSafely')) {
    throw 'disk_sampler_does_not_forward_native_process_containment'
}
$marker = Join-Path ([IO.Path]::GetTempPath()) ('ramshared-query-' + [Guid]::NewGuid().ToString('N') + '.pid')
$cancelled = [PowerShell]::Create()
$ownedChild = $null
try {
    [void]$cancelled.AddScript({
        param($QueryDefinition, $StopDefinition, $Marker)
        Set-Item Function:\script:Invoke-BoundedCampaignCimQuery ([scriptblock]::Create($QueryDefinition))
        Set-Item Function:\script:Stop-CampaignProcessInstanceSafely ([scriptblock]::Create($StopDefinition))
        $safePath = $Marker.Replace("'", "''")
        Invoke-BoundedCampaignCimQuery -TimeoutSeconds 2 -Query ('$PID | Set-Content -LiteralPath ''' + $safePath + '''; Start-Sleep -Seconds 30')
    }.ToString()).AddArgument($queryDefinition).AddArgument($stopDefinition).AddArgument($marker)
    $active = $cancelled.BeginInvoke()
    $limit = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path $marker) -and $limit.Elapsed.TotalSeconds -lt 5) { Start-Sleep -Milliseconds 50 }
    if (-not (Test-Path $marker)) { throw 'cancelled_query_child_identity_not_observed' }
    $ownedChild = Get-Process -Id ([int](Get-Content $marker -Raw)) -ErrorAction Stop
    $pinnedHandle = $ownedChild.Handle
    $stop = $cancelled.BeginStop($null, $null)
    if (-not $active.AsyncWaitHandle.WaitOne(10000)) { throw 'cancelled_query_did_not_settle' }
    try { $cancelled.EndInvoke($active) | Out-Null } catch {}
    if (-not $ownedChild.WaitForExit(5000)) { throw 'cancelled_query_orphaned_its_owned_child' }
} finally {
    if ($null -ne $ownedChild) { $ownedChild.Dispose() }
    $cancelled.Dispose()
    Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
}
Write-Host 'SHARED_WSL_CAMPAIGN_NATIVE_QUERY=PASS'
