# 发布签名

Shellmate 的发布版使用一张**自签名代码签名证书**「Shellmate Code Signing」签名。它不需要 Apple 开发者账号，作用有两个：

- **钥匙串不再反复询问**：macOS 钥匙串按代码签名识别应用。临时签名（ad-hoc）每次构建都不同，每个新版本都会被当成另一个应用，读取已保存的密码时要重新点「始终允许」。固定证书签名后，只有第一次换成这张证书时会问一次。
- **应用内更新可以校验来源**：更新程序只安装用这张证书签名的新版本。即使 GitHub 上的安装包被替换，也装不进来。

它**不能**消除首次打开时「无法验证开发者」的提示，那需要付费的 Developer ID 证书和 Apple 公证。

证书的 SHA-1 记录在 `crates/shellmate/assets/signing/certificate.sha1`，编译进应用作为更新校验的依据。打包脚本和发版工作流也会核对签名是否与它一致。

## 一、生成证书（只做一次）

在仓库根目录执行：

```sh
scripts/make-signing-cert.sh
```

脚本会在 `~/Documents/shellmate-signing/` 生成三个文件，并打印证书的 SHA-1。也可以把目录作为参数传入，但不能放在仓库里。

| 文件 | 内容 | 是否保密 |
|---|---|---|
| `shellmate-signing.p12` | 证书和私钥 | **保密** |
| `password.txt` | `.p12` 的密码 | **保密** |
| `shellmate-signing.cer` | 公开证书 | 可以公开 |

脚本不会在终端输出任何保密内容。

把打印出的 SHA-1 写入 `crates/shellmate/assets/signing/certificate.sha1`（小写、一行）并提交。

## 二、备份

把 `shellmate-signing.p12` 和 `password.txt` 备份到安全的地方，例如密码管理器的附件、加密的移动硬盘。**不要**放进任何 Git 仓库或网盘的公开目录。

丢失的后果：
- 只能生成新证书，已安装的旧版本会拒绝自动更新新证书签名的版本。用户需要手动下载安装一次，钥匙串也会再询问一次。
- 换证书后要同步更新 `certificate.sha1` 和 GitHub Secrets。

证书有效期 20 年。

## 三、导入本机钥匙串（用于本机打包）

导入后，`scripts/bundle-macos.sh` 会自动使用这张证书，本机打出的包和 GitHub 发布的包签名一致：

```sh
security import ~/Documents/shellmate-signing/shellmate-signing.p12 \
  -k ~/Library/Keychains/login.keychain-db \
  -P "$(cat ~/Documents/shellmate-signing/password.txt)" \
  -T /usr/bin/codesign
```

- `-T /usr/bin/codesign` 允许 codesign 使用私钥，不必每次询问。
- 检查是否导入成功：`security find-identity -p codesigning`，应列出「Shellmate Code Signing」。自签名证书显示 `CSSMERR_TP_NOT_TRUSTED` 是正常的，不影响签名。
- 第一次用它签名时，系统可能弹窗询问是否允许 codesign 使用钥匙串，选「始终允许」。

不导入也可以，本机打包会退回临时签名。

## 四、添加 GitHub Secrets

1. 打开仓库页面 → **Settings** → 左侧 **Secrets and variables** → **Actions** → **New repository secret**。
2. 添加 `SIGNING_CERT_P12`，内容是 `.p12` 的 base64 编码。在终端执行下面的命令，结果会直接复制到剪贴板，然后粘贴到 Secret 输入框：

   ```sh
   base64 -i ~/Documents/shellmate-signing/shellmate-signing.p12 | pbcopy
   ```

3. 再添加 `SIGNING_CERT_PASSWORD`，内容是密码。同样先复制到剪贴板：

   ```sh
   pbcopy < ~/Documents/shellmate-signing/password.txt
   ```

4. 粘贴完成后清空剪贴板：`pbcopy < /dev/null`。

GitHub Secrets 添加后无法再查看，只能覆盖。发版工作流在缺少这两个 Secret，或 `certificate.sha1` 仍是占位值时会直接失败，不会发布未签名的版本。

## 五、验证

推送版本标签后，Release 工作流的「Import the signing certificate」和两个「Verify」步骤通过，就说明签名正确。也可以对下载的包检查：

```sh
codesign -dvv /Applications/Shellmate.app 2>&1 | grep Authority
# Authority=Shellmate Code Signing
```
