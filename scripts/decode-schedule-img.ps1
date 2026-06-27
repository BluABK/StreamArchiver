<#
.SYNOPSIS
    Batch-decodes streamer schedule banner images into structured JSON using Claude Code.

.DESCRIPTION
    Loops over every image in a folder, sends each to the Claude Code CLI with a strict
    OCR-to-JSON prompt, strips any stray markdown fences, validates the result, and merges
    everything into one combined JSON file. Falls back to a stronger model if the cheap
    model returns invalid JSON for a given image.

.PARAMETER Folder
    Folder containing the banner images. Defaults to the current directory.

.PARAMETER Output
    Path for the combined JSON file. Defaults to .\schedule.json

.PARAMETER Model
    Primary (cheap) model to try first. Defaults to "haiku".

.PARAMETER FallbackModel
    Model to retry with if the primary returns invalid JSON. Defaults to "sonnet".

.PARAMETER Year
    The year to assume for dates (banners rarely show it). Defaults to current year.

.PARAMETER Timezone
    IANA timezone the banner times are in. Defaults to America/Los_Angeles (PDT/PST).

.PARAMETER Offset
    UTC offset string matching the timezone/season. Defaults to -07:00 (PDT).

.PARAMETER SavePerFile
    If set, also writes a <imagename>.json next to the combined output for each banner.

.EXAMPLE
    .\Decode-Schedules.ps1 -Folder .\banners -Output .\week.json

.EXAMPLE
    .\Decode-Schedules.ps1 -Folder .\banners -Model sonnet -Timezone "Europe/Oslo" -Offset "+02:00"
#>

[CmdletBinding()]
param(
    [string]$Folder        = ".",
    [string]$Output        = ".\schedule.json",
    [string]$Model         = "haiku",
    [string]$FallbackModel = "sonnet",
    [int]$Year             = (Get-Date).Year,
    [string]$Timezone      = "America/Los_Angeles",
    [string]$Offset        = "-07:00",
    [switch]$SavePerFile
)

# Image extensions Claude Code accepts (JPEG/PNG/GIF/WebP).
$extensions = @("*.jpg", "*.jpeg", "*.png", "*.gif", "*.webp")

# --- Sanity checks -----------------------------------------------------------

if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Write-Error "The 'claude' CLI was not found on PATH. Install Claude Code first."
    exit 1
}

if (-not (Test-Path -LiteralPath $Folder)) {
    Write-Error "Folder not found: $Folder"
    exit 1
}

# --- Prompt template ---------------------------------------------------------
# {IMAGE_PATH}, {YEAR}, {TZ}, {OFFSET} are substituted per image below.
# Single-quoted here-string => fully literal, so inner quotes need no escaping.

$promptTemplate = @'
You are an OCR-to-JSON extractor. Read the streamer schedule in {IMAGE_PATH} and output an array of event objects.

RULES:
- Output ONLY raw JSON. No markdown, no code fences, no backticks, no commentary, no leading or trailing text. The first character of your reply must be '[' and the last must be ']'.
- Timezone: use exactly what the banner shows. The labels indicate {TZ}, so timezone = '{TZ}' and the UTC offset = {OFFSET}. Do NOT convert to any other timezone. Do NOT use my local timezone.
- The year is {YEAR}.
- Transcribe titles literally. 'w' or 'W' before a name means 'with' (a collaborator), e.g. 'FEARS TO FATHOM w CRELLY' -> title 'Fears to Fathom', collab 'Crelly'. Do not guess or 'correct' names.
- Skip any card marked OFFLINE or with an unknown date ('????').
- If a time is vague (e.g. 'Evening'), set time and datetime to null but keep the raw text in time_label.

Each object has these fields:
- title (string)
- collab (string or null)
- date (YYYY-MM-DD)
- day (weekday name)
- time (HH:MM 24-hour, or null)
- time_label (raw time text from banner, e.g. '12.00 P.M.' or 'Evening')
- timezone (IANA name)
- datetime (ISO 8601 with offset, or null if no exact time)
- source_image (set this to the filename: {IMAGE_PATH})
'@

# --- Helper: run one image through a model, return parsed objects or $null ---

function Invoke-Decode {
    param(
        [string]$ImagePath,
        [string]$ImageDir,
        [string]$UseModel
    )

    # Build the prompt for this specific image.
    $prompt = $promptTemplate.
        Replace('{IMAGE_PATH}', $ImagePath).
        Replace('{YEAR}',       $Year.ToString()).
        Replace('{TZ}',         $Timezone).
        Replace('{OFFSET}',     $Offset)

    # Call the CLI. --add-dir grants Read access to the image's folder (it may be
    # outside the working directory). Join lines so multi-line output is one string.
    $raw = (claude --model $UseModel --add-dir $ImageDir -p $prompt) -join "`n"

    if ([string]::IsNullOrWhiteSpace($raw)) {
        Write-Warning "  [$UseModel] returned empty output."
        return $null
    }

    # Strip any stray markdown fences the model may have added anyway.
    $clean = $raw -replace '```json', '' -replace '```', ''

    # Trim to the outermost JSON array, in case of leading/trailing prose.
    $start = $clean.IndexOf('[')
    $end   = $clean.LastIndexOf(']')
    if ($start -ge 0 -and $end -gt $start) {
        $clean = $clean.Substring($start, $end - $start + 1)
    }

    # Validate by parsing. Throws on bad JSON -> caught by caller.
    try {
        return $clean | ConvertFrom-Json
    }
    catch {
        Write-Warning "  [$UseModel] produced invalid JSON: $($_.Exception.Message)"
        return $null
    }
}

# --- Main loop ---------------------------------------------------------------

$images = Get-ChildItem -LiteralPath $Folder -Include $extensions -File -Recurse |
          Sort-Object FullName

if ($images.Count -eq 0) {
    Write-Error "No images found in '$Folder' (looked for: $($extensions -join ', '))."
    exit 1
}

Write-Host "Found $($images.Count) image(s) in '$Folder'." -ForegroundColor Cyan

$allEvents = [System.Collections.Generic.List[object]]::new()
$failed    = [System.Collections.Generic.List[string]]::new()

foreach ($img in $images) {
    # Forward slashes are safest inside the prompt on Windows.
    $imgPath = $img.FullName.Replace('\', '/')
    $imgDir  = $img.DirectoryName.Replace('\', '/')
    Write-Host "Decoding: $($img.Name)" -ForegroundColor Yellow

    # Try cheap model first, then fall back to the stronger one.
    $events = Invoke-Decode -ImagePath $imgPath -ImageDir $imgDir -UseModel $Model
    if ($null -eq $events) {
        Write-Host "  Retrying with $FallbackModel..." -ForegroundColor DarkYellow
        $events = Invoke-Decode -ImagePath $imgPath -ImageDir $imgDir -UseModel $FallbackModel
    }

    if ($null -eq $events) {
        Write-Warning "  Giving up on $($img.Name)."
        $failed.Add($img.Name)
        continue
    }

    # ConvertFrom-Json returns a single object (not an array) if there's one event.
    if ($events -isnot [System.Array]) { $events = @($events) }

    foreach ($e in $events) { $allEvents.Add($e) }
    Write-Host "  OK - $($events.Count) event(s)." -ForegroundColor Green

    if ($SavePerFile) {
        $perFile = Join-Path (Split-Path $Output -Parent) "$($img.BaseName).json"
        $events | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $perFile -Encoding utf8
    }
}

# --- Write combined output ---------------------------------------------------

if ($allEvents.Count -eq 0) {
    Write-Error "No events were successfully decoded."
    exit 1
}

# Depth 6 so nested fields aren't silently truncated; utf8 (no BOM) for portability.
$allEvents | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $Output -Encoding utf8

Write-Host ""
Write-Host "Done. $($allEvents.Count) event(s) written to '$Output'." -ForegroundColor Cyan
if ($failed.Count -gt 0) {
    Write-Warning "Failed to decode $($failed.Count) image(s): $($failed -join ', ')"
}