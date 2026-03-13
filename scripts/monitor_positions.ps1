# Monitor for positions + opportunities — run in separate PowerShell window
# Usage: powershell -ExecutionPolicy Bypass -Command "& '\\wsl.localhost\Ubuntu\home\andydoc\prediction-trader\scripts\monitor_positions.ps1'"
# Or double-click P:\scripts\monitor.bat

$stateUrl = "http://localhost:5556/state"
$interval = 30
$lastOpen = 0
$lastOppHash = ""

Write-Host "=== Prediction Trader Monitor ===" -ForegroundColor Cyan
Write-Host "Checking every ${interval}s... Press Ctrl+C to stop.`n"

while ($true) {
    $ts = Get-Date -Format "HH:mm:ss"
    try {
        $resp = Invoke-RestMethod -Uri $stateUrl -TimeoutSec 5
        $cash = [math]::Round($resp.current_capital, 2)
        $nOpen = $resp.open_positions.Count
        $nClosed = $resp.closed_positions.Count

        # Position change detection
        if ($nOpen -gt $lastOpen) {
            $newCount = $nOpen - $lastOpen
            Write-Host "`n*** $newCount NEW POSITION(S) at $ts ***" -ForegroundColor Green
            Write-Host "  Cash: `$$cash  Open: $nOpen  Closed: $nClosed" -ForegroundColor Yellow
            foreach ($p in $resp.open_positions) {
                $markets = $p.markets.PSObject.Properties
                $method = $p.metadata.method
                $first = $true
                foreach ($m in $markets) {
                    $leg = $m.Value
                    $name = $leg.name.Substring(0, [Math]::Min(45, $leg.name.Length))
                    $sh = $leg.shares
                    $bet = [math]::Round($leg.bet_amount, 4)
                    $pp = if ($sh -gt 0) { [math]::Round($bet / $sh, 4) } else { "?" }
                    if ($first) {
                        Write-Host "  [$method] $name" -ForegroundColor Cyan
                        $first = $false
                    }
                    Write-Host "    `$$bet  $sh sh  @$pp  $name" -ForegroundColor White
                }
            }
            [console]::beep(800, 200); [console]::beep(1000, 200)
            $lastOpen = $nOpen
        }
        elseif ($nOpen -lt $lastOpen) {
            Write-Host "`n$ts  POSITION CLOSED  cash=`$$cash  open=$nOpen  closed=$nClosed" -ForegroundColor Yellow
            $lastOpen = $nOpen
        }

        # Check opportunities from live dashboard (recent_opps in state snapshot)
        $oppCount = 0
        $oppHash = ""
        if ($resp.recent_opps) {
            $oppCount = $resp.recent_opps.Count
            # Build hash from first opp name to detect changes
            if ($oppCount -gt 0) {
                $oppHash = "$oppCount-$($resp.recent_opps[0].constraint_id)"
            }
        }

        if ($oppCount -gt 0 -and $oppHash -ne $lastOppHash) {
            Write-Host ""
            Write-Host "  >> $oppCount opportunity(ies) found:" -ForegroundColor Magenta
            foreach ($o in $resp.recent_opps) {
                $pct = [math]::Round($o.expected_profit_pct * 100, 1)
                $hrs = if ($o.hours_to_resolve) { [math]::Round($o.hours_to_resolve, 1) } else { "?" }
                $meth = $o.metadata.method
                $names = $o.market_names
                $first = if ($names -and $names.Count -gt 0) { $names[0].Substring(0, [Math]::Min(50, $names[0].Length)) } else { "?" }
                Write-Host "     ${pct}%  ${hrs}h  $meth  $first" -ForegroundColor Magenta
            }
            [console]::beep(600, 150)
            $lastOppHash = $oppHash
            Write-Host "$ts  cash=`$$cash  open=$nOpen  closed=$nClosed  opps=$oppCount" -ForegroundColor DarkGray
        }
        elseif ($oppCount -eq 0 -and $lastOppHash -ne "") {
            $lastOppHash = ""
            Write-Host "$ts  cash=`$$cash  open=$nOpen  closed=$nClosed  opps=0" -ForegroundColor DarkGray
        }
        else {
            Write-Host "$ts  cash=`$$cash  open=$nOpen  closed=$nClosed  opps=$oppCount" -ForegroundColor DarkGray
        }
    }
    catch {
        Write-Host "$ts  Dashboard unreachable" -ForegroundColor Red
    }
    Start-Sleep -Seconds $interval
}
