# Instaluje (albo aktualizuje) odtwarzacz mpv w "Program Files\mpv" - najnowsza kompilacja
# shinchiro/mpv-winbuild-cmake z GitHuba, w wariancie x86-64-v3 (AVX2), gdy procesor go obsluguje,
# inaczej w wariancie ogolnym x86-64. VideoPipeline uzywa go pod F3.
#
# Rejestruje tez mpv w systemie: "Otworz za pomoca", Programy domyslne, skrot w menu Start, PATH.
# Windows 10/11 nie pozwala skryptom po cichu zmienic domyslnego programu - rozszerzenia bez zadnego
# skojarzenia dostaja mpv od razu, dla pozostalych skrypt otwiera Ustawienia -> Aplikacje domyslne.
#
#   zainstaluj-mpv.cmd              instalacja / aktualizacja + skojarzenia
#   zainstaluj-mpv.cmd -BezSkojarzen  sama instalacja / aktualizacja
#   zainstaluj-mpv.cmd -Odinstaluj  usuniecie mpv i skojarzen
param(
    [switch]$Odinstaluj,
    [switch]$BezSkojarzen,
    [switch]$Wymus,
    [switch]$Podniesiony
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest bez paska jest wielokrotnie szybszy

function Zakoncz([int]$kod) {
    if ($Podniesiony) { Read-Host 'Nacisnij Enter, aby zamknac okno' | Out-Null }
    exit $kod
}

# ------------------------------------------------------------------ uprawnienia administratora
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Write-Host 'Instalacja w Program Files wymaga uprawnien administratora - potwierdz w oknie UAC.'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Podniesiony')
    if ($Odinstaluj) { $argList += '-Odinstaluj' }
    if ($BezSkojarzen) { $argList += '-BezSkojarzen' }
    if ($Wymus) { $argList += '-Wymus' }
    try {
        $p = Start-Process -FilePath 'powershell.exe' -ArgumentList $argList -Verb RunAs -Wait -PassThru
        exit $p.ExitCode
    } catch {
        Write-Host 'Anulowano - bez uprawnien administratora nie mozna instalowac w Program Files.'
        exit 1
    }
}

$dest = Join-Path $env:ProgramFiles 'mpv'
$mpv = Join-Path $dest 'mpv.exe'
$progId = 'io.mpv.video'
$classes = 'HKLM:\SOFTWARE\Classes'
$capPath = 'SOFTWARE\Clients\Media\mpv\Capabilities'
$exts = @('.mkv', '.mp4', '.m4v', '.mov', '.avi', '.webm', '.ts', '.m2ts', '.mts', '.wmv', '.flv',
          '.mpg', '.mpeg', '.vob', '.ogv', '.3gp', '.hevc', '.h264', '.264', '.265', '.y4m', '.divx')
$startLnk = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'mpv.lnk'

function Ustaw-Reg([string]$path, [string]$name, [string]$value) {
    if (-not (Test-Path -LiteralPath $path)) { New-Item -Path $path -Force | Out-Null }
    if ($name -eq '') {
        Set-Item -LiteralPath $path -Value $value
    } else {
        New-ItemProperty -LiteralPath $path -Name $name -Value $value -PropertyType String -Force | Out-Null
    }
}

function Usun-Klucz([string]$path) {
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
}

function Odswiez-Powloke {
    try {
        Add-Type -Namespace VP -Name Shell -MemberDefinition @'
[DllImport("shell32.dll")] public static extern void SHChangeNotify(int e, uint f, System.IntPtr a, System.IntPtr b);
'@ -ErrorAction SilentlyContinue
        [VP.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)   # SHCNE_ASSOCCHANGED
    } catch { }
}

function Sciezka-PATH([bool]$dodaj) {
    $cur = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $parts = @($cur -split ';' | Where-Object { $_ -ne '' -and $_.TrimEnd('\') -ne $dest.TrimEnd('\') })
    if ($dodaj) { $parts += $dest }
    $new = $parts -join ';'
    if ($new -ne $cur) { [Environment]::SetEnvironmentVariable('Path', $new, 'Machine') }
}

# ------------------------------------------------------------------ odinstalowanie
if ($Odinstaluj) {
    Get-Process mpv -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $mpv } | Stop-Process -Force
    foreach ($e in $exts) {
        $k = "$classes\$e"
        if (Test-Path -LiteralPath "$k\OpenWithProgids") {
            Remove-ItemProperty -LiteralPath "$k\OpenWithProgids" -Name $progId -ErrorAction SilentlyContinue
        }
        if ((Test-Path -LiteralPath $k) -and ((Get-Item -LiteralPath $k).GetValue('') -eq $progId)) {
            Set-Item -LiteralPath $k -Value ''
        }
    }
    Usun-Klucz "$classes\$progId"
    Usun-Klucz "$classes\Applications\mpv.exe"
    Usun-Klucz 'HKLM:\SOFTWARE\Clients\Media\mpv'
    Usun-Klucz 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\mpv.exe'
    Remove-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\RegisteredApplications' -Name 'mpv' -ErrorAction SilentlyContinue
    Sciezka-PATH $false
    if (Test-Path -LiteralPath $startLnk) { Remove-Item -LiteralPath $startLnk -Force }
    if (Test-Path -LiteralPath $dest) { Remove-Item -LiteralPath $dest -Recurse -Force }
    Odswiez-Powloke
    Write-Host "Usunieto mpv z $dest wraz ze skojarzeniami."
    Zakoncz 0
}

# ------------------------------------------------------------------ wybor kompilacji
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# x86-64-v3 wymaga m.in. AVX2 (PF_AVX2_INSTRUCTIONS_AVAILABLE = 40; starsze Windows zwracaja false -
# wtedy bezpieczny wariant ogolny).
$v3 = $false
try {
    Add-Type -Namespace VP -Name Cpu -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern bool IsProcessorFeaturePresent(uint feature);
'@ -ErrorAction SilentlyContinue
    $v3 = [VP.Cpu]::IsProcessorFeaturePresent(40)
} catch { $v3 = $false }

Write-Host 'Sprawdzam najnowsza kompilacje mpv (github.com/shinchiro/mpv-winbuild-cmake)...'
$ua = 'VideoPipeline-mpv-installer'
try {
    $rel = Invoke-RestMethod -Uri 'https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest' -UserAgent $ua
} catch {
    Write-Host "Nie udalo sie pobrac listy wydan mpv z GitHuba: $($_.Exception.Message)"
    Zakoncz 1
}
$pattern = if ($v3) { '^mpv-x86_64-v3-.*\.7z$' } else { '^mpv-x86_64-\d.*\.7z$' }
$asset = $rel.assets | Where-Object { $_.name -match $pattern -and $_.name -notmatch '-dev-' } | Select-Object -First 1
if (-not $asset -and $v3) {
    $asset = $rel.assets | Where-Object { $_.name -match '^mpv-x86_64-\d.*\.7z$' -and $_.name -notmatch '-dev-' } | Select-Object -First 1
}
if (-not $asset) {
    Write-Host "W wydaniu $($rel.tag_name) nie ma archiwum mpv dla x86-64."
    Zakoncz 1
}
$wariant = if ($asset.name -match '-v3-') { 'x86-64-v3 (AVX2)' } else { 'x86-64' }
Write-Host "Kompilacja: $($asset.name) [$wariant]"

$stamp = Join-Path $dest 'videopipeline-mpv.txt'
$installed = if (Test-Path -LiteralPath $stamp) { (Get-Content -LiteralPath $stamp -Raw).Trim() } else { '' }
if ((Test-Path -LiteralPath $mpv) -and $installed -eq $asset.name -and -not $Wymus) {
    Write-Host "mpv jest aktualny ($dest)."
} else {
    # -------------------------------------------------------------- pobranie i rozpakowanie
    $tmp = Join-Path $env:TEMP ('vp-mpv-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $archive = Join-Path $tmp $asset.name
        Write-Host ("Pobieram {0:N1} MB..." -f ($asset.size / 1MB))
        Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -UseBasicParsing -UserAgent $ua
        $out = Join-Path $tmp 'x'
        New-Item -ItemType Directory -Path $out | Out-Null
        # tar.exe z Windows 11 rozpakowuje .7z; na Windows 10 - samodzielny 7zr.exe z 7-zip.org.
        $ok = $false
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        if (Test-Path -LiteralPath $tar) {
            & $tar -xf $archive -C $out 2>$null
            $ok = ($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath (Join-Path $out 'mpv.exe'))
        }
        if (-not $ok) {
            Write-Host 'Pobieram 7zr.exe (7-zip.org) do rozpakowania archiwum...'
            $sevenZip = Join-Path $tmp '7zr.exe'
            Invoke-WebRequest -Uri 'https://www.7-zip.org/a/7zr.exe' -OutFile $sevenZip -UseBasicParsing -UserAgent $ua
            & $sevenZip x -y "-o$out" $archive | Out-Null
            $ok = ($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath (Join-Path $out 'mpv.exe'))
        }
        if (-not $ok) { throw 'nie udalo sie rozpakowac archiwum mpv' }

        $running = Get-Process mpv -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $mpv }
        if ($running) {
            Write-Host 'Zamknij mpv (uruchomiony z Program Files) i uruchom skrypt ponownie.'
            Zakoncz 1
        }
        New-Item -ItemType Directory -Path $dest -Force | Out-Null
        # Pliki z archiwum nadpisuja stare; wlasne ustawienia (portable_config, mpv.conf) zostaja.
        & robocopy.exe $out $dest /E /NFL /NDL /NJH /NJS /NP | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "kopiowanie do $dest nie powiodlo sie (robocopy $LASTEXITCODE)" }
        Set-Content -LiteralPath $stamp -Value $asset.name -Encoding ASCII
        Write-Host "Zainstalowano mpv w $dest"
    } catch {
        Write-Host "BLAD: $($_.Exception.Message)"
        Zakoncz 1
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ------------------------------------------------------------------ rejestracja w systemie
Ustaw-Reg 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\mpv.exe' '' $mpv
Ustaw-Reg 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\mpv.exe' 'Path' $dest
Sciezka-PATH $true
$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut($startLnk)
$lnk.TargetPath = $mpv
$lnk.Arguments = '--player-operation-mode=pseudo-gui'
$lnk.WorkingDirectory = $dest
$lnk.IconLocation = "$mpv,0"
$lnk.Description = 'mpv - odtwarzacz wideo'
$lnk.Save()

if ($BezSkojarzen) {
    Write-Host 'Pominieto skojarzenia z plikami wideo (-BezSkojarzen).'
    Zakoncz 0
}

$open = "`"$mpv`" -- `"%1`""
Ustaw-Reg "$classes\$progId" '' 'Plik wideo (mpv)'
Ustaw-Reg "$classes\$progId" 'FriendlyTypeName' 'Plik wideo'
Ustaw-Reg "$classes\$progId\DefaultIcon" '' "`"$mpv`",0"
Ustaw-Reg "$classes\$progId\shell" '' 'open'
Ustaw-Reg "$classes\$progId\shell\open" '' 'Odtworz w mpv'
Ustaw-Reg "$classes\$progId\shell\open\command" '' $open

Ustaw-Reg "$classes\Applications\mpv.exe" 'FriendlyAppName' 'mpv'
Ustaw-Reg "$classes\Applications\mpv.exe\shell\open\command" '' $open

Ustaw-Reg "HKLM:\$capPath" 'ApplicationName' 'mpv'
Ustaw-Reg "HKLM:\$capPath" 'ApplicationDescription' 'mpv - odtwarzacz wideo'
Ustaw-Reg "HKLM:\$capPath" 'ApplicationIcon' "`"$mpv`",0"
Ustaw-Reg 'HKLM:\SOFTWARE\RegisteredApplications' 'mpv' $capPath

$nowe = @()
foreach ($e in $exts) {
    Ustaw-Reg "$classes\Applications\mpv.exe\SupportedTypes" $e ''
    Ustaw-Reg "HKLM:\$capPath\FileAssociations" $e $progId
    Ustaw-Reg "$classes\$e\OpenWithProgids" $progId ''
    # Rozszerzenie bez zadnego programu w systemie i bez wyboru uzytkownika - od razu mpv.
    $cur = (Get-Item -LiteralPath "$classes\$e").GetValue('')
    $choice = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\$e\UserChoice"
    if ([string]::IsNullOrEmpty($cur) -and -not (Test-Path -LiteralPath $choice)) {
        Set-Item -LiteralPath "$classes\$e" -Value $progId
        $nowe += $e
    }
}
Odswiez-Powloke
Write-Host ''
Write-Host "mpv jest w 'Otworz za pomoca' i w Programach domyslnych dla: $($exts -join ' ')"
if ($nowe.Count -gt 0) { Write-Host "Od razu otwierane w mpv (nie mialy programu): $($nowe -join ' ')" }
Write-Host 'Windows nie pozwala skryptom zmienic domyslnego programu - otwieram Ustawienia:'
Write-Host '  wybierz mpv i ustaw go dla potrzebnych typow plikow (np. .mkv, .mp4).'
try { Start-Process 'ms-settings:defaultapps?registeredAppMachine=mpv' } catch { }
Zakoncz 0
