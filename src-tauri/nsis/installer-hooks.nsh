; DSH Desktop 安装钩子。
; 安装前扫描“应用与功能”注册表（DisplayName = "DeepSeek Harness"），
; 识别旧版安装（NSIS 与 MSI 均可），静默卸载后再安装新版，避免新旧并存。

!macro NSIS_HOOK_PREINSTALL
  Call UninstallLegacyApp
!macroend

; 执行旧版卸载：$3 = UninstallString（调用方保证非空）
Function RunLegacyUninstaller
  ${If} $3 == ""
    Return
  ${EndIf}
  ${StrCase} $5 $3 "L"
  ${StrLoc} $6 $5 "msiexec" ">"
  ${If} $6 != ""
    ; —— 旧版为 MSI 安装：提取 {GUID} 静默卸载 ——
    ${StrLoc} $7 $3 "{" ">"
    ${If} $7 == ""
      Return
    ${EndIf}
    StrCpy $8 $3 38 $7
    DetailPrint "检测到旧版 DeepSeek Harness（MSI），正在卸载 ..."
    ExecWait "msiexec.exe /x $8 /qn" $9
    ${If} $9 <> 0
      ; 静默失败常见于需要管理员权限，退回交互式卸载（会弹 UAC）
      DetailPrint "MSI 静默卸载失败（退出码 $9），尝试交互式卸载 ..."
      ExecWait "msiexec.exe /x $8" $9
    ${EndIf}
  ${Else}
    ; —— 旧版为 NSIS 安装 ——
    ; UninstallString 形如 "目录\uninstall.exe"：
    ; 去掉首引号（偏移 1）与尾部 \uninstall.exe"（15 字符）得到安装目录
    StrCpy $8 $3 -15 1
    ${If} $8 == ""
      Return
    ${EndIf}
    DetailPrint "检测到旧版 DeepSeek Harness（NSIS），正在卸载 ..."
    ; 注意：_?= 的值不能加引号——NSIS 规定它取命令行剩余部分，引号会被当作路径字符
    ; 混入 $INSTDIR，导致卸载器内所有删除操作静默失败；且必须位于命令行末尾。
    ; 正是该参数让卸载器原地同步执行，ExecWait 才能等到卸载完成。
    ExecWait '"$8\uninstall.exe" /S _?=$8' $9
    DetailPrint "旧版卸载器退出码：$9"
    ${If} $9 == 0
      ; _?= 模式下卸载器无法删除自身，手动清理残留的卸载器与空目录
      Delete "$8\uninstall.exe"
      RMDir "$8"
    ${Else}
      DetailPrint "旧版自动卸载失败（退出码 $9），保留旧版卸载入口以便手动卸载"
      ; 注意：此处不可引用 $PassiveMode —— 钩子在主脚本的 Var 声明之前被 include，
      ; 运行时变量尚未可见（会触发 unknown variable 警告并恒为空）。
      ${IfNot} ${Silent}
        MessageBox MB_ICONEXCLAMATION|MB_OKCANCEL "无法自动卸载旧版 DeepSeek Harness（退出码 $9）。$\n建议取消后从“设置 → 应用”手动卸载旧版，再重新运行本安装程序。$\n$\n是否仍继续安装？（可能与旧版并存）" /SD IDOK IDOK +2
        Abort
      ${EndIf}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function UninstallLegacyApp
  DetailPrint "正在检测旧版 DeepSeek Harness ..."

  ; —— HKCU（旧版 NSIS 默认按当前用户安装）——
  StrCpy $0 0
  legacy_hkcu_loop:
    EnumRegKey $1 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall" $0
    StrCmp $1 "" legacy_hkcu_done
    IntOp $0 $0 + 1
    ReadRegStr $2 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
    StrCmp $2 "DeepSeek Harness" 0 legacy_hkcu_loop
    ReadRegStr $3 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "UninstallString"
    Call RunLegacyUninstaller
    Goto legacy_hkcu_loop
  legacy_hkcu_done:

  ; —— HKLM 64 位视图（旧版 MSI / 按机器安装的 NSIS）——
  SetRegView 64
  StrCpy $0 0
  legacy_hklm64_loop:
    EnumRegKey $1 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall" $0
    StrCmp $1 "" legacy_hklm64_done
    IntOp $0 $0 + 1
    ReadRegStr $2 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
    StrCmp $2 "DeepSeek Harness" 0 legacy_hklm64_loop
    ReadRegStr $3 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "UninstallString"
    Call RunLegacyUninstaller
    Goto legacy_hklm64_loop
  legacy_hklm64_done:

  ; —— HKLM 32 位视图（WOW6432Node）——
  SetRegView 32
  StrCpy $0 0
  legacy_hklm32_loop:
    EnumRegKey $1 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall" $0
    StrCmp $1 "" legacy_hklm32_done
    IntOp $0 $0 + 1
    ReadRegStr $2 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
    StrCmp $2 "DeepSeek Harness" 0 legacy_hklm32_loop
    ReadRegStr $3 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\$1" "UninstallString"
    Call RunLegacyUninstaller
    Goto legacy_hklm32_loop
  legacy_hklm32_done:
  SetRegView 64

  ; 兜底：清理旧版可能遗留的开机自启项与厂商注册表键。
  ; 自启项两个值名都清理：旧卸载器按产品名清 “DeepSeek Harness”，
  ; 但 autostart.rs 实际写入的值名是 “DeepSeekHarness”（无空格），旧卸载器从未清掉它。
  ; 厂商键（含安装语言等子值）旧卸载器仅在用户勾选“删除应用数据”时才清理，静默卸载必然遗留。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeepSeek Harness"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DeepSeekHarness"
  DeleteRegKey HKCU "Software\deepseekai\DeepSeek Harness"
  DeleteRegKey /ifempty HKCU "Software\deepseekai"
FunctionEnd
