param([string]$Out, [string]$Only = "")
# Live check of Clipet snippet expansion in a standard Win32 text box.
# Sends keys ONLY while this window is in the foreground; otherwise aborts.
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, UIntPtr e);
  public static void Front(IntPtr h) {
    uint fg = GetWindowThreadProcessId(GetForegroundWindow(), IntPtr.Zero);
    uint me = GetCurrentThreadId();
    AttachThreadInput(me, fg, true);
    BringWindowToTop(h); SetForegroundWindow(h);
    AttachThreadInput(me, fg, false);
  }
  public static void ClickAt(int x, int y) { SetCursorPos(x, y); mouse_event(2, 0, 0, 0, UIntPtr.Zero); mouse_event(4, 0, 0, 0, UIntPtr.Zero); }
}
"@

$form = New-Object Windows.Forms.Form
$form.Text = "Clipet snippet test"
$form.TopMost = $true
$form.Width = 700; $form.Height = 200
$tb = New-Object Windows.Forms.TextBox
$tb.Width = 640; $tb.Top = 10; $tb.Left = 10
$pw = New-Object Windows.Forms.TextBox
$pw.Width = 640; $pw.Top = 50; $pw.Left = 10; $pw.UseSystemPasswordChar = $true
$form.Controls.Add($tb); $form.Controls.Add($pw)

$cases = @(
  @{ n = "typed in order";            t = "tb"; pre = "";            k = ";name ";                               e = "Keo Kimleang" },
  @{ n = "ACCEPTANCE name,Home,;,End"; t = "tb"; pre = "";           k = "name{HOME};{END} ";                    e = "Keo Kimleang" },
  @{ n = "after other words";         t = "tb"; pre = "";            k = "hello ;name ";                         e = "hello Keo Kimleang" },
  @{ n = "typo fixed with arrows";    t = "tb"; pre = "";            k = ";nme{LEFT}{LEFT}a{END} ";              e = "Keo Kimleang" },
  @{ n = "typo fixed with backspace"; t = "tb"; pre = "";            k = ";namx{BS}e ";                          e = "Keo Kimleang" },
  @{ n = "prefix added mid-sentence"; t = "tb"; pre = "Hi name";     k = "{LEFT}{LEFT}{LEFT}{LEFT};{END} ";      e = "Hi Keo Kimleang" },
  @{ n = "unknown trigger";           t = "tb"; pre = "";            k = ";unknown ";                            e = ";unknown " },
  @{ n = "glued to a word (typed)";   t = "tb"; pre = "";            k = "abc;name ";                            e = "abc;name " },
  @{ n = "glued to unseen text";      t = "tb"; pre = "abc";         k = ";name ";                               e = "abc;name " },
  @{ n = "caret inside the trigger";  t = "tb"; pre = "";            k = ";name{LEFT}{LEFT} ";                   e = ";na me" },
  @{ n = "pasted trigger";            t = "tb"; pre = "";            k = "^v ";                                  e = "Keo Kimleang"; clip = ";name" },
  @{ n = "multiple triggers";         t = "tb"; pre = "";            k = ";x ;name ";                            e = ";x Keo Kimleang" },
  @{ n = "escape ;;name";             t = "tb"; pre = "";            k = ";;name ";                              e = ";name " },
  @{ n = "unicode before (CJK)";      t = "tb"; pre = "日本語 ";      k = ";name ";                               e = "日本語 Keo Kimleang" },
  @{ n = "unicode glued (CJK)";       t = "tb"; pre = "日本語";       k = ";name ";                               e = "日本語;name " },
  @{ n = "unicode Khmer, out of order"; t = "tb"; pre = "សួស្តី name"; k = "{LEFT}{LEFT}{LEFT}{LEFT};{END} ";    e = "សួស្តី Keo Kimleang" },
  @{ n = "undo with Backspace";       t = "tb"; pre = "";            k = ";name ";  k2 = "{BS}";                 e = ";name " },
  @{ n = "password field (typed)";    t = "pw"; pre = "";            k = ";name ";                               e = ";name " },
  @{ n = "password field (out of order)"; t = "pw"; pre = "";        k = "name{HOME};{END} ";                    e = ";name " }
)
if ($Only) { $cases = $cases | Where-Object { $_.n -like "*$Only*" } }

function Pump([int]$ms) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.ElapsedMilliseconds -lt $ms) { [Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 10 }
}

$results = New-Object Collections.Generic.List[string]
$script:lost = $false
# Sends one key at a time and stops the moment this window is not in front,
# so no keystroke can reach another app.
function Send-Guarded([string]$keys) {
  foreach ($m in [regex]::Matches($keys, '\{[^}]+\}|\^.|.')) {
    if ($script:lost -or [W]::GetForegroundWindow() -ne $form.Handle) { $script:lost = $true; return }
    [Windows.Forms.SendKeys]::SendWait($m.Value)
    Pump 40
  }
}
$form.Add_Shown({
  Pump 500
  $savedClip = $null
  try { if ([Windows.Forms.Clipboard]::ContainsText()) { $savedClip = [Windows.Forms.Clipboard]::GetText() } } catch {}
  $pass = 0; $fail = 0
  foreach ($c in $cases) {
    if ([W]::GetForegroundWindow() -ne $form.Handle) { [W]::Front($form.Handle); Pump 300 }
    if ([W]::GetForegroundWindow() -ne $form.Handle) {
      # Click this window's own title bar (it is TopMost, so nothing else is under the cursor).
      [W]::ClickAt($form.Left + 120, $form.Top + 12); Pump 400
    }
    if ([W]::GetForegroundWindow() -ne $form.Handle) { $results.Add("ABORT: test window not in front; no keys sent"); break }
    $box = if ($c.t -eq "pw") { $pw } else { $tb }
    $box.Focus() | Out-Null
    $box.Text = $c.pre
    $box.SelectionStart = $box.Text.Length
    Pump 100
    if ($c.clip) { [Windows.Forms.Clipboard]::SetText($c.clip); Pump 300 }
    Send-Guarded "{END}"   # caret moves: Clipet forgets the word
    Pump 150
    Send-Guarded $c.k
    Pump 1200
    if ($c.k2) { Send-Guarded $c.k2; Pump 1200 }
    if ($script:lost) { $results.Add("ABORT: test window lost focus during '$($c.n)'; stopped sending keys"); break }
    $got = $box.Text
    $ok = $got -ceq $c.e
    if ($ok) { $pass++ } else { $fail++ }
    $results.Add(("{0} | {1} | expected [{2}] got [{3}]" -f $(if ($ok) { "PASS" } else { "FAIL" }), $c.n, $c.e, $got))
  }
  if ($null -ne $savedClip) { try { [Windows.Forms.Clipboard]::SetText($savedClip) } catch {} }
  $results.Add("TOTAL pass=$pass fail=$fail")
  [IO.File]::WriteAllLines($Out, $results, [Text.Encoding]::UTF8)
  $form.Close()
})
[void]$form.ShowDialog()
