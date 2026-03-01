# Prediction Trader Setup Script
# Run this with: powershell -ExecutionPolicy Bypass -File setup.ps1

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "Prediction Market Trading System Setup" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

$baseDir = "V:\prediction-trader"

Write-Host "[1/5] Verifying directory structure..." -ForegroundColor Yellow
Set-Location $baseDir

# Check if we have the Python files
$requiredFiles = @("main.py", "config\config.yaml", "requirements.txt")
$missingFiles = @()

foreach ($file in $requiredFiles) {
    if (-not (Test-Path $file)) {
        $missingFiles += $file
    }
}

if ($missingFiles.Count -gt 0) {
    Write-Host "ERROR: Missing files!" -ForegroundColor Red
    Write-Host "Please download these files from Claude and place them in V:\prediction-trader:" -ForegroundColor Red
    foreach ($file in $missingFiles) {
        Write-Host "  - $file" -ForegroundColor Red
    }
    Write-Host ""
    Write-Host "Download all files from the chat above and place them in:" -ForegroundColor Yellow
    Write-Host "  V:\prediction-trader\" -ForegroundColor Yellow
    exit 1
}

Write-Host "OK - All required files present" -ForegroundColor Green

Write-Host ""
Write-Host "[2/5] Setting up Python virtual environment..." -ForegroundColor Yellow

# Create venv
if (Test-Path "venv") {
    Write-Host "Virtual environment already exists, using existing..." -ForegroundColor Cyan
} else {
    Write-Host "Creating virtual environment..." -ForegroundColor Cyan
    wsl python3 -m venv venv
    if ($LASTEXITCODE -ne 0) {
        Write-Host "ERROR: Failed to create virtual environment" -ForegroundColor Red
        Write-Host "Make sure Python 3.9+ is installed in WSL" -ForegroundColor Red
        exit 1
    }
    Write-Host "OK - Virtual environment created" -ForegroundColor Green
}

Write-Host ""
Write-Host "[3/5] Installing Python dependencies..." -ForegroundColor Yellow
Write-Host "This may take a few minutes..." -ForegroundColor Cyan

wsl bash -c "cd /mnt/v/prediction-trader && source venv/bin/activate && pip install --upgrade pip && pip install -r requirements.txt"

if ($LASTEXITCODE -ne 0) {
    Write-Host "WARNING: Some packages may have failed to install" -ForegroundColor Yellow
    Write-Host "You can manually install them later" -ForegroundColor Yellow
} else {
    Write-Host "OK - Dependencies installed" -ForegroundColor Green
}

Write-Host ""
Write-Host "[4/5] Verifying Ollama..." -ForegroundColor Yellow

$ollamaModels = ollama list 2>$null
if ($LASTEXITCODE -eq 0) {
    if ($ollamaModels -match "qwen2.5-coder:32b") {
        Write-Host "OK - Ollama and qwen2.5-coder:32b found" -ForegroundColor Green
    } else {
        Write-Host "WARNING: qwen2.5-coder:32b not found" -ForegroundColor Yellow
        Write-Host "Run: ollama pull qwen2.5-coder:32b" -ForegroundColor Cyan
    }
} else {
    Write-Host "WARNING: Ollama not found or not running" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "[5/5] Creating startup scripts..." -ForegroundColor Yellow

# Create Windows startup script
$startScript = @'
# Start Prediction Trader (Windows)
Write-Host "Starting Prediction Market Trading System..." -ForegroundColor Cyan

Set-Location V:\prediction-trader
wsl bash -c "cd /mnt/v/prediction-trader && source venv/bin/activate && python main.py"
'@

$startScript | Out-File -FilePath "start.ps1" -Encoding UTF8
Write-Host "OK - Created start.ps1" -ForegroundColor Green

# Create WSL startup script  
$bashScript = @'
#!/bin/bash
cd /mnt/v/prediction-trader || exit 1
echo "Starting Prediction Market Trading System..."
source venv/bin/activate
python main.py
'@

$bashScript | Out-File -FilePath "start.sh" -Encoding UTF8
wsl chmod +x /mnt/v/prediction-trader/start.sh
Write-Host "OK - Created start.sh" -ForegroundColor Green

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "SETUP COMPLETE!" -ForegroundColor Green  
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "To start the trading system:" -ForegroundColor Cyan
Write-Host "  powershell -File start.ps1" -ForegroundColor White
Write-Host ""
Write-Host "Monitor logs in: V:\prediction-trader\logs\" -ForegroundColor Yellow
Write-Host ""
