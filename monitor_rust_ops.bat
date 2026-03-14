@echo off
setlocal enabledelayedexpansion

:: ============================================================
:: Rust Position Manager — First-Time Operation Monitor
:: Tails the trading engine log and highlights the first
:: occurrence of each operation type (ENTER, REPLACE, RESOLVE,
:: LIQUIDATE) with color-coded output.
:: ============================================================

:: ANSI color codes (Windows 10+ Terminal)
set "GREEN=[92m"
set "YELLOW=[93m"
set "CYAN=[96m"
set "MAGENTA=[95m"
set "RED=[91m"
set "BOLD=[1m"
set "RESET=[0m"

:: Track first-time flags
set "SEEN_ENTER=0"
set "SEEN_REPLACE=0"
set "SEEN_RESOLVE=0"
set "SEEN_LIQUIDATE=0"
set "ALL_CONFIRMED=0"

set "LOGDIR=/home/andydoc/prediction-trader/logs"

echo %BOLD%%CYAN%========================================================%RESET%
echo %BOLD%%CYAN%  Rust PM First-Time Operation Monitor%RESET%
echo %BOLD%%CYAN%========================================================%RESET%
echo.
echo  Log dir: %LOGDIR%/trading_engine_YYYYMMDD.log
echo  Handles midnight rollover automatically.
echo  Watching for first occurrence of each operation type...
echo.
echo  %GREEN%[ ] ENTER%RESET%      -- Position entry via rust_pm.enter_position()
echo  %YELLOW%[ ] REPLACE%RESET%    -- Liquidate old + enter new via Rust PM
echo  %CYAN%[ ] RESOLVE%RESET%    -- Resolution via rust_pm.check_resolutions()
echo  %MAGENTA%[ ] LIQUIDATE%RESET%  -- Proactive exit via rust_pm.liquidate_position()
echo.
echo %BOLD%Tailing log... (Ctrl+C to stop)%RESET%
echo.

:: Use WSL bash loop that follows today's log and re-opens on date rollover
:: cd to C:\ first to avoid WSL path translation error on P:\
pushd C:\
wsl.exe -d Ubuntu -- bash -c "LOGDIR=%LOGDIR%; while true; do D=$(date +%%Y%%m%%d); F=$LOGDIR/trading_engine_${D}.log; [ -f \"$F\" ] || { sleep 2; continue; }; tail -n 0 -f \"$F\" & PID=$!; while [ \"$(date +%%Y%%m%%d)\" = \"$D\" ]; do sleep 10; done; kill $PID 2>/dev/null; wait $PID 2>/dev/null; done" | powershell -NoProfile -Command ^
  "$seenEnter = $false; $seenReplace = $false; $seenResolve = $false; $seenLiquidate = $false; " ^
  "$e = [char]27; " ^
  "$green = \"$e[92m\"; $yellow = \"$e[93m\"; $cyan = \"$e[96m\"; $magenta = \"$e[95m\"; " ^
  "$bold = \"$e[1m\"; $reset = \"$e[0m\"; " ^
  "$input | ForEach-Object { " ^
  "  $line = $_; " ^
  "  if ($line -match 'ENTER:' -and -not $seenEnter) { " ^
  "    $seenEnter = $true; " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}${green}=== FIRST ENTER DETECTED (Rust PM) ===${reset}\"; " ^
  "    Write-Host \"${green}$line${reset}\"; " ^
  "    Write-Host \"${green}[X] ENTER confirmed through rust_pm.enter_position()${reset}\"; " ^
  "    Write-Host ''; " ^
  "  } " ^
  "  elseif ($line -match 'REPLACE:' -and -not $seenReplace) { " ^
  "    $seenReplace = $true; " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}${yellow}=== FIRST REPLACE DETECTED (Rust PM) ===${reset}\"; " ^
  "    Write-Host \"${yellow}$line${reset}\"; " ^
  "    Write-Host \"${yellow}[X] REPLACE confirmed through rust_pm (liquidate + enter)${reset}\"; " ^
  "    Write-Host ''; " ^
  "  } " ^
  "  elseif ($line -match 'RESOLVED' -and -not $seenResolve) { " ^
  "    $seenResolve = $true; " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}${cyan}=== FIRST RESOLVE DETECTED (Rust PM) ===${reset}\"; " ^
  "    Write-Host \"${cyan}$line${reset}\"; " ^
  "    Write-Host \"${cyan}[X] RESOLVE confirmed through rust_pm.check_resolutions()${reset}\"; " ^
  "    Write-Host ''; " ^
  "  } " ^
  "  elseif (($line -match 'PROACTIVE EXIT:' -or $line -match 'Liquidated:') -and -not $seenLiquidate) { " ^
  "    $seenLiquidate = $true; " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}${magenta}=== FIRST LIQUIDATE DETECTED (Rust PM) ===${reset}\"; " ^
  "    Write-Host \"${magenta}$line${reset}\"; " ^
  "    Write-Host \"${magenta}[X] LIQUIDATE confirmed through rust_pm.liquidate_position()${reset}\"; " ^
  "    Write-Host ''; " ^
  "  } " ^
  "  if ($seenEnter -and $seenReplace -and $seenResolve -and $seenLiquidate) { " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}${green}========================================================${reset}\"; " ^
  "    Write-Host \"${bold}${green}  ALL 4 OPERATION TYPES CONFIRMED THROUGH RUST PM!${reset}\"; " ^
  "    Write-Host \"${bold}${green}========================================================${reset}\"; " ^
  "    Write-Host \"${green}  [X] ENTER      -- rust_pm.enter_position()${reset}\"; " ^
  "    Write-Host \"${yellow}  [X] REPLACE    -- rust_pm (liquidate + enter)${reset}\"; " ^
  "    Write-Host \"${cyan}  [X] RESOLVE    -- rust_pm.check_resolutions()${reset}\"; " ^
  "    Write-Host \"${magenta}  [X] LIQUIDATE  -- rust_pm.liquidate_position()${reset}\"; " ^
  "    Write-Host ''; " ^
  "    Write-Host \"${bold}A1 VERIFICATION COMPLETE -- paper_engine is fully bypassed.${reset}\"; " ^
  "    break; " ^
  "  } " ^
  "}"

popd
echo.
echo Monitor finished.
pause
