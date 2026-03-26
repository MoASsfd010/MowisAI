param(
  [string]$InputMarkdown = "MowisAI.md",
  [string]$OutputPdf = "MowisAI.pdf"
)

# Converts MowisAI.md -> MowisAI.pdf using pandoc if available.
# Run from the project directory:
#   powershell -ExecutionPolicy Bypass -File .\generate_MowisAI_pdf.ps1

$ErrorActionPreference = "Stop"

if (-not (Test-Path $InputMarkdown)) {
  throw "Missing input markdown: $InputMarkdown"
}

$pandoc = Get-Command pandoc -ErrorAction SilentlyContinue
if ($pandoc) {
  & pandoc $InputMarkdown -o $OutputPdf
  Write-Host "Wrote: $OutputPdf"
  exit 0
}

throw "pandoc not found on PATH. Install pandoc or convert $InputMarkdown -> PDF manually."

