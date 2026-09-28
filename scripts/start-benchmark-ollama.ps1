param(
    [string]$BenchmarkRoot = (Join-Path $PSScriptRoot '../../benchmark')
)

$ErrorActionPreference = 'Stop'
$BenchmarkRoot = [IO.Path]::GetFullPath($BenchmarkRoot)
$executable = Join-Path $BenchmarkRoot 'tools/ollama/ollama.exe'
if (-not (Test-Path -LiteralPath $executable)) {
    throw "Missing official Ollama portable installation: $executable"
}
$env:OLLAMA_MODELS = Join-Path $BenchmarkRoot 'models'
$env:OLLAMA_HOST = '127.0.0.1:11434'
$env:OLLAMA_CONTEXT_LENGTH = '4096'
$env:OLLAMA_KEEP_ALIVE = '30m'
$env:OLLAMA_NO_CLOUD = '1'
& $executable serve
