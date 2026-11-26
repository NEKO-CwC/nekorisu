# Installs nekorisu on Windows. No administrator rights needed. Works in Windows PowerShell 5.1
# and PowerShell 7.
#
#   powershell -ExecutionPolicy Bypass -NoProfile -Command "irm https://github.com/NEKO-CwC/nekorisu/releases/latest/download/install.ps1 | iex"
#
# What it does (the Windows counterpart of install.sh):
#   1. downloads nekorisu-x86_64-pc-windows-msvc.tar.gz and SHA256SUMS, checks the SHA-256,
#      unpacks it with Windows' own tar.exe and runs `nekorisu.exe --version`
#   2. puts nekorisu.exe and nekorisu-start.exe in ~\.nekorisu\versions\<version>\
#   3. makes ~\.local\bin\nekorisu.exe a hard link (or a copy) of that nekorisu.exe and writes its
#      path to ~\.nekorisu\current; an exe that is running is renamed aside, not overwritten.
#      Copies nekorisu-start.exe (the launcher that runs core) next to it
#   4. stops a running core, then registers a scheduled task that runs the launcher at login
#      and starts it now (`nekorisu core enable`)
#   5. adds ~\.local\bin to the user's PATH if it isn't there
#
# The first install trusts HTTPS to GitHub, as most installers do; every later
# update is checked against the signing keys built into the binary.
#
# Environment:
#   NEKORISU_DOWNLOAD_BASE  where to download from (default: GitHub's latest release);
#                           a local folder or a file: URL works too
#   NEKORISU_HOME           default ~\.nekorisu
#   NEKORISU_BIN_DIR        default ~\.local\bin
#   NEKORISU_NO_AUTOSTART=1 skip registering the login start in step 4

& {
  Set-StrictMode -Version 2
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue' # the progress bar makes Windows PowerShell 5.1 downloads crawl

  # What the user reads, in Chinese, written as \u escapes so that this file is plain ASCII:
  # Windows PowerShell 5.1 reads a script without a BOM in the local code page (which mangles
  # UTF-8 Chinese, comments included) and its `irm` decodes one as Latin-1, while a BOM breaks
  # `irm | iex`. The comments say what each one means.
  $messages = @{
    Error        = '\u9519\u8bef\uff1a{0}' # Error: {0}
    Arm64        = '\u8fd9\u53f0\u7535\u8111\u662f ARM64\uff1a\u88c5\u7684\u662f x64 \u7248\uff0c\u7531 Windows \u81ea\u5e26\u7684 x64 \u4eff\u771f\u8fd0\u884c' # ARM64: installing the x64 build, which runs under Windows x64 emulation
    BadArch      = '\u4e0d\u652f\u6301\u8fd9\u4e2a\u67b6\u6784\uff1a{0}\uff08\u53ea\u652f\u6301 64 \u4f4d Windows\uff09' # unsupported architecture {0} (64-bit Windows only)
    NoTar        = '\u627e\u4e0d\u5230 Windows \u81ea\u5e26\u7684 tar.exe\uff08\u9700\u8981 Windows 10 1803 \u6216\u66f4\u65b0\u7684\u7248\u672c\uff09' # Windows' own tar.exe is missing (needs Windows 10 1803 or later)
    NoDownload   = '\u4e0b\u8f7d\u4e0d\u4e86 {0}\uff1a{1}' # can't download {0}: {1}
    NotFound     = '\u627e\u4e0d\u5230 {0}' # {0} not found
    Downloading  = '\u4e0b\u8f7d {0} \u2026\u2026' # downloading {0} ...
    NoSum        = 'SHA256SUMS \u91cc\u6ca1\u6709 {0}' # {0} is not in SHA256SUMS
    BadSum       = 'SHA-256 \u5bf9\u4e0d\u4e0a\uff0c\u4e0b\u8f7d\u7684\u6587\u4ef6\u53ef\u80fd\u635f\u574f\u6216\u88ab\u7be1\u6539' # SHA-256 mismatch: the download may be damaged or tampered with
    NoUnpack     = '\u89e3\u4e0d\u5f00 {0}' # can't unpack {0}
    NotInArchive = '\u538b\u7f29\u5305\u91cc\u6ca1\u6709 {0}' # {0} is not in the archive
    NoRun        = '\u8fd0\u884c\u4e0d\u4e86\u4e0b\u8f7d\u7684 nekorisu.exe\uff08\u5e73\u53f0\u4e0d\u5bf9\uff1f\uff09' # the downloaded nekorisu.exe doesn't run (wrong platform?)
    Foreign      = '{0} \u5df2\u7ecf\u5b58\u5728\uff0c\u800c\u4e14\u4e0d\u662f\u5b89\u88c5\u811a\u672c\u88c5\u7684\uff1b\u5148\u79fb\u8d70\u5b83\u518d\u88c5' # {0} exists and was not installed by this script; move it away first
    NoSwitch     = '\u6362\u4e0d\u4e86 {0}\uff1a{1}' # can't switch {0}: {1}
    NoLauncher   = '\u6ce8\u610f\uff1a\u6ca1\u80fd\u66f4\u65b0 {0}\uff08{1}\uff09\uff1bcore \u4e0b\u6b21\u542f\u52a8\u65f6\u4f1a\u518d\u8bd5' # note: couldn't update {0} ({1}); core tries again at its next start
    Installed    = '\u5df2\u5b89\u88c5 nekorisu {0}\uff1a{1} \u2192 {2}' # installed nekorisu {0}: {1} -> {2}
    NoAutostart  = '\u5f00\u673a\u81ea\u542f\u6ca1\u6ce8\u518c\u6210\u529f\uff0c\u53ef\u4ee5\u7a0d\u540e\u8fd0\u884c\uff1anekorisu core enable' # login start not registered; run later: nekorisu core enable
    PathAdded    = '\u5df2\u628a {0} \u52a0\u8fdb\u4f60\u7684 PATH\uff08\u7528\u6237\u73af\u5883\u53d8\u91cf\uff09\uff1b\u65b0\u6253\u5f00\u7684\u7ec8\u7aef\u91cc\u751f\u6548' # added {0} to your PATH (user variable); new terminals see it
    Next         = '\u4e0b\u4e00\u6b65\uff1anekorisu scan \u770b\u770b\u672c\u673a\u6709\u54ea\u4e9b\u4f1a\u8bdd' # next: nekorisu scan to see the sessions on this machine
  }
  # The message `key`, with {0}, {1}, ... filled in from the rest of the arguments.
  function T([string]$key) { [regex]::Unescape($messages[$key]) -f $args }
  function Say([string]$text) { Write-Host $text }
  function Die([string]$text) { throw (T Error $text) }
  # Runs a program with its output thrown away. Windows PowerShell 5.1 would turn redirected
  # stderr lines into errors, which 'Stop' makes fatal, so not here.
  function Quiet([string]$program) {
    $ErrorActionPreference = 'Continue'
    & $program @args *> $null
  }

  $base = if ($env:NEKORISU_DOWNLOAD_BASE) { $env:NEKORISU_DOWNLOAD_BASE } else { 'https://github.com/NEKO-CwC/nekorisu/releases/latest/download' }
  $profileDir = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
  $homeDir = if ($env:NEKORISU_HOME) { $env:NEKORISU_HOME } else { Join-Path $profileDir '.nekorisu' }
  $binDir = if ($env:NEKORISU_BIN_DIR) { $env:NEKORISU_BIN_DIR } else { Join-Path $profileDir '.local\bin' }

  # ---- platform ----
  $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  switch ($arch) {
    'AMD64' { }
    'ARM64' { Say (T Arm64) }
    default { Die (T BadArch $arch) }
  }
  $target = 'x86_64-pc-windows-msvc'
  $asset = "nekorisu-$target.tar.gz"
  $tar = Join-Path $env:SystemRoot 'System32\tar.exe' # not a tar.exe from Git Bash, which reads C: as a host
  if (-not (Test-Path -LiteralPath $tar)) { Die (T NoTar) }

  # Downloads $name from $base into $out. $base may be https://, file: or a local folder.
  function Fetch([string]$name, [string]$out) {
    if ($base -match '^https?://') {
      [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
      try {
        Invoke-WebRequest -UseBasicParsing -Uri "$($base.TrimEnd('/'))/$name" -OutFile $out
      } catch {
        Die (T NoDownload "$($base.TrimEnd('/'))/$name" $_.Exception.Message)
      }
    } else {
      $folder = if ($base -match '^file:') { ([Uri]$base).LocalPath } else { $base }
      $from = Join-Path $folder $name
      if (-not (Test-Path -LiteralPath $from -PathType Leaf)) { Die (T NotFound $from) }
      Copy-Item -LiteralPath $from -Destination $out
    }
  }

  # Renames $path aside (a running exe can be renamed but not overwritten) and tries to delete it.
  function Set-Aside([string]$path) {
    $aside = "$path.old-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())"
    Move-Item -LiteralPath $path -Destination $aside
    Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue # still running: core removes it later
  }

  # Puts $source at $dest, keeping an identical file that is already there.
  function Place([string]$source, [string]$dest) {
    if (Test-Path -LiteralPath $dest) {
      if ((Get-FileHash -LiteralPath $source).Hash -eq (Get-FileHash -LiteralPath $dest).Hash) { return }
      Set-Aside $dest
    }
    Move-Item -LiteralPath $source -Destination $dest
  }

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("nekorisu-install-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    # ---- 1. download, check, unpack, run ----
    Say (T Downloading $asset)
    Fetch $asset (Join-Path $tmp $asset)
    Fetch 'SHA256SUMS' (Join-Path $tmp 'SHA256SUMS')
    $want = $null
    foreach ($line in Get-Content -LiteralPath (Join-Path $tmp 'SHA256SUMS')) {
      $parts = $line.Trim() -split '\s+'
      if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) { $want = $parts[0].ToLowerInvariant() }
    }
    if (-not $want) { Die (T NoSum $asset) }
    $got = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp $asset)).Hash.ToLowerInvariant()
    if ($want -ne $got) { Die (T BadSum) }

    $unpack = Join-Path $tmp 'unpack'
    New-Item -ItemType Directory -Path $unpack | Out-Null
    & $tar -xzf (Join-Path $tmp $asset) -C $unpack
    if ($LASTEXITCODE -ne 0) { Die (T NoUnpack $asset) }
    $newExe = Join-Path $unpack 'nekorisu.exe'
    $newStart = Join-Path $unpack 'nekorisu-start.exe'
    if (-not (Test-Path -LiteralPath $newExe -PathType Leaf)) { Die (T NotInArchive 'nekorisu.exe') }
    if (-not (Test-Path -LiteralPath $newStart -PathType Leaf)) { Die (T NotInArchive 'nekorisu-start.exe') }
    $printed = & { $ErrorActionPreference = 'Continue'; & $newExe --version 2>$null }
    $version = if ($LASTEXITCODE -eq 0 -and "$printed" -match '^nekorisu (\S+)$') { $Matches[1] } else { $null }
    if (-not $version) { Die (T NoRun) }

    # ---- 2. versions\<version>\ ----
    $link = Join-Path $binDir 'nekorisu.exe'
    $pointer = Join-Path $homeDir 'current'
    if ((Test-Path -LiteralPath $link) -and -not (Test-Path -LiteralPath $pointer)) {
      Die (T Foreign $link)
    }
    $dest = Join-Path $homeDir "versions\$version"
    New-Item -ItemType Directory -Force -Path $dest, $binDir | Out-Null
    $destExe = Join-Path $dest 'nekorisu.exe'
    Place $newExe $destExe
    Place $newStart (Join-Path $dest 'nekorisu-start.exe')

    # ---- 3. the link, the pointer and the launcher ----
    $fresh = "$link.nekorisu-new"
    Remove-Item -LiteralPath $fresh -Force -ErrorAction SilentlyContinue
    try {
      New-Item -ItemType HardLink -Path $fresh -Target $destExe | Out-Null
    } catch {
      Copy-Item -LiteralPath $destExe -Destination $fresh # another volume: a copy does too
    }
    $aside = $null
    if (Test-Path -LiteralPath $link) {
      $aside = "$link.old-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())"
      Move-Item -LiteralPath $link -Destination $aside
    }
    try {
      Move-Item -LiteralPath $fresh -Destination $link
      $pointerTmp = "$pointer.tmp"
      [IO.File]::WriteAllText($pointerTmp, $destExe) # UTF-8 without BOM, no line break
      Move-Item -LiteralPath $pointerTmp -Destination $pointer -Force
    } catch {
      Remove-Item -LiteralPath $fresh -Force -ErrorAction SilentlyContinue
      if ($aside) {
        Remove-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $aside -Destination $link
      }
      Die (T NoSwitch $link $_.Exception.Message)
    }
    if ($aside) { Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue } # still running: core removes it later
    $start = Join-Path $binDir 'nekorisu-start.exe'
    try {
      # A copy, since core refreshes it from the version folder later. The one here usually
      # runs core, so it is set aside rather than overwritten.
      $from = Join-Path $dest 'nekorisu-start.exe'
      if ((Test-Path -LiteralPath $start) -and (Get-FileHash -LiteralPath $from).Hash -ne (Get-FileHash -LiteralPath $start).Hash) {
        Set-Aside $start
      }
      Copy-Item -LiteralPath $from -Destination $start -Force
    } catch {
      Say (T NoLauncher $start $_.Exception.Message)
    }
    Say (T Installed $version $link $destExe)

    # ---- 4. core ----
    # A core from an earlier install keeps running the old version until it restarts.
    Quiet $link core stop
    if ($env:NEKORISU_NO_AUTOSTART -ne '1') {
      & $link core enable
      if ($LASTEXITCODE -ne 0) { Say (T NoAutostart) }
    }

    # ---- 5. PATH ----
    $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
      $raw = $envKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      $entries = @("$raw" -split ';' | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') })
      if ($entries -notcontains $binDir.TrimEnd('\')) {
        $kind = if ("$raw") { $envKey.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $joined = if ("$raw") { "$($raw.TrimEnd(';'));$binDir" } else { $binDir }
        $envKey.SetValue('Path', $joined, $kind)
        $env:Path = "$env:Path;$binDir"
        try {
          # Tell Explorer, so terminals opened from now on see the new PATH.
          Add-Type -Namespace NekorisuInstall -Name Native -MemberDefinition '[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint msg, System.UIntPtr wParam, string lParam, uint flags, uint timeout, out System.UIntPtr result);'
          $result = [UIntPtr]::Zero
          [NekorisuInstall.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result) | Out-Null
        } catch { }
        Say (T PathAdded $binDir)
      }
    } finally {
      $envKey.Close()
    }
    Say (T Next)
  } finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
}
