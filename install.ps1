<#
  ultimate-ai-local-search — Windows installer (PowerShell).
  Requires: Docker Desktop (with WSL2) running. Mirrors install.sh.
  Run:  powershell -ExecutionPolicy Bypass -File .\install.ps1
        .\install.ps1 -Provider ollama   (non-interactive)
#>
param(
  [string]$Provider = "",
  [string]$Model = "",
  [string]$Dimension = "",
  [switch]$NoSmoke
)
$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $Repo

function Say($m){ Write-Host "▸ $m" -ForegroundColor Cyan }
function Ok ($m){ Write-Host "✓ $m" -ForegroundColor Green }
function Die($m){ Write-Host "✗ $m" -ForegroundColor Red; exit 1 }
function Have($c){ $null -ne (Get-Command $c -ErrorAction SilentlyContinue) }

Say "ultimate-ai-local-search — Windows"
if(-not (Have docker)){ Die "Docker not found. Install Docker Desktop (WSL2 backend) and re-run." }
try { docker info *> $null } catch { Die "Docker is not running. Start Docker Desktop and re-run." }
if(-not (Have jq)){ Say "tip: jq not required on Windows (using native JSON)" }
# Node not used by installer, but claude-context runs via npx afterward and needs Node 22+
# (a dep uses require(ESM), unsupported before 22.12). Warn if missing/old.
$nodeMajor = if(Have node){ try { [int]((node --version) -replace '^v','' -replace '\..*','') } catch { 0 } } else { 0 }
if((Have node) -and (Have npx) -and ($nodeMajor -ge 22)){ Ok "node present ($(node --version))" }
else { Write-Host "! Node.js 22+ REQUIRED afterward (claude-context runs via npx, needs require(ESM)). Current: $(if(Have node){node --version}else{'none'}). Get it from https://nodejs.org." -ForegroundColor Yellow }

# ── provider selection ──
if(-not $Provider){
  Write-Host "`nWhere should embeddings be computed?"
  Write-Host "  Local:  1) Ollama   2) LM Studio"
  Write-Host "  Cloud:  3) OpenAI   4) VoyageAI   5) Gemini   6) OpenRouter"
  switch(Read-Host "Choose [1-6] (default 1)"){
    "2"{$Provider="lmstudio"} "3"{$Provider="openai"} "4"{$Provider="voyageai"}
    "5"{$Provider="gemini"} "6"{$Provider="openrouter"} default {$Provider="ollama"}
  }
}
$EmbProvider=""; $BaseUrl=""; $OllamaHost="http://127.0.0.1:11434"
switch($Provider){
  "ollama"    { $EmbProvider="Ollama";   if(-not $Model){$Model="nomic-embed-text"};                     if(-not $Dimension){$Dimension="768"} }
  "lmstudio"  { $EmbProvider="OpenAI";   if(-not $Model){$Model="text-embedding-nomic-embed-text-v1.5"}; if(-not $Dimension){$Dimension="768"}; $BaseUrl="http://127.0.0.1:1234/v1" }
  "openai"    { $EmbProvider="OpenAI";   if(-not $Model){$Model="text-embedding-3-small"};               if(-not $Dimension){$Dimension="1536"} }
  "voyageai"  { $EmbProvider="VoyageAI"; if(-not $Model){$Model="voyage-code-3"};                        if(-not $Dimension){$Dimension="1024"} }
  "gemini"    { $EmbProvider="Gemini";   if(-not $Model){$Model="text-embedding-004"};                   if(-not $Dimension){$Dimension="768"} }
  "openrouter"{ $EmbProvider="OpenRouter";if(-not $Model){$Model="openai/text-embedding-3-small"};       if(-not $Dimension){$Dimension="1536"} }
  default { Die "unknown provider: $Provider" }
}
Say "provider=$Provider model=$Model dim=$Dimension"

# ── provider setup ──
$ApiKeyVar=""; $ApiKeyVal=""
switch($Provider){
  "ollama" {
    if(-not (Have ollama)){
      if(Have winget){ winget install --id Ollama.Ollama -e --accept-source-agreements --accept-package-agreements }
      else { Die "Install Ollama from https://ollama.com/download then re-run." }
    }
    Say "pulling model $Model…"; ollama pull $Model
  }
  "lmstudio" {
    if(-not (Have lms)){ Die "Install LM Studio from https://lmstudio.ai (and its 'lms' CLI), then re-run." }
    lms get $Model 2>$null; lms server start 2>$null
  }
  "openai"     { $ApiKeyVar="OPENAI_API_KEY";     $ApiKeyVal=$env:OPENAI_API_KEY }
  "voyageai"   { $ApiKeyVar="VOYAGEAI_API_KEY";   $ApiKeyVal=$env:VOYAGEAI_API_KEY }
  "gemini"     { $ApiKeyVar="GEMINI_API_KEY";     $ApiKeyVal=$env:GEMINI_API_KEY }
  "openrouter" { $ApiKeyVar="OPENROUTER_API_KEY"; $ApiKeyVal=$env:OPENROUTER_API_KEY }
}
if($ApiKeyVar -and -not $ApiKeyVal){
  $sec = Read-Host "Enter $ApiKeyVar" -AsSecureString
  $ApiKeyVal = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
  if(-not $ApiKeyVal){ Die "$ApiKeyVar required" }
}

# ── write .env ──
Say "writing .env"
$lines = @(
  "MILVUS_PORT=19530","MILVUS_HEALTH_PORT=9091","MILVUS_VERSION=v2.5.6",
  "ETCD_VERSION=v3.5.18","MINIO_VERSION=RELEASE.2023-03-20T20-16-18Z",
  "MILVUS_ADDRESS=127.0.0.1:19530",
  "EMBEDDING_PROVIDER=$EmbProvider","EMBEDDING_MODEL=$Model","EMBEDDING_DIMENSION=$Dimension"
)
if($BaseUrl){ $lines += "OPENAI_BASE_URL=$BaseUrl" }
if($Provider -eq "lmstudio"){ $lines += "OPENAI_API_KEY=lm-studio" }
if($Provider -eq "ollama"){ $lines += "OLLAMA_HOST=$OllamaHost" }
if($ApiKeyVar){ $lines += "$ApiKeyVar=$ApiKeyVal" }
$lines -join "`n" | Set-Content -NoNewline -Path "$Repo\.env"
Ok ".env written"

# ── start Milvus ──
Say "starting Milvus stack…"
docker compose --env-file "$Repo\.env" up -d
Say "waiting for Milvus health…"
$healthy=$false
for($i=0;$i -lt 60;$i++){
  try { Invoke-RestMethod "http://127.0.0.1:9091/healthz" -TimeoutSec 5 *> $null; $healthy=$true; break } catch { Start-Sleep 3 }
}
if(-not $healthy){ Die "Milvus did not become healthy. Check: docker compose logs milvus" }
Ok "Milvus healthy"

# ── merge claude-context MCP into ~/.claude.json (native JSON) ──
$cfg = Join-Path $HOME ".claude.json"
if(-not (Test-Path $cfg)){ "{}" | Set-Content $cfg }
Copy-Item $cfg "$cfg.bak.$(Get-Date -Format yyyyMMdd-HHmmss)"
$j = Get-Content $cfg -Raw | ConvertFrom-Json
$envObj = [ordered]@{ MILVUS_ADDRESS="127.0.0.1:19530"; EMBEDDING_PROVIDER=$EmbProvider; EMBEDDING_MODEL=$Model; EMBEDDING_DIMENSION=$Dimension }
if($BaseUrl){ $envObj.OPENAI_BASE_URL=$BaseUrl }
if($Provider -eq "lmstudio"){ $envObj.OPENAI_API_KEY="lm-studio" }
if($Provider -eq "ollama"){ $envObj.OLLAMA_HOST=$OllamaHost }
if($ApiKeyVar){ $envObj[$ApiKeyVar]=$ApiKeyVal }
$server = [ordered]@{ type="stdio"; command="npx"; args=@("-y","@zilliz/claude-context-mcp@latest"); env=$envObj }
if(-not $j.mcpServers){ $j | Add-Member -NotePropertyName mcpServers -NotePropertyValue (@{}) -Force }
$j.mcpServers | Add-Member -NotePropertyName "claude-context" -NotePropertyValue $server -Force
$j | ConvertTo-Json -Depth 30 | Set-Content $cfg
Ok "claude-context MCP written to $cfg (restart your agent)"

Write-Host "`nDone. Restart your AI agent, then index a project via claude-context (index_codebase)." -ForegroundColor Green
Write-Host "Note: the Windows path is less battle-tested than install.sh — open an issue if anything trips." -ForegroundColor Yellow
