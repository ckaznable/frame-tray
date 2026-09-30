# frame-tray

在沒有系統匣（tray）的環境（例如 Steam Frame）中，用一個獨立視窗顯示 StatusNotifierItem / AppIndicator 圖示。

- 若 session bus 上沒有 `org.kde.StatusNotifierWatcher`，程式會自己擔任 watcher，讓應用程式可以註冊 tray 圖示。
- 若已有 watcher，則以 host 身分讀取其項目清單。
- 左鍵：`Activate`（`ItemIsMenu` 的項目則開啟選單）；右鍵：開啟 dbusmenu 選單；中鍵：`SecondaryActivate`；滾輪：`Scroll`。
- 選單直接畫在視窗內，不會被小視窗裁切。

## 建置

```sh
cargo build --release
# Steam Frame (ARM64)
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

## 安裝與自動啟動

```sh
install -Dm755 target/aarch64-unknown-linux-gnu/release/frame-tray ~/.local/bin/frame-tray
install -Dm644 frame-tray.desktop ~/.config/autostart/frame-tray.desktop
```

`frame-tray` 必須在要顯示圖示的應用程式**之前**啟動：很多應用程式只在啟動時檢查 watcher，找不到就不再建立 tray 圖示。

參數：`--on-top` 讓視窗保持最上層。環境變數 `FRAME_TRAY_ICON_THEME` 可指定優先使用的圖示主題。

## 限制

- 只支援 StatusNotifierItem（D-Bus）。舊式 XEmbed tray（Wine、部分舊 GTK2 程式）需另外執行 `xembedsniproxy`（KDE 的 `plasma-workspace` 提供）轉成 SNI。
- Wayland 下無法取得螢幕絕對座標，傳給 `Activate` 的座標僅供參考。
